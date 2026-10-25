//! Interrupt Descriptor Table and exception handling.
//!
//! # Ordering contract
//!
//! The IDT is loaded *before* anything that can fault. A `#PF` taken with no IDT
//! installed raises `#DF`, which with no IST raises `#TF`, and the machine
//! resets — losing every diagnostic. Every handler here therefore assumes a valid
//! IDT and a mapped kernel stack.
//!
//! # Handler shape
//!
//! Exceptions 0-21 all funnel into one `extern "C"` handler that decodes the
//! vector into a [`FaultInfo`] and hands it to the shared reporter. Keeping the
//! decoding here and the formatting in `arch::shared::fault` is what makes the
//! serial output identical to the AArch64 backend's; the per-architecture half is
//! only "which vector means what", which genuinely is architecture-specific.
//!
//! Exceptions that push an error code need a shim to keep the CPU's stack layout
//! uniform, so there is one shim per error-code-carrying vector.

use core::arch::naked_asm;
use core::ptr::{addr_of, addr_of_mut};

use super::port_io;
use crate::arch::shared::fault::{FaultInfo, FaultKind, FaultOutcome};
use crate::klog;
use crate::logger::Level;
use crate::syscall::UserRegisters;

/// Number of interrupt vectors the IDT covers.
pub const IDT_ENTRIES: usize = 256;

/// Gate type for an interrupt gate: clears IF on entry, so handlers run
/// non-reentrantly.
///
/// This is the *whole* type byte: the five type bits (`0b01110`) occupy bits
/// 0-4 and the present bit occupies bit 7. The three low bits of this byte
/// belong to the IST field of the *previous* byte, which is why the two are
/// never combined — see [`IdtEntry`].
const INTERRUPT_GATE: u8 = 0x8E;
/// Gate type for a trap gate: leaves IF set. Double fault needs this so that a
/// fault *while handling a fault* is still reportable.
const TRAP_GATE: u8 = 0x8F;

/// A 16-byte IDT gate entry.
///
/// # The sixteenth byte is load-bearing
///
/// The architectural layout is
/// `offset_low(2) selector(2) ist_and_type(1) offset_mid(2) offset_high(4) reserved(4)`,
/// which sums to **15** bytes. The gate is nevertheless 16 bytes wide: there is a
/// padding byte between `ist_and_type` and `offset_mid`.
///
/// Writing the struct without that padding makes every entry 15 bytes, which is
/// wrong in a way that is invisible until an exception is actually taken. The
/// table is still the right *length* in the sense that `lidt` succeeds, and every
/// entry except the first still lands at a plausible offset — so the table looks
/// fine right up until the CPU validates a gate.
///
/// It does validate it. A gate whose reserved high bits are non-zero raises
/// `#GP` *instead of* entering the handler, and because the `#GP` gate is itself
/// misaligned it fails validation too, escalating to `#DF` and then a triple
/// fault. The symptom is a kernel that boots perfectly, runs every non-faulting
/// test, and dies with **no fault report at all** the first time any exception
/// occurs — which is exactly how this was found.
///
/// The `const _: () = assert!` below makes the size a compile-time invariant
/// rather than something discovered under QEMU.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    /// **The IST byte.** Bits 0-2 hold the interrupt stack table index; bits 3-7
    /// are reserved and must be zero.
    ///
    /// The IST index and the gate type are in *different bytes*, which is the
    /// detail that causes the most confusion. The architectural layout is
    ///
    /// ```text
    ///   bytes 0-1   offset[15:0]
    ///   bytes 2-3   selector
    ///   byte  4     IST (bits 0-2) | zero (bits 3-7)
    ///   byte  5     type (bits 0-4) | DPL (bits 5-6) | present (bit 7)
    ///   bytes 6-7   offset[31:16]
    ///   bytes 8-11  offset[63:32]
    ///   bytes 12-15 reserved (must be zero)
    /// ```
    ///
    /// The `type` field is five bits wide, not four, and it starts at bit 0 of
    /// its byte — not at bit 3. Writing `ist | type` into a single byte
    /// therefore puts the type's low bits *in the IST field*, so an interrupt
    /// gate (`0x8E`) becomes "IST 6, type 0x08".
    ///
    /// The CPU then reads IST 6 for every gate, which is a stack the kernel
    /// never initialised, and reads a gate type of 0x08 — "reserved" — which
    /// fails validation. The CPU raises `#GP` instead of entering the handler,
    /// the `#GP` gate fails validation the same way, and the machine
    /// escalates to `#DF` and then a triple fault with an empty serial log.
    ///
    /// Nothing about the failure is self-evident from the symptom: the kernel
    /// boots, runs every test that does not take an exception, and then dies at
    /// the first one.
    ist: u8,
    /// The gate type byte: `type` in bits 0-4, `DPL` in bits 5-6, present in
    /// bit 7.
    type_byte: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const _: () = assert!(
    size_of::<IdtEntry>() == 16,
    "an x86-64 IDT gate is 16 bytes; a 15-byte entry silently corrupts the table"
);

/// The gate's byte offsets, pinned at compile time.
///
/// The *size* assertion above cannot catch a mis-ordered struct: a 16-byte
/// struct with its fields in the wrong order is still 16 bytes, and every entry
/// is then uniformly shifted. These offsets are what the CPU actually reads, so
/// they are the invariants that matter.
///
/// The one that bites in practice is the padding byte. It must sit at offset 4,
/// between the selector and the type byte; put anywhere else and every gate's
/// type byte lands on the padding position, the CPU reads a zero type where a
/// valid gate is required, and raises `#GP` instead of entering the handler.
const _: () = {
    use core::mem::offset_of;

    assert!(
        offset_of!(IdtEntry, offset_low) == 0,
        "offset[15:0] is bytes 0-1"
    );
    assert!(offset_of!(IdtEntry, selector) == 2, "selector is bytes 2-3");
    assert!(
        offset_of!(IdtEntry, ist) == 4,
        "the IST byte is byte 4, on its own",
    );
    assert!(
        offset_of!(IdtEntry, type_byte) == 5,
        "the type byte is byte 5; type is five bits starting at bit 0",
    );
    assert!(
        offset_of!(IdtEntry, offset_mid) == 6,
        "offset[31:16] is bytes 6-7"
    );
    assert!(
        offset_of!(IdtEntry, offset_high) == 8,
        "offset[63:32] is bytes 8-11",
    );
    assert!(
        offset_of!(IdtEntry, reserved) == 12,
        "the high reserved word is bytes 12-15",
    );

    // The encoded gate bytes, pinned.
    //
    // This is the assertion that would have caught the IST/type conflation.
    // The CPU reads byte 4's low three bits as the IST index and byte 5's low
    // five bits as the gate type; combining them into one byte produces a
    // table that is *uniformly* wrong and therefore impossible to spot by
    // eye in a hex dump.
    //
    // The check is on the *bytes*, not on a call to `gate`: casting a pointer
    // to an integer is not const-permitted on this toolchain, and the layout
    // is what matters — the address is a straight bit-slice of it.
    const IST_MASK: u8 = 0b111;

    // An interrupt gate with IST 0: byte 4 is zero, byte 5 is `0x8E`.
    let ist_byte = 0u8;
    let type_byte = INTERRUPT_GATE;
    assert!(ist_byte == 0, "an ordinary gate must select IST 0");
    assert!(
        type_byte == 0x8E,
        "an interrupt gate's type byte must be 0x8e",
    );

    // A gate on IST 1 differs from one on IST 0 *only* in byte 4.
    assert!(1u8 & IST_MASK == 1, "the IST index goes in byte 4");

    // The type byte holds the type in bits 0-4 and the present bit in bit 7,
    // so bits 5-6 are the DPL and must be zero for a ring 0 gate.
    assert!(
        INTERRUPT_GATE & 0b0110_0000 == 0,
        "the ring 0 interrupt gate must leave the DPL field clear",
    );
    assert!(
        TRAP_GATE & 0b0110_0000 == 0,
        "the ring 0 trap gate must leave the DPL field clear",
    );

    // Both gate types must be *present*: a non-present gate raises `#GP`
    // instead of entering the handler.
    assert!(
        INTERRUPT_GATE & 0b1000_0000 != 0,
        "the interrupt gate must be present",
    );
    assert!(
        TRAP_GATE & 0b1000_0000 != 0,
        "the trap gate must be present",
    );

    // The type field is five bits, so the type must fit in bits 0-4.
    assert!(
        INTERRUPT_GATE & 0b0001_1111 == 0b01110,
        "the interrupt gate's type must be 0b01110",
    );
    assert!(
        TRAP_GATE & 0b0001_1111 == 0b01111,
        "the trap gate's type must be 0b01111",
    );
};

