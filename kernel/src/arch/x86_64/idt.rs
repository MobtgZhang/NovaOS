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

use core::arch::asm;
use core::ptr::addr_of_mut;

use super::port_io;
use crate::arch::shared::fault::{FaultInfo, FaultKind};
use crate::klog;
use crate::logger::Level;

/// Number of interrupt vectors the IDT covers.
pub const IDT_ENTRIES: usize = 256;

/// Gate type for an interrupt gate: clears IF on entry, so handlers run
/// non-reentrantly.
const INTERRUPT_GATE: u8 = 0x8E;
/// Gate type for a trap gate: leaves IF set. Double fault needs this so that a
/// fault *while handling a fault* is still reportable.
const TRAP_GATE: u8 = 0x8F;

/// A 16-byte IDT gate entry.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    /// Low nibble is the IST index, high nibble is zero.
    ist_and_type: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

/// The pseudo-descriptor form used by `lidt`.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtPseudoDescriptor {
    limit: u16,
    base: u64,
}

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
    pub const fn has_error_code(self) -> bool {
        matches!(
            self,
            Exception::DivideError
                | Exception::Overflow
                | Exception::BoundRangeExceeded
                | Exception::InvalidOpcode
                | Exception::DeviceNotAvailable
                | Exception::DoubleFault
                | Exception::InvalidTss
                | Exception::SegmentNotPresent
                | Exception::StackSegmentFault
                | Exception::GeneralProtectionFault
                | Exception::PageFault
                | Exception::AlignmentCheck
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

/// The CPU-pushed exception frame.
///
/// # Safety
///
/// Field offsets are ABI. The `extern "C"` handlers in this module are the only
/// place this struct is constructed, and they match the CPU's push order exactly:
/// optional error code, `RIP`, `CS`, `RFLAGS`, then `RSP`/`SS` if the privilege
/// level changed. Reading this struct at any other offset is undefined.
#[repr(C)]
pub struct ExceptionFrame {
    /// Error code. Meaningless (and read as garbage) for vectors that push none.
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl ExceptionFrame {
    /// Whether the fault happened in ring 3.
    #[inline]
    pub fn in_user_ring(&self) -> bool {
        // CS holds a selector; its low two bits encode the privilege level.
        (self.cs & 0x3) == 3
    }
}

#[repr(C, align(16))]
struct Idt {
    entries: [IdtEntry; IDT_ENTRIES],
}

static mut IDT: Idt = Idt {
    entries: [IdtEntry {
        offset_low: 0,
        selector: 0,
        ist_and_type: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    }; IDT_ENTRIES],
};

/// Shared exception reporter.
///
/// Every CPU exception lands here via a thin shim that knows only its own vector.
/// This function's whole job is translation: x86-64's vector number, error code,
/// and register frame become an architecture-neutral [`FaultInfo`]. Formatting is
/// deliberately *not* done here — it lives in `arch::shared::fault` so that the
/// AArch64 backend emits the identical report for the equivalent condition.
///
/// # Safety
///
/// Only called from IDT stubs, which the CPU enters with a valid kernel stack.
unsafe extern "C" fn dispatch(vector: u64, frame: &ExceptionFrame) -> ! {
    let info = match Exception::from_vector(vector as u8) {
        Some(exception) => decode(exception, frame),
        // A vector outside 0..=31 is a hardware interrupt with no handler.
        None => FaultInfo {
            kind: FaultKind::UnexpectedInterrupt,
            vector,
            ip: frame.rip,
            sp: frame.rsp,
            address: None,
            from_user: frame.in_user_ring(),
            detail: 0,
        },
    };

    // SAFETY: reached only from an IDT stub, i.e. with a valid kernel stack and
    // an installed IDT. The shared reporter writes bytes with a bounded spin.
    unsafe { crate::arch::shared::fault::report(crate::arch::console(), &info) };

    // Halt rather than resume: an unhandled kernel fault means the register file
    // and stack can no longer be trusted.
    //
    // SAFETY: halting on an unhandled fault is always acceptable.
    unsafe { crate::arch::exit(crate::EXIT_PANIC) }
}

/// Translate an x86-64 exception into the architecture-neutral report.
///
/// The one place the vector-to-meaning mapping lives. Everything after this point
/// is shared, which is why adding an architecture does not mean copying the
/// reporter.
fn decode(exception: Exception, frame: &ExceptionFrame) -> FaultInfo {
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

    FaultInfo {
        kind,
        vector: exception.vector() as u64,
        ip: frame.rip,
        sp: frame.rsp,
        address,
        from_user: frame.in_user_ring(),
        // For `#PF` this carries the P/W/U/I bits, which are the first thing
        // anyone debugging a translation fault looks at.
        detail: if exception.has_error_code() {
            frame.error_code
        } else {
            0
        },
    }
}

// The stubs below are the actual IDT targets. Each one exists because the CPU's
// stack layout differs depending on whether the vector pushes an error code.

macro_rules! stub_without_error_code {
    ($fn_name:ident, $vector:expr) => {
        extern "C" fn $fn_name(frame: &ExceptionFrame) -> ! {
            // SAFETY: the CPU pushes RIP/CS/RFLAGS/RSP/SS in this order for
            // vectors that carry no error code, which is exactly `ExceptionFrame`
            // read from the current stack pointer.
            unsafe { dispatch($vector as u64, frame) }
        }
    };
}

macro_rules! stub_with_error_code {
    ($fn_name:ident, $vector:expr) => {
        extern "C" fn $fn_name(frame: &ExceptionFrame) -> ! {
            // SAFETY: the CPU pushes ERROR_CODE, RIP/CS/RFLAGS/RSP/SS in this
            // order for these vectors, which is exactly `ExceptionFrame` read
            // from the current stack pointer.
            unsafe { dispatch($vector as u64, frame) }
        }
    };
}

stub_without_error_code!(debug_stub, 1);
stub_without_error_code!(breakpoint_stub, 3);
stub_without_error_code!(coprocessor_overrun_stub, 9);
stub_without_error_code!(reserved15_stub, 15);
stub_without_error_code!(x87_stub, 16);
stub_without_error_code!(machine_check_stub, 18);
stub_without_error_code!(simd_stub, 19);
stub_without_error_code!(virtualization_stub, 20);
stub_without_error_code!(control_protection_stub, 21);
stub_without_error_code!(nmi_stub, 2);
stub_without_error_code!(device_not_available_stub, 7);

stub_with_error_code!(div_error_stub, 0);
stub_with_error_code!(overflow_stub, 4);
stub_with_error_code!(bound_range_stub, 5);
stub_with_error_code!(invalid_opcode_stub, 6);
stub_with_error_code!(double_fault_stub, 8);
stub_with_error_code!(invalid_tss_stub, 10);
stub_with_error_code!(segment_not_present_stub, 11);
stub_with_error_code!(stack_segment_fault_stub, 12);
stub_with_error_code!(general_protection_fault_stub, 13);
stub_with_error_code!(page_fault_stub, 14);
stub_with_error_code!(alignment_check_stub, 17);

/// Build a gate entry pointing at `handler`.
///
/// `ist` selects an interrupt stack table entry; 0 means "use the current stack".
///
/// `handler` is taken as `*const ()` rather than `usize` because a function
/// *item* and its address are not interchangeable: a generic or inlined item can
/// exist in several copies, and casting the item directly to an integer picks one
/// arbitrarily. Going through a pointer makes the address unambiguous, which
/// matters here because the whole entry is a raw offset split across three
/// fields.
///
/// Not `const`: casting a pointer to an integer is not permitted during const
/// evaluation, and the entries are built once at boot rather than at compile
/// time.
fn gate(ist: u8, gate_type: u8, handler: *const ()) -> IdtEntry {
    let address = handler as usize;
    IdtEntry {
        offset_low: (address & 0xFFFF) as u16,
        selector: super::gdt::KERNEL_CODE_SELECTOR,
        ist_and_type: ist | gate_type,
        offset_mid: ((address >> 16) & 0xFFFF) as u16,
        offset_high: ((address >> 32) & 0xFFFF_FFFF) as u32,
        zero: 0,
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

        // Vectors 22-31 are reserved. Leaving them zero would report an
        // uninformative "unexpected interrupt", so they share a stub that at
        // least names the vector.
        for index in 22..32 {
            entries
                .add(index)
                .write(gate(0, INTERRUPT_GATE, reserved15_stub as *const ()));
        }

        let descriptor = IdtPseudoDescriptor {
            limit: (size_of::<Idt>() - 1) as u16,
            base: (&raw const IDT) as u64,
        };

        asm!("lidt [{}]", in(reg) &descriptor, options(readonly, nostack, preserves_flags));
    }

    klog!(Level::Debug, "idt: loaded {IDT_ENTRIES} entries");
}