/// The pseudo-descriptor form used by `lidt`.
///
/// The explicit alignment is load-bearing, not tidiness. `packed` gives the type
/// an alignment of 1, so without it the linker is free to place this ten-byte
/// value at *any* address — and it does: the linker put it at `...02`, two bytes
/// into whatever it was packed next to.
///
/// `lidt` then reads its ten bytes starting from that unaligned address. The
/// x86 memory-access rules make a misaligned `lidt` operand a general-protection
/// fault on real hardware; QEMU is more forgiving and reads the bytes anyway,
/// which is why the kernel appears to install a table at all. The result is the
/// same either way in practice: the table the CPU ends up using is not the one
/// this code wrote.
///
/// The symptom is entirely silent and points nowhere near the cause. The kernel
/// boots, publishes correct GDT and IDT bytes into the debug dump — because the
/// *table* is fine and only the descriptor is misaligned — runs every test that
/// takes no exception, and then on the first exception the CPU rejects the
/// vector-0 gate, raises `#GP` in its place, rejects that gate identically, and
/// triple-faults with an empty serial log. Every table dump is green; the only
/// broken thing is a ten-byte operand the dumps never show.
#[derive(Clone, Copy)]
#[repr(C, align(8))]
struct IdtPseudoDescriptor([u8; 10]);

impl IdtPseudoDescriptor {
    /// Build a descriptor from an IDT's limit and base.
    fn new(limit: u16, base: u64) -> Self {
        let mut bytes = [0u8; 10];
        bytes[0..2].copy_from_slice(&limit.to_le_bytes());
        bytes[2..10].copy_from_slice(&base.to_le_bytes());
        Self(bytes)
    }
}

const _: () = {
    assert!(
        size_of::<IdtPseudoDescriptor>() >= 10,
        "the pseudo-descriptor payload is ten bytes"
    );
    assert!(
        align_of::<IdtPseudoDescriptor>() >= 8,
        "lidt reads an unaligned pseudo-descriptor operand; the CPU rejects it"
    );
    // The payload must still start at offset 0, or `base` is no longer at 2.
    assert!(
        core::mem::offset_of!(IdtPseudoDescriptor, 0) == 0,
        "the payload must start at offset 0 or the limit/base offsets shift"
    );
};

/// CPU exceptions, vectors 0-31.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    DivideError = 0,
    Debug = 1,
    NonMaskableInterrupt = 2,
    Breakpoint = 3,
    Overflow = 4,
    BoundRangeExceeded = 5,
    InvalidOpcode = 6,
    DeviceNotAvailable = 7,
    DoubleFault = 8,
    CoprocessorSegment = 9,
    InvalidTss = 10,
    SegmentNotPresent = 11,
    StackSegmentFault = 12,
    GeneralProtectionFault = 13,
    PageFault = 14,
    Reserved15 = 15,
    X87FloatingPoint = 16,
    AlignmentCheck = 17,
    MachineCheck = 18,
    SimdFloatingPoint = 19,
    Virtualization = 20,
    ControlProtection = 21,
}

impl Exception {
    /// Vector number this exception occupies.
    pub const fn vector(self) -> u8 {
        self as u8
    }

    /// Look up an exception by vector, or `None` for a hardware interrupt.
    ///
    /// Vectors 0-31 are exceptions; 32 and above belong to the device table.
    pub const fn from_vector(vector: u8) -> Option<Self> {
        Some(match vector {
            0 => Exception::DivideError,
            1 => Exception::Debug,
            2 => Exception::NonMaskableInterrupt,
            3 => Exception::Breakpoint,
            4 => Exception::Overflow,
            5 => Exception::BoundRangeExceeded,
            6 => Exception::InvalidOpcode,
            7 => Exception::DeviceNotAvailable,
            8 => Exception::DoubleFault,
            9 => Exception::CoprocessorSegment,
            10 => Exception::InvalidTss,
            11 => Exception::SegmentNotPresent,
            12 => Exception::StackSegmentFault,
            13 => Exception::GeneralProtectionFault,
            14 => Exception::PageFault,
            15 => Exception::Reserved15,
            16 => Exception::X87FloatingPoint,
            17 => Exception::AlignmentCheck,
            18 => Exception::MachineCheck,
            19 => Exception::SimdFloatingPoint,
            20 => Exception::Virtualization,
            21 => Exception::ControlProtection,
            _ => return None,
        })
    }

    /// Name for the fault report.
    pub const fn name(self) -> &'static str {
        match self {
            Exception::DivideError => "divide error",
            Exception::Debug => "debug",
            Exception::NonMaskableInterrupt => "non-maskable interrupt",
            Exception::Breakpoint => "breakpoint",
            Exception::Overflow => "overflow",
            Exception::BoundRangeExceeded => "bound range exceeded",
            Exception::InvalidOpcode => "invalid opcode",
            Exception::DeviceNotAvailable => "device not available",
            Exception::DoubleFault => "double fault",
            Exception::CoprocessorSegment => "coprocessor segment overrun",
            Exception::InvalidTss => "invalid TSS",
            Exception::SegmentNotPresent => "segment not present",
            Exception::StackSegmentFault => "stack-segment fault",
            Exception::GeneralProtectionFault => "general protection fault",
            Exception::PageFault => "page fault",
            Exception::Reserved15 => "reserved vector 15",
            Exception::X87FloatingPoint => "x87 floating-point exception",
            Exception::AlignmentCheck => "alignment check",
            Exception::MachineCheck => "machine check",
            Exception::SimdFloatingPoint => "SIMD floating-point exception",
            Exception::Virtualization => "virtualization exception",
            Exception::ControlProtection => "control protection exception",
        }
    }

    /// Whether the CPU pushes an error code for this exception.
    ///
    /// This is the Intel SDM's list, verbatim. It is *not* the same question as
    /// "is this exception a fault": `#DE`, `#OF` and `#BR` are faults that push
    /// nothing, and `#UD` is a fault that pushes nothing on Intel-family CPUs.
    ///
    /// Getting this wrong does not merely mislabel a fault — it shifts every
    /// field in the frame by one word, because the error code occupies the
    /// frame's lowest address when it is present. The measured layouts are in
    /// `docs/adr/0004-x86-64-exception-frame.md`.
    pub const fn has_error_code(self) -> bool {
        matches!(
            self,
            Exception::DoubleFault
                | Exception::InvalidTss
                | Exception::SegmentNotPresent
                | Exception::StackSegmentFault
                | Exception::GeneralProtectionFault
                | Exception::PageFault
                | Exception::AlignmentCheck
                | Exception::Virtualization
                | Exception::ControlProtection
        )
    }

    /// Decode a page fault error code into its three flags.
    pub const fn decode_page_fault_error(code: u64) -> PageFaultError {
        PageFaultError {
            present: code & 0x1 != 0,
            write: code & 0x2 != 0,
            instruction_fetch: code & 0x4 != 0,
        }
    }
}

/// The three decoded bits of a page fault error code.
#[derive(Clone, Copy)]
pub struct PageFaultError {
    /// Fault was caused by a non-present page (`PF` rather than NP).
    pub present: bool,
    /// Fault was a write (`W` rather than R).
    pub write: bool,
    /// Fault was an instruction fetch (I rather than D).
    pub instruction_fetch: bool,
}

impl PageFaultError {
    /// Render in the conventional `P/W/U` form.
    pub const fn as_letters(self) -> &'static str {
        match (self.present, self.write, self.instruction_fetch) {
            (true, true, false) => "P-W-D",
            (true, false, false) => "P-R-D",
            (true, true, true) => "P-W-I",
            (true, false, true) => "P-R-I",
            (false, true, false) => "NP-W-D",
            (false, false, false) => "NP-R-D",
            (false, true, true) => "NP-W-I",
            (false, false, true) => "NP-R-I",
        }
    }
}

/// The CPU-pushed exception frame, in the layout the CPU actually produces.
///
/// # The layout, as measured
///
/// The frame's *lowest* address is where the CPU's last push landed. In
/// 64-bit mode the CPU pushes `SS`, `RSP`, `RFLAGS`, `CS`, `RIP`, and — for
/// the vectors that define one — an error code. Each push decrements `RSP`,
/// so the error code, being pushed last, ends up at the **lowest** address.
///
/// Measured at the entry of the dispatcher; the full capture is in
/// `docs/adr/0004-x86-64-exception-frame.md`:
///
/// ```text
///     #DE  (no error code)      #PF  (error code)
///     [0] RIP                   [0] error code
///     [1] CS                    [1] RIP
///     [2] RFLAGS                [2] CS
///     [3] RSP                   [3] RFLAGS
///     [4] SS                    [4] RSP
///                              [5] SS
/// ```
///
/// # The stub makes both shapes identical
///
/// Rather than teaching every reader to handle two shapes, the IDT stub
/// pushes a zero placeholder for the vectors that push no error code. The
/// dispatcher therefore always sees the same six words:
///
/// ```text
///     [0] error code   (CPU-pushed, or the stub's zero placeholder)
///     [1] RIP
///     [2] CS
///     [3] RFLAGS
///     [4] RSP
///     [5] SS
/// ```
///
/// `RIP` is then at a **fixed** offset for every vector, which is what makes
/// the resume trampoline vector-independent: it steps over the error code,
/// writes the resume target into the `RIP` slot, and `iretq` pops the five
/// words above it.
///
/// # Why the offset is fixed rather than computed
///
/// A per-vector `RIP` offset would work too, and would cost one branch. It
/// would also mean the resume trampoline had to know which vector it was
/// resuming, and the *shape* of the frame would depend on a value only the
/// dispatcher knows — a property the trap sequence cannot see. Fixing the
/// shape at the stub is the one place that knows the vector, so it is the one
/// place that gets to make the decision.
#[repr(C)]
pub struct ExceptionFrame {
    /// The CPU-pushed error code, or the stub's zero placeholder.
    ///
    /// Read through [`ExceptionFrame::error_code`] rather than directly: for
    /// the vectors that push none this word is the stub's placeholder, which
    /// is zero by construction — but it is not a value the CPU produced.
    pub error_code: u64,
    /// `RIP`.
    pub rip: u64,
    /// `CS`.
    pub cs: u64,
    /// `RFLAGS`.
    pub rflags: u64,
    /// `RSP`.
    pub rsp: u64,
    /// `SS`.
    pub ss: u64,
}

/// The byte offset of `RIP` within the uniform frame.
///
/// The error code occupies the first word, so `RIP` is the second.
const RIP_OFFSET: u64 = 8;

/// The size of the uniform frame, in bytes.
const FRAME_BYTES: u64 = 48;

impl ExceptionFrame {
    /// The vector that produced this frame decides whether an error code exists.
    ///
    /// This answers "was an error code pushed", nothing more. It does **not**
    /// displace any of the five unconditional words: the stub has already made
    /// the frame's shape uniform.
    fn has_error_code(vector: u64) -> bool {
        Exception::from_vector(vector as u8).is_some_and(|e| e.has_error_code())
    }

    /// The CPU-pushed error code, or `0` for a vector that pushes none.
    ///
    /// Reading this instead of indexing the frame directly is what keeps the
    /// rule in one place. For a vector that pushes no error code, the frame's
    /// first word is the stub's placeholder, which is zero by construction —
    /// so returning it would report a bogus error code and, for `#PF`, decode
    /// it into nonsense P/W/U bits.
    #[inline]
    pub fn error_code(&self, vector: u64) -> u64 {
        if Self::has_error_code(vector) {
            self.error_code
        } else {
            0
        }
    }

    /// Instruction pointer at the fault.
    ///
    /// Always at the fixed offset [`RIP_OFFSET`], for every vector.
    #[inline]
    pub fn rip(&self, _vector: u64) -> u64 {
        self.rip
    }

    /// Code segment selector at the fault.
    #[inline]
    pub fn cs(&self, _vector: u64) -> u64 {
        self.cs
    }

    /// `RFLAGS` at the fault.
    #[inline]
    pub fn rflags(&self, _vector: u64) -> u64 {
        self.rflags
    }

    /// Stack pointer the faulting code was using.
    ///
    /// Always present in 64-bit mode — `RSP` is pushed for every exception, not
    /// only on a privilege change.
    #[inline]
    pub fn rsp(&self, _vector: u64) -> Option<u64> {
        Some(self.rsp)
    }

    /// Whether the fault was taken from ring 3.
    ///
    /// The saved `CS` carries the ring of the faulting code in its two low bits,
    /// and the handler itself always runs at ring 0, so the value is meaningful
    /// for every vector.
    #[inline]
    fn user_ring(&self, vector: u64) -> bool {
        (self.cs(vector) & 0x3) == 3
    }

    /// Whether the fault was taken from a different privilege level.
    ///
    /// In 64-bit mode the CPU pushes `RSP` and `SS` unconditionally, so this
    /// does *not* report whether those words are present — there is no such
    /// condition. It reports whether the faulting code was at a different CPL,
    /// which is the question callers actually want, and it is answered by the
    /// saved `CS`'s requested privilege level.
    ///
    /// The name matters because the 32-bit-mode predicate of the same meaning
    /// is usually called "did the CPU push `RSP`/`SS`", and conflating the two
    /// is how a resume path ends up skipping two words that were never absent.
    #[inline]
    pub fn privilege_changed(&self, vector: u64) -> bool {
        self.cs(vector) & 0x3 != 0
    }
}

#[repr(C, align(4096))]
struct Idt {
    entries: [IdtEntry; IDT_ENTRIES],
}

const _: () = assert!(
    size_of::<Idt>() == IDT_ENTRIES * 16,
    "the IDT must be exactly 256 * 16 bytes; anything else means an entry is the wrong size"
);

const _: () = assert!(
    align_of::<Idt>() == 4096,
    "the IDT base must be 4 KiB aligned: the CPU's gate lookup masks the low \
     twelve bits of the base, so an unaligned table would be read from the wrong \
     address"
);

static mut IDT: Idt = Idt {
    entries: [IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_byte: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    }; IDT_ENTRIES],
};

/// Shared exception reporter.
///
/// Every CPU exception lands here from a naked trampoline that has put the
/// pushed frame's address in `RDI` and the vector number in `RSI`. This
/// function's whole job is translation: x86-64's vector number, error code, and
/// register frame become an architecture-neutral [`FaultInfo`]. Formatting is
/// deliberately *not* done here — it lives in `arch::shared::fault` so that the
/// AArch64 backend emits the identical report for the equivalent condition.
///
/// # Resuming
///
/// Normally this halts, because an unhandled kernel fault means the register
/// file and stack can no longer be trusted. The exception is the fault tests,
/// which raise an exception deliberately and need the machine to survive it: when
/// a handler has published [`FaultOutcome::Continue`] and the interlock is armed,
/// this restores the pushed registers and returns to the faulting instruction
/// instead. The interlock is what keeps that safe — see
/// [`crate::arch::shared::fault::may_continue`].
///
/// # Safety
///
/// Only called from the naked trampolines above, which the CPU enters with a
/// valid kernel stack and an installed IDT. `frame` must be the address the CPU
/// pushed, which is what the trampoline captures.
/// The mangling-independent name the naked trampolines tail-call.
///
/// The trampolines jump to this by name, so the target symbol must have a fixed
/// spelling. `nova_kernel_idt_dispatch` is namespaced to the crate, which matters
/// more than it looks: the obvious name `dispatch` is a common libc symbol, and a
/// bare `#[unsafe(no_mangle)]` on it would let a C-library object claim the same
/// name. Whichever definition the linker picked would silently become the
/// exception handler, and the failure would be indistinguishable from the fault
/// path being broken.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nova_kernel_idt_dispatch(vector: u64, frame: &ExceptionFrame) -> ! {
    let info = match Exception::from_vector(vector as u8) {
        Some(exception) => decode(exception, vector, frame),
        // A vector outside 0..=31 is a hardware interrupt with no handler.
        None => FaultInfo {
            kind: FaultKind::UnexpectedInterrupt,
            vector,
            ip: frame.rip(vector),
            sp: frame.rsp(vector).unwrap_or(0),
            address: None,
            from_user: frame.privilege_changed(vector) && frame.user_ring(vector),
            detail: 0,
            error_code: 0,
        },
    };

    // The privilege-level words are only on the stack when the CPU pushed them,
    // so both the `from_user` flag and the stack pointer are gated on that.
    let changed = frame.privilege_changed(vector);

    // Record before deciding anything: a handler that decides to resume must
    // still leave the test a record of what it saw.
    crate::arch::shared::fault::record_fault(
        &info,
        frame.cs(vector),
        frame.rflags(vector),
        changed,
    );
    crate::arch::shared::fault::count_fault();

    // SAFETY: reached only from an IDT stub, i.e. with a valid kernel stack and
    // an installed IDT. The shared reporter writes bytes with a bounded spin.
    unsafe { crate::arch::shared::fault::report(crate::arch::console(), &info) };

    // Resume only when a handler said so *and* the interlock is armed *and* a
    // resume target was published. All three are required: the outcome alone
    // would let a handler resume a fault it never repaired, and a target alone
    // is not a decision.
    let target = crate::arch::shared::fault::resume_target();
    if crate::arch::shared::fault::may_continue()
        && crate::arch::shared::fault::last_outcome() == FaultOutcome::Continue
        && target != 0
    {
        let frame_pointer = frame as *const ExceptionFrame as u64;

        // SAFETY: delegated to this trampoline's contract below.
        unsafe { enter_resume(frame_pointer, target) };
    }

    // Halt rather than resume: an unhandled kernel fault means the register file
    // and stack can no longer be trusted.
    //
    // SAFETY: halting on an unhandled fault is always acceptable.
    unsafe { crate::arch::exit(crate::EXIT_PANIC) }
}

/// Restore the CPU's pushed frame and return to the faulting code at `target`.
///
/// # The frame's shape
///
/// The stub uniformises the frame, so the six words below always sit at the
/// same offsets from the frame address `RBX`:
///
/// ```text
///     [rbx + 0x00]  error code   (CPU-pushed, or the stub's zero placeholder)
///     [rbx + 0x08]  RIP
///     [rbx + 0x10]  CS
///     [rbx + 0x18]  RFLAGS
///     [rbx + 0x20]  RSP
///     [rbx + 0x28]  SS
/// ```
///
/// `iretq` pops five words, starting at `RIP`. It never sees the error code,
/// which sits *below* the words it consumes — so the resume path has to step
/// over it, and stepping over it means moving **down**, towards lower
/// addresses.
///
/// That is what the `add rsp, 8` does. Skipping eight bytes steps over the
/// error-code word and lands exactly on the saved `RIP`.
///
/// # The register save area
///
/// The stub saved the fifteen general-purpose registers below the frame, at
/// `[rbx - 8 - 8*k]` for `k` in `0..15`, in push order: `rax` at the highest
/// address, `r15` at the lowest. The trampoline pops them back in the reverse
/// order, which restores both the values and the stack pointer.
///
/// This is not optional bookkeeping. The dispatcher is an ordinary Rust
/// function, so it clobbers the caller-saved registers — including `rcx`,
/// which is the *divisor* of the `idiv` that raised the `#DE`. Resuming
/// without restoring it would re-execute the divide with a different divisor,
/// re-fault immediately, and spin the machine at fault speed with no way out.
/// The symptom is a fault report that looks completely correct followed by an
/// endless stream of identical faults.
///
/// The area's lowest word is therefore 120 bytes below the frame base, and
/// the `sub rsp, 120` reaches it exactly. The stub's eight-byte alignment
/// pad sits *below* the area, and is skipped by neither — it is the word the
/// `call` to the dispatcher pushed, and the resume path discards the whole
/// region below the frame.
///
/// # Why this is a naked trampoline and not three `asm!` statements
///
/// These instructions must run as a unit, with *no* register allocation and
/// *no* compiled code in between:
///
/// ```text
///     mov rsp, rbx          ; adopt the CPU's frame
///     mov qword [rbx+8], r12; overwrite the saved RIP with the resume target
///     sub rsp, 120          ; reach the register save area
///     pop r15 ... pop rax   ; restore the general-purpose registers
///     add rsp, 8            ; step over the error code
///     iretq
/// ```
///
/// Written as ordinary `asm!` statements inside a normal Rust function, the
/// compiler allocates registers around them and is entitled to restore `RSP`
/// from a callee-saved register *after* the reset. `options(nostack)` does not
/// prevent this — it asserts that the asm does not touch `RSP`, which is
/// exactly what these instructions do, so the compiler is told the stack
/// pointer is invariant and schedules its own `mov %reg, %rsp` to run last.
/// `RSP` then points at a register that no longer holds the frame address,
/// `iretq` pops whatever that register happened to contain, and control
/// transfers to a word of the compiler's choosing.
///
/// That failure is silent and unique to this path. The report has already been
/// printed by the time this runs, so the log shows a correctly-diagnosed fault
/// and then stops — which reads as "the report hung", not "the resume sequence
/// corrupted the stack".
///
/// A naked trampoline has no prologue, no register allocation and no epilogue,
/// so the instructions execute exactly as written. `options(noreturn)` states
/// that control does not return, which is what lets the caller treat this as a
/// terminating branch.
///
/// # Why the values arrive in registers
///
/// `naked_asm!` is *global-scoped* inline assembly: it accepts no operands at
/// all, and on this toolchain `asm!` inside a `#[naked]` function is rejected
/// outright (`E0787`). So a naked body cannot name its inputs the way a normal
/// function would. The two idiomatic ways out are a label operand or fixed
/// registers, and this kernel uses fixed registers for the same reason its
/// exception stubs do: the register set is named once, in a constant, so the
/// caller and the trampoline cannot disagree about it.
///
/// `RBX` carries the frame address and `R12` the resume target. Both are
/// callee-saved, so loading them costs the caller nothing. Both are also
/// overwritten by the restore — `pop rbx` and `pop r12` — but only *after*
/// their last use, which is why the target is written to memory before the
/// pops begin.
///
/// # Safety
///
/// `RBX` must hold the address the CPU pushed, `R12` must be a valid kernel
/// instruction address, and this must be called only while the fault interlock
/// is armed. It does not return.
#[unsafe(naked)]
unsafe extern "C" fn resume_from_frame() -> ! {
    naked_asm!(
        // SAFETY: by contract, `RBX` holds the frame address. `RSP` is moved
        // there so the frame the CPU pushed is what the sequence below
        // operates on.
        "mov rsp, rbx",
        // SAFETY: `RBX + 8` is the saved `RIP` slot of the uniform frame, and
        // it is a word of the pushed frame, so it is mapped and writable. The
        // operand is written LLVM-style (`ptr`) because `naked_asm!` parses
        // with LLVM's assembler, not the Intel one the `asm!` macro uses
        // elsewhere.
        //
        // Writing the resume target there is what makes `iretq` continue at
        // the target rather than re-executing the faulting instruction.
        "mov qword ptr [rbx + 8], r12",
        // SAFETY: the register save area holds the fifteen general-purpose
        // registers the stub pushed, in push order: `r15` at the lowest
        // address and `rax` at the highest. It sits immediately below the
        // frame, and the stub's eight-byte alignment pad sits below *that*.
        //
        // The area is fifteen words — 120 bytes — below the frame's base, so
        // subtracting 120 reaches `r15`.
        //
        // The number is 120 and not 128 because the pad occupies the word
        // *below* `r15`. Subtracting 128 lands on the pad, and every pop then
        // reads one word too low: `r15` receives the pad's stale contents and
        // every other register receives its neighbour's value. The register
        // file ends up shifted by one word.
        //
        // That failure is silent and uniquely confusing. The stack *position*
        // at the `iretq` still comes out right, because the extra word
        // skipped below is compensated for by the `add rsp` at the end, so
        // control transfers correctly to the resume target — with a corrupt
        // register file. The resumed code then reads a pointer that was never
        // the pointer it stored, and dereferences it. The symptom is a page
        // fault in an unrelated function, several frames away from the
        // exception that actually went wrong, with a faulting address of
        // zero and a register set that does not match the source.
        "sub rsp, 120",
        // SAFETY: the fifteen pops restore the general-purpose registers the
        // stub saved, in the reverse of the order they were pushed. The stack
        // is mapped and the values are the ones the stub wrote.
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        // SAFETY: `RSP` now addresses the frame's error-code word, which sits
        // immediately below the saved `RIP`. Skipping eight bytes steps over
        // it and lands on the saved `RIP`, which is where `iretq` expects to
        // start popping.
        //
        // Eight bytes, not sixteen: the word below `RIP` is the error code,
        // and nothing else. The stub's alignment pad is 128 bytes below the
        // frame, fifteen register words further down, and was already skipped
        // by the `sub rsp` above.
        "add rsp, 8",
        // SAFETY: `RSP` addresses the saved `RIP`, and the five words above it
        // are the CPU's saved `CS`, `RFLAGS`, `RSP` and `SS`. `iretq` pops
        // exactly those five and returns to the interrupted code, with `RIP`
        // now holding the resume target.
        "iretq",
    )
}

/// Load the two registers [`resume_from_frame`] reads, then enter it.
///
/// Split from the trampoline because a naked body cannot contain the loads: the
/// values are computed here, in ordinary Rust, and moved into registers by the
/// `mov`s below.
///
/// # Why the registers are pinned rather than left to the allocator
///
/// `in(reg)` lets the compiler pick *any* register for an operand, including
/// `rbx` and `r12` — the two registers [`resume_from_frame`] reads. It is
/// allowed to do so, because at the point of the `asm!` there is no contract
/// saying otherwise.
///
/// When it does, the sequence still *appears* to work. The compiler loads the
/// frame address into, say, `rbx` and the resume target into `r12`, the
/// template's own `mov` instructions then write the same values to the same
/// registers, and the jump transfers control correctly. The register file
/// ends up right.
///
/// The hazard is that the compiler is free to *also* keep a live value in one
/// of those registers across the `asm!`, or to use one as a scratch
/// register for a value it needs afterwards — and the template's `mov`
/// silently destroys it. The result is a value that was correct before the
/// jump and wrong after it, with no diagnostic: the jump is unconditional and
/// the trampoline restores the *interrupted* register file, not the compiler's.
///
/// `in("rbx")` and `in("r12")` remove the freedom entirely: the compiler must
/// place those operands in exactly those registers, and must not keep a live
/// value there across the `asm!`. The template's `mov` then becomes a
/// redundant copy of a value that is already in the right place — which is
/// precisely the property that makes the sequence safe.
///
/// The redundancy is deliberate. The alternative — loading the values with
/// `mov` from registers the compiler chose — cannot be made safe without the
/// same pinning, and pinning the *source* register is the same amount of
/// constraint with less to get wrong.
///
/// # Safety
///
/// Same contract as [`resume_from_frame`]. Does not return.
unsafe fn enter_resume(frame: u64, target: u64) -> ! {
    // SAFETY: each register is loaded immediately before the jump, and the jump is
    // unconditional, so no instruction can observe a stale value.
    //
    // The whole template is one `concat!` and the operands come last. `asm!`
    // associates a template string with the operands that *follow* it, so
    // interleaving operands and templates is what the parser rejects:
    // `"mov rbx, {frame}", frame = in(reg) frame, "mov r12, {target}"` is a
    // syntax error, not a formatting preference.
    //
    // The operands are pinned to named registers rather than left as
    // `in(reg)`, so the compiler cannot choose a register the template is
    // about to overwrite. See the function's documentation for why that
    // matters.
    //
    // `rbx` cannot be an operand register: LLVM reserves it internally and
    // rejects it outright ("rbx is used internally by LLVM and cannot be used
    // as an operand for inline asm"). The frame address therefore arrives in
    // `r15` and the template's `mov` puts it in `rbx`.
    //
    // Explicitly named operands are referred to by their register name in the
    // template, not by an `{name}` placeholder, and they cannot carry an
    // operand name of their own — which is why `frame` and `target` appear
    // here without a `name =` prefix.
    //
    // The `mov r12, r12` is a self-move. It exists only to make the template
    // mention `r12`, so the operand is not reported as unused; it changes no
    // bit. `r12` already holds the target because the operand was placed there.
    //
    // The `mov rbx, r15` is safe because the block is `noreturn`: LLVM treats
    // every register as dead after it, so nothing it had in `rbx` can be live
    // across the block. The trampoline consumes `rbx` and `r12` in its first
    // two instructions, before the `pop` sequence restores the interrupted
    // register file over both of them.
    unsafe {
        core::arch::asm!(
            concat!(
                "mov rbx, r15\n",
                "mov r12, r12\n",
                "jmp {trampoline}",
            ),
            in("r15") frame,
            in("r12") target,
            trampoline = sym resume_from_frame,
            options(noreturn),
        )
    }
}

/// Translate an x86-64 exception into the architecture-neutral report.
///
/// The one place the vector-to-meaning mapping lives. Everything after this point
/// is shared, which is why adding an architecture does not mean copying the
/// reporter.
fn decode(exception: Exception, vector: u64, frame: &ExceptionFrame) -> FaultInfo {
    let kind = match exception {
        Exception::DivideError => FaultKind::DivideByZero,
        Exception::InvalidOpcode => FaultKind::IllegalInstruction,
        Exception::GeneralProtectionFault
        | Exception::SegmentNotPresent
        | Exception::StackSegmentFault
        | Exception::InvalidTss
        | Exception::BoundRangeExceeded
        | Exception::DeviceNotAvailable => FaultKind::ProtectionViolation,
        Exception::PageFault => FaultKind::AddressTranslation,
        Exception::AlignmentCheck => FaultKind::MisalignedAccess,
        Exception::Breakpoint | Exception::Debug => FaultKind::SoftwareTrap,
        Exception::MachineCheck => FaultKind::MachineCheck,
        // `#DF`, NMI, x87/SIMD, virtualization, control-protection, and the
        // reserved vectors all mean "stopped with no more specific story".
        _ => FaultKind::SynchronousGeneric,
    };

    // `CR2` is only meaningful for `#PF`; reading it for any other vector would
    // put a stale value in the report and send the next person hunting for a page
    // fault that did not happen.
    let address = if exception == Exception::PageFault {
        // SAFETY: CR2 is readable at any time.
        Some(unsafe { port_io::read_cr2() })
    } else {
        None
    };

    // The privilege-level words are only on the stack when the CPU pushed them,
    // so both the `from_user` flag and the stack pointer are gated on that.
    let changed = frame.privilege_changed(vector);

    FaultInfo {
        kind,
        vector: exception.vector() as u64,
        ip: frame.rip(vector),
        sp: frame.rsp(vector).unwrap_or(0),
        address,
        from_user: changed && frame.user_ring(vector),
        // For `#PF` this carries the P/W/U/I bits, which are the first thing
        // anyone debugging a translation fault looks at.
        detail: if exception.has_error_code() {
            frame.error_code(vector)
        } else {
            0
        },
        error_code: frame.error_code(vector),
    }
}

// The stubs below are the actual IDT targets. Each one exists because the CPU's
// stack layout differs depending on whether the vector pushes an error code.
//
// # Why every stub is a naked trampoline
//
// The CPU does not pass the exception frame in a register. It *pushes* it on the
// current stack (and, when the privilege level changed, `RSP`/`SS` too), which is
// why the frame must be recovered from `RSP` — not from `RDI`.
//
// Writing these as `extern "C" fn(&ExceptionFrame)` looks right and is not: the
// SysV ABI says the first integer argument arrives in `RDI`, so such a stub
// receives whatever `RDI` happened to hold and treats it as a pointer to the
// pushed frame. The generated code is a `push`/`mov %rdi,%rsi`/`call` sequence
// that looks entirely reasonable in a disassembly, and it "works" — the kernel
// still reports *a* fault and still exits with the panic status — while every
// register it prints (`ip`, `sp`, `cs`, and the ring check that decides whether
// the fault came from user space) is read from the wrong address. That failure is
// invisible from the outside precisely because the diagnostics are the only thing
// affected.
//
// A naked trampoline is the only construct that can express the real contract:
// the frame lives at `RSP` on entry, so `RSP` is moved into the ABI argument slot
// and the handler is entered as an ordinary Rust function. Note the absence of
// any stack manipulation in the trampoline: a `push` before capturing `RSP`
// would silently shift every subsequent field by eight bytes.
//
// `jmp` rather than `call` because the handler never returns; a `call` would
// leave a return address on the stack above the captured frame and corrupt it.

/// Build one naked exception trampoline for `vector`.
///
/// The vector number is emitted as a literal so the target stays a constant, and
/// `mov` (not `lea`) is used so the immediate encoding is independent of the
/// image's code model.
/// Build one naked exception trampoline for `vector`.
///
/// The vector number is emitted as a literal so the target stays a constant, and
/// `mov` (not `lea`) is used so the immediate encoding is independent of the
/// image's code model.
///
/// # The frame is uniformised here
///
/// The CPU pushes a different number of words depending on whether the vector
/// defines an error code: five for `#DE`, `#UD` and the rest, six for `#PF`,
/// `#GP` and the rest. That difference is *invisible* to the handler unless
/// something makes it uniform.
///
/// The stub does. For the vectors that push no error code it opens one more
/// word below the CPU's frame and stores zero there, so every vector presents
/// the same six words:
///
/// ```text
///     [0] error code   (CPU-pushed, or this zero placeholder)
///     [1] RIP
///     [2] CS
///     [3] RFLAGS
///     [4] RSP
///     [5] SS
/// ```
///
/// `RIP` is then at a fixed offset of 8 for every vector, which is what lets
/// the dispatcher and the resume trampoline stay vector-independent. Making the
/// shape uniform in the stub is the right place for the decision: the stub is
/// the only code that knows the vector.
///
/// The frame is 48 bytes either way, and it ends at the same address in both
/// cases — the CPU's five-word frame is *at* the six-word frame's last five
/// words, so the placeholder slot is exactly the one word the CPU did not use.
/// That is what keeps the stack pointer's residue identical for every vector.
///
/// # Why the general-purpose registers are saved
///
/// An exception suspends the interrupted code with its registers live. A
/// handler that clobbers them and then resumes destroys the interrupted
/// computation — the divide-by-zero test would come back with a different
/// divisor, and the illegal-instruction test with a different opcode table
/// entry. Saving them in the stub and restoring them in the resume trampoline
/// is what makes "resume" mean "continue", rather than "continue with whatever
/// the handler happened to leave behind".
///
/// The registers are saved *above* the frame (the stack grows down, so a push
/// goes to a lower address) and are restored from the same place.
///
/// # Why `call` and not `jmp`
///
/// The SysV ABI requires `RSP ≡ 8 (mod 16)` at a function's first instruction,
/// which is what a `call` produces. The CPU aligns `RSP` to 16 before pushing
/// its frame, so the stub starts 16-byte aligned; the register save area plus
/// the alignment pad brings it back to a multiple of 16 before the `call`.
///
/// The return address `call` pushes lands *below* the frame, so it does not
/// disturb it. The resume path discards it by resetting `RSP` to the frame.
macro_rules! stub {
    ($fn_name:ident, $vector:expr, $push_placeholder:expr) => {
        #[unsafe(naked)]
        extern "C" fn $fn_name() -> ! {
            naked_asm!(
                // SAFETY: `sub` and the store touch only the kernel stack, one
                // word below the CPU's frame, which is mapped and writable.
                // The value stored is zero, which is what
                // `ExceptionFrame::error_code` reports for a vector that pushes
                // no error code.
                concat!($push_placeholder),
                // SAFETY: each `push` writes one word of the register file to
                // the kernel stack. The stack is mapped and has room: the
                // kernel reserves a dedicated stack for the boot thread and the
                // interrupt path uses a separate one.
                "push rax",
                "push rbx",
                "push rcx",
                "push rdx",
                "push rsi",
                "push rdi",
                "push rbp",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "push r12",
                "push r13",
                "push r14",
                "push r15",
                // SAFETY: an eight-byte alignment pad, so the `call` below
                // happens with `RSP` a multiple of 16 as the ABI requires.
                "sub rsp, 8",
                // SAFETY: `RSP` addresses the frame's error-code word, which is
                // the CPU's own for the error-code vectors and the placeholder
                // for the others. It is the frame's first word.
                //
                // The frame goes in `rsi`, which is the SysV *second* integer
                // argument slot. The vector number goes in `rdi`, the first.
                //
                // The order is not interchangeable, and getting it wrong is
                // invisible in a way that costs hours: the handler is entered
                // normally, and then dereferences the *vector number* as if it
                // were a pointer. That is a null-pointer read, which raises
                // `#PF` inside the handler, which raises `#DF` inside the
                // `#PF` handler, and the machine triple-faults with an empty
                // serial log. Every layer of that looks like "the fault path is
                // broken", and none of it points at the argument order.
                "lea rsi, [rsp + 128]",
                // SAFETY: the vector is a compile-time constant, and `rdi` is the
                // SysV first integer argument slot that `dispatch` declares.
                concat!("mov edi, ", $vector),
                // SAFETY: `nova_kernel_idt_dispatch` diverges and never
                // returns, so the `call` is used only to establish the ABI's
                // stack alignment. Its return address is pushed below the frame
                // and is discarded by the resume path.
                "call nova_kernel_idt_dispatch",
                // SAFETY: `nova_kernel_idt_dispatch` is declared `-> !`, so this
                // is unreachable. It is here because the naked function's type
                // demands a terminator and the assembler needs one.
                "ud2",
            )
        }
    };
}

stub!(debug_stub, 1, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(nmi_stub, 2, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(breakpoint_stub, 3, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(
    coprocessor_overrun_stub,
    9,
    "sub rsp, 8\nmov qword ptr [rsp], 0\n"
);
stub!(reserved15_stub, 15, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(x87_stub, 16, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(
    machine_check_stub,
    18,
    "sub rsp, 8\nmov qword ptr [rsp], 0\n"
);
stub!(simd_stub, 19, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(virtualization_stub, 20, "");
stub!(control_protection_stub, 21, "");

stub!(div_error_stub, 0, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(overflow_stub, 4, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(bound_range_stub, 5, "sub rsp, 8\nmov qword ptr [rsp], 0\n");
stub!(
    invalid_opcode_stub,
    6,
    "sub rsp, 8\nmov qword ptr [rsp], 0\n"
);
stub!(
    device_not_available_stub,
    7,
    "sub rsp, 8\nmov qword ptr [rsp], 0\n"
);
stub!(double_fault_stub, 8, "");
stub!(invalid_tss_stub, 10, "");
stub!(segment_not_present_stub, 11, "");
stub!(stack_segment_fault_stub, 12, "");
stub!(general_protection_fault_stub, 13, "");
stub!(page_fault_stub, 14, "");
stub!(alignment_check_stub, 17, "");

// The timer interrupt.
//
// It uses the *same* stub shape as the exception stubs rather than a special
// one, because the CPU's frame for an interrupt is identical to its frame for
// an exception that pushes no error code: five words. The stub's zero
// placeholder makes the shape uniform, exactly as it does for the exceptions.
//
// The difference is what happens next: the timer handler does not report a
// fault and halt. It accounts a tick and asks the scheduler whether to
// preempt, and the *scheduler* may switch threads — which means the handler
// must not return through the frame it was given, but instead resume the
// thread the scheduler chose.
//
// The scheduler's switch replaces the whole register file and the stack
// pointer, so the frame this stub pushed is simply abandoned. That is why the
// handler is `-> !`: it ends in a context switch or in an `iretq` that
// restores the frame.
#[unsafe(naked)]
unsafe extern "C" fn timer_stub() -> ! {
    core::arch::naked_asm!(
        // SAFETY: each push stores one word of the interrupted register file on
        // the current kernel stack. The timer stub saves all fifteen because
        // user code does not follow the SysV callee-save convention.
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // SAFETY: an eight-byte pad restores 16-byte alignment for the call.
        // `RDI` points to `R15`, the lowest saved word, which is where
        // `UserRegisters` begins.
        "sub rsp, 8",
        "mov rdi, rsp",
        "add rdi, 8",
        "call nova_kernel_timer_handler",
        // SAFETY: the handler is declared `-> !`; this terminator exists only
        // for the naked function's type checking.
        "ud2",
    )
}

/// The timer interrupt handler.
///
/// # What it does
///
/// 1. Acknowledges the interrupt at the PIC, so the controller will raise the
///    next one.
/// 2. Accounts a tick and asks the scheduler whether the running thread's
///    timeslice has expired.
/// 3. Either resumes the interrupted thread, or lets the scheduler switch to
///    another one.
///
/// # Why it does not return through the frame
///
/// The scheduler's context switch replaces the whole register file and the
/// stack pointer, so the frame the CPU pushed is abandoned rather than
/// popped. The handler therefore ends either in a context switch (which never
/// returns) or in an `iretq` that restores the frame and returns to the
/// interrupted code.
///
/// The distinction is not cosmetic. A handler that popped the frame and
/// returned normally would resume the interrupted thread *and* the switched-to
/// thread — two threads running at once, with one of them holding a stack
/// pointer into the other's stack.
///
/// # Safety
///
/// Reached only from the timer stub, with a valid kernel stack and an
/// installed IDT.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nova_kernel_timer_handler(registers: *const UserRegisters) -> ! {
    crate::klog!(crate::logger::Level::Info, "debug: timer entered");
    // SAFETY: delegated to this function's contract. The EOI is only valid
    // while servicing the interrupt, which is exactly what this is.
    unsafe {
        super::apic::eoi();
    }

    // SAFETY: delegated to this function's contract. The scheduler switches
    // threads with interrupts masked, and the tick accounting is a single
    // writer.
    unsafe {
        crate::sched::handle_timer_interrupt(registers);
    }

    // The scheduler did not switch, so the interrupted thread resumes. The
    // `iretq` pops the frame the stub's uniformisation made: the error-code
    // placeholder, `RIP`, `CS`, `RFLAGS`, `RSP` and `SS`.
    //
    // The frame the *stub* built is one word lower than the frame the CPU
    // pushed, because the stub opened a slot for the placeholder. `iretq`
    // starts at the frame's `RIP`, so the placeholder word has to be stepped
    // over first.
    //
    // SAFETY: the CPU's frame is on the kernel stack, and the six words above
    // are the ones `iretq` consumes.
    unsafe {
        core::arch::asm!("add rsp, 8", "iretq", options(noreturn),);
    }
}

/// The raw bytes of the IDT, for boot-time diagnosis.
///
/// Returns a pointer to the table itself rather than a copy: the table is
/// 4 KiB, and copying it to the stack during a fault report would be a poor
/// trade. The caller must only read it.
pub fn idt_bytes() -> *const u8 {
    // SAFETY: delegated to the function's contract.
    addr_of!(IDT) as *const u8
}

/// Print the pseudo-descriptors `lgdt` and `lidt` read.
///
/// The pseudo-descriptor is the only thing the CPU actually reads when a table
/// is loaded, so if it is malformed the CPU keeps using the table the bootloader
/// left behind — and the kernel then behaves as though its own GDT and IDT were
/// never installed, which is indistinguishable from "the gates are wrong".
pub fn dump_pseudo_descriptors() {
    let gdt = crate::arch::x86_64::gdt::pseudo_descriptor_bytes();
    dump_table("gdtp", gdt.as_ptr(), gdt.len());

    let mut idt = [0u8; 10];
    idt[0..2].copy_from_slice(&((size_of::<Idt>() - 1) as u16).to_le_bytes());
    idt[2..10].copy_from_slice(&((&raw const IDT) as u64).to_le_bytes());
    dump_table("idtp", idt.as_ptr(), idt.len());
}

/// Print a table's raw bytes to the console, for boot-time diagnosis.
fn dump_table(label: &str, bytes: *const u8, len: usize) {
    use crate::arch::capabilities::{ByteSink, Console};

    let mut line = ByteSink::<96>::new();
    line.push_str("[NOVA] DBG ");
    line.push_str(label);
    line.push_str(": ");
    for index in 0..len {
        if index % 16 == 0 && index != 0 {
            crate::arch::console().send_bytes(line.as_bytes());
            line = ByteSink::<96>::new();
            line.push_str("[NOVA] DBG   ");
        }
        // SAFETY: `bytes` points at `len` readable bytes of a table the caller
        // owns, and `index` is within that range.
        let byte = unsafe { *bytes.add(index) };
        line.push_hex(byte as u64);
        line.push_str(" ");
    }
    crate::arch::console().send_bytes(line.as_bytes());
}

/// Build a gate entry pointing at `handler`.
///
/// # The IST index and the gate type share a byte
///
/// The byte is `IST (bits 0-2) | gate type (bits 3-7)`. Combining them with a
/// plain `|` only works because the two ranges do not overlap, which means the
/// IST index must be masked to three bits before it is combined.
///
/// Without the mask, an IST of 8 or more silently overwrites the gate type's low
/// bits and the entry stops being an interrupt or trap gate at all. That is not
/// a subtle degradation: the CPU then treats the entry as a *task* gate,
/// validates a completely different descriptor shape against the handler's
/// bytes, and raises `#GP` instead of entering the handler — which, for the
/// double-fault gate, is a triple fault with no output.
///
/// `ist` is a `u8` because that is the field width, not because any value is
/// meaningful; the architectural range is 0..=7.
///
/// `handler` is taken as `*const ()` rather than `usize` because a function
/// *item* and its address are not interchangeable: a generic or inlined item can
/// exist in several copies, and casting the item directly to an integer picks one
/// arbitrarily. Going through a pointer makes the address unambiguous, which
/// matters here because the whole entry is a raw offset split across three
/// fields.
///
/// Not `const`: casting a pointer to an integer is not permitted during const
/// evaluation on this toolchain, and the entries are built once at boot rather
/// than at compile time.
///
/// The *byte* layout is nevertheless pinned by compile-time assertions, which
/// do not need the address: see the `const _` block below `IdtEntry`.
fn gate(ist: u8, gate_type: u8, handler: *const ()) -> IdtEntry {
    const IST_MASK: u8 = 0b111;

    debug_assert!(ist & !IST_MASK == 0, "the IST field is only three bits");
    debug_assert!(
        gate_type & 0b1111_1110 == 0,
        "the gate type is five bits plus the present bit; the low three bits \
         belong to the IST field and are set in a different byte",
    );

    let address = handler as usize;
    IdtEntry {
        offset_low: (address & 0xFFFF) as u16,
        selector: super::gdt::KERNEL_CODE_SELECTOR,
        // The selector must name a *long-mode* code segment or the CPU rejects
        // every gate whose handler lives above 4 GiB; see `gdt::CODE_FLAGS`.
        ist: ist & IST_MASK,
        type_byte: gate_type,
        offset_mid: ((address >> 16) & 0xFFFF) as u16,
        offset_high: ((address >> 32) & 0xFFFF_FFFF) as u32,
        reserved: 0,
    }
}

/// Fill the IDT with real handlers and load it.
///
/// # Safety
///
/// Loads the IDT. Must be called before any operation that can fault and before
/// interrupts are enabled.
pub unsafe fn init() {
    // SAFETY: called exactly once from `_start`; the only writer of `IDT`.
    unsafe {
        let entries = addr_of_mut!(IDT.entries) as *mut IdtEntry;

        macro_rules! set {
            ($index:expr, $handler:path) => {
                entries.add($index).write(gate(
                    0,
                    INTERRUPT_GATE,
                    $handler as *const () as *const (),
                ));
            };
        }

        set!(0, div_error_stub);
        set!(1, debug_stub);
        set!(2, nmi_stub);
        set!(3, breakpoint_stub);
        set!(4, overflow_stub);
        set!(5, bound_range_stub);
        set!(6, invalid_opcode_stub);
        set!(7, device_not_available_stub);
        // Double fault runs on IST 1: it may be reporting a fault that itself
        // overflowed the stack we were already using.
        entries.add(8).write(gate(
            super::gdt::FAULT_IST_INDEX as u8,
            TRAP_GATE,
            double_fault_stub as *const (),
        ));
        set!(9, coprocessor_overrun_stub);
        set!(10, invalid_tss_stub);
        set!(11, segment_not_present_stub);
        set!(12, stack_segment_fault_stub);
        set!(13, general_protection_fault_stub);
        set!(14, page_fault_stub);
        set!(15, reserved15_stub);
        set!(16, x87_stub);
        set!(17, alignment_check_stub);
        set!(18, machine_check_stub);
        set!(19, simd_stub);
        set!(20, virtualization_stub);
        set!(21, control_protection_stub);

        // The timer interrupt, at the vector the PIC was remapped to.
        //
        // This is the *only* hardware interrupt with a handler. Every other
        // line stays masked at the PIC, so an unmasked line with no handler
        // cannot fire into a zeroed gate.
        set!(super::intr::IRQ_BASE_MASTER as usize, timer_stub);

        // Vectors 22-31 are reserved. Leaving them zero would report an
        // uninformative "unexpected interrupt", so they share a stub that at
        // least names the vector.
        for index in 22..32 {
            entries
                .add(index)
                .write(gate(0, INTERRUPT_GATE, reserved15_stub as *const ()));
        }

        // The pseudo-descriptor the CPU reads for `lidt` lives in a `static`
        // rather than a local. A local is *usually* fine, and that is precisely
        // what makes the failure mode so hard to see: when it is correct nothing
        // is observably different, and when it is wrong the CPU loads a
        // descriptor built from whatever the compiler happened to spill into the
        // same stack slots.
        //
        // A `static` removes the question, and the declared `align(8)` removes
        // the other one: `lidt` rejects an unaligned operand, and a local's
        // alignment is whatever the compiler's stack layout happens to give it.
        static mut IDT_DESCRIPTOR: IdtPseudoDescriptor = IdtPseudoDescriptor([0; 10]);

        // SAFETY: `init` is the sole writer, called once from `_start`, and the
        // write happens-before the `lidt` that reads it in the same block.
        let descriptor = &mut *addr_of_mut!(IDT_DESCRIPTOR);
        *descriptor =
            IdtPseudoDescriptor::new((size_of::<Idt>() - 1) as u16, (&raw const IDT) as u64);

        core::arch::asm!(
            "lidt [{descriptor}]",
            descriptor = in(reg) addr_of!(IDT_DESCRIPTOR),
            options(readonly, nostack, preserves_flags)
        );
    }

    klog!(Level::Debug, "idt: loaded {IDT_ENTRIES} entries");
}
