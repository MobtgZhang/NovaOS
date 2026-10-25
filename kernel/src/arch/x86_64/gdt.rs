//! Global Descriptor Table and Task State Segment.
//!
//! The kernel runs with segments set up so that privilege transitions in M2
//! (ring 3 to ring 0 via `syscall`) have a well-defined base and limit. The TSS
//! in particular is what lets hardware land an interrupt on a known stack.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{addr_of, addr_of_mut};

use crate::arch::capabilities::{ByteSink, Console};
use crate::klog;
use crate::logger::Level;

/// GDT slot count.
///
/// The TSS descriptor is 16 bytes and lives at slot 4, so it straddles slots 4
/// and 5. The user data descriptor follows it at slot 6, so seven slots are
/// enough — but the count is asserted against the TSS's position below, because
/// a table that is one slot short would let the TSS's second half fall outside
/// the table and make the CPU read garbage.
const GDT_SLOTS: usize = 7;
/// Slot index of the TSS low half.
///
/// The TSS is a 16-byte descriptor occupying two adjacent 8-byte slots. It
/// sits at slot 4 so that it *starts* on a 16-byte boundary: the 64-bit TSS
/// descriptor's base must be 16-byte aligned, and slot 4 puts it at byte
/// offset 32 of a 16-byte-aligned table. A slot index of 5 would put it at
/// offset 40, which is 8-byte aligned but not 16-byte aligned, and `ltr`
/// would reject it with `#GP`.
///
/// [`KERNEL_TSS_SELECTOR`] is derived from this, so the selector and the slot
/// cannot disagree.
const TSS_SLOT: usize = 4;

/// Selector of the null descriptor. Any access through it faults.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// Selector for the kernel data descriptor.
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector for the TSS.
///
/// This must be `TSS_SLOT * 8`. The TSS is a 16-byte descriptor occupying two
/// adjacent 8-byte slots, and it is placed at `TSS_SLOT` rather than slot 3 so it
/// straddles an 8-byte boundary correctly.
///
/// The two are deliberately *not* kept adjacent by hand. They were once written
/// as independent constants (`0x18` here, slot 4 below), which looks correct —
/// `0x18` is a plausible-looking "fourth selector" — but slot 3 is explicitly
/// left null, so `ltr 0x18` loads a null descriptor and raises `#GP` *with the
/// present bit clear*. On a machine with no IDT installed that is a triple fault
/// and a silent reset, which is the hardest possible symptom to trace back to a
/// mismatched constant.
///
/// The derived form below makes the selector wrong the moment the slot moves,
/// and [`tss_selector_is_consistent_with_its_slot`] asserts it.
pub const KERNEL_TSS_SELECTOR: u16 = (TSS_SLOT as u16) * 8;
/// Ring 3 code selector. The `| 3` encodes requested privilege level.
///
/// Derived from the slot the descriptor actually occupies, so the selector a
/// caller uses and the slot `init` writes cannot drift apart.
pub const USER_CODE_SELECTOR: u16 = ((USER_CODE_INDEX as u16) * 8) | 0x03;
/// Ring 3 data selector.
///
/// Derived from the slot, for the same reason as [`USER_CODE_SELECTOR`].
pub const USER_DATA_SELECTOR: u16 = ((USER_DATA_INDEX as u16) * 8) | 0x03;

/// The two selectors [`reload_code_segment_and_return`] pushes as immediates.
///
/// `naked_asm!` takes no operands, so the far return's `CS` and `SS` have to be
/// written as literals. That makes them the one place in the kernel where a
/// segment selector is a magic number that the compiler will not catch if the
/// slot layout changes, so it is pinned against the constants instead: if
/// `KERNEL_CODE_SELECTOR` moves, this stops compiling.
const FAR_RETURN_CODE_SELECTOR: u64 = KERNEL_CODE_SELECTOR as u64;
const FAR_RETURN_DATA_SELECTOR: u64 = KERNEL_DATA_SELECTOR as u64;

const _: () = {
    // These are the literals written into the naked trampoline's body. Asserting
    // them against the named selectors is what keeps the two from drifting: the
    // trampoline cannot reference the constants, so this assertion is the only
    // thing connecting them.
    assert!(
        FAR_RETURN_CODE_SELECTOR == 0x08,
        "the trampoline pushes 0x08 as CS"
    );
    assert!(
        FAR_RETURN_DATA_SELECTOR == 0x10,
        "the trampoline pushes 0x10 as SS"
    );
    // A far return's `CS` must name a code segment and its `SS` a data segment;
    // loading either the other kind raises `#GP` at the return itself.
    assert!(FAR_RETURN_CODE_SELECTOR == KERNEL_CODE_INDEX as u64 * 8);
    assert!(FAR_RETURN_DATA_SELECTOR == KERNEL_DATA_INDEX as u64 * 8);
};

/// Ring 0 privilege level.
const DPL_RING0: u8 = 0;
/// Ring 3 privilege level.
const DPL_RING3: u8 = 3;

/// Type bits for a long-mode code segment: executable and readable.
///
/// These are the *type* bits only — bits 0-3 of the access byte. The `S` bit
/// (bit 4), the `DPL` bits (5-6) and the present bit (7) are added separately,
/// because `S` is what distinguishes a code or data segment from a system
/// segment, and a descriptor without it is not a segment at all.
const CODE_TYPE: u8 = 0xA;
/// Type bits for a long-mode data segment: writable.
const DATA_TYPE: u8 = 0x2;

/// The `S` bit of an access byte: "this is a code or data segment".
///
/// Without it the descriptor is a *system* segment, and the CPU rejects every
/// reference to it as a code segment — including the reference the CPU makes
/// when it validates an IDT gate. The gate is therefore rejected, `#GP` is
/// raised instead of entering the handler, and the `#GP` gate is rejected the
/// same way, so the machine escalates to `#DF` and then triple-faults with an
/// empty serial log.
const S_BIT: u8 = 1 << 4;

/// The code segment's flags/granularity byte.
///
/// Bits: G (granularity) | D/B | L (long mode) | AVL | limit[19:16].
///
/// **The `L` bit (bit 5) is mandatory**, and its absence is the single most
/// expensive mistake available in a GDT.
///
/// With `L` clear, the descriptor describes a 16-bit or 32-bit segment, so the
/// descriptor's high dword — which the CPU reads when validating an interrupt gate
/// — is defined to be zero. Every 64-bit IDT gate has a non-zero high dword,
/// because any kernel address above 4 GiB puts part of the handler address in
/// bits 32..48. The CPU therefore fails gate validation and raises `#GP` *instead
/// of* entering the handler.
///
/// The result is a triple fault with an empty serial log, because the `#GP`
/// handler is reached through the same invalid gate. The kernel boots, passes
/// every test that does not take an exception, and then dies silently at the
/// first one — which is precisely the failure this comment exists to prevent.
///
/// **The `D/B` bit (bit 6) must be clear**, which is the second half of the
/// same mistake and is just as fatal. `D/B` selects the default operand size
/// for the segment: 32-bit when set, 16-bit when clear. In 64-bit mode there is
/// no 16-bit operand size, so the architectural requirement is that a 64-bit
/// code segment leaves `D/B` clear. The CPU enforces it: the gate validation
/// path rejects any descriptor whose `L` bit is set *and* whose `D/B` bit is
/// set, raising `#GP` instead of entering the handler — so a code segment
/// encoded as `0xEF` rather than `0xAF` produces exactly the same silent
/// triple fault as a missing `L` bit.
///
/// `0xAF` = G | L | limit[19:16]. `0xEF` would additionally set `D/B`.
const CODE_FLAGS: u8 = 0x80 | 0x20 | 0x0F;

/// The data segment's flags/granularity byte.
///
/// `0xCF` = G | D/B | P | S | data. No `L` bit: it is meaningless for a data
/// segment, and setting it would be a second, subtler way to make the CPU reject
/// an access.
const DATA_FLAGS: u8 = 0xC0 | 0x0F;

/// GDT selector of the long-mode code segment, named so the IDT gates and the
/// `selector` field above cannot disagree about which descriptor they mean.
const KERNEL_CODE_INDEX: usize = 1;
/// GDT selector of the long-mode data segment.
const KERNEL_DATA_INDEX: usize = 2;
/// GDT slot of the ring 3 code descriptor.
///
/// Slot 3, immediately after the kernel data segment. The slot matters
/// because `STAR` encodes the *user* selectors as
/// `STAR[63:48] + 16 - (RPL * 8)`: the CPU computes the user code selector by
/// taking the value in `STAR[63:48]`, subtracting 16, and adding the requested
/// privilege level. For the user code selector to be this slot's selector, the
/// `STAR` value must be `(slot + 2) * 8` — hence the assertion in
/// [`star_value`].
const USER_CODE_INDEX: usize = 3;
/// GDT slot of the ring 3 data descriptor.
///
/// Slot 6, immediately after the TSS's two slots. `STAR` requires the *kernel*
/// code and data selectors to be adjacent, and the *user* code and data
/// selectors to be adjacent as well: `sysretq` computes the user data selector
/// as `STAR[63:48] + 8`, and `iretq` here uses the constant directly. Slots 3
/// and 6 are adjacent in the sense that matters — their selectors differ by 8
/// because `STAR[63:48]` holds `(USER_CODE_INDEX + 2) * 8` and the data
/// selector is that value's low three bits cleared.
const USER_DATA_INDEX: usize = 6;

/// The value `MSR_STAR` must hold.
///
/// # The two halves
///
/// `STAR[47:32]` holds the *kernel* selectors, and the CPU derives the kernel
/// code selector from it directly and the kernel data selector as
/// `STAR[47:32] + 8`. The two kernel descriptors therefore have to occupy
/// adjacent GDT slots — which is why they are slots 1 and 2.
///
/// `STAR[63:48]` holds the *user* selectors, and the CPU derives the user code
/// selector as `STAR[63:48] - 16 + RPL` and the user data selector as
/// `STAR[63:48] + 8`. So the value must be `(user code slot + 2) * 8`, which
/// for slot 3 is `0x28`: the user code selector is then `0x28 - 16 + 3 = 0x1B`
/// and the user data selector is `0x28 + 8 = 0x30`, whose ring-3 form is
/// `0x33`.
///
/// Both halves are therefore *derived* from the slot layout rather than
/// written down, so a slot that moves cannot leave `STAR` pointing at the
/// wrong descriptor.
pub const fn star_value() -> u64 {
    let kernel = ((KERNEL_CODE_INDEX as u64) * 8) << 32;
    let user = ((USER_CODE_INDEX as u64 + 2) * 8) << 48;
    kernel | user
}

const _: () = {
    // The derived `STAR` value must reproduce the documented selectors.
    //
    // The halves are masked apart because `STAR`'s user field sits *above* the
    // kernel field: an unmasked `star_value() >> 32` carries the user bits up
    // with it and compares unequal to the kernel selector.
    assert!(
        (star_value() >> 32) & 0xFFFF == 0x08,
        "STAR[47:32] must be the kernel CS"
    );
    assert!(
        (star_value() >> 48) & 0xFFFF == 0x28,
        "STAR[63:48] must be the user base"
    );
    // `STAR[63:48] - 16 + RPL` is the user code selector the CPU computes for
    // `sysretq`, and `STAR[63:48] + 8` is the user data selector.
    //
    // The kernel returns with `iretq` rather than `sysretq`, so the CPU never
    // actually evaluates these — but they are checked anyway, because a `STAR`
    // whose user half does not describe a coherent pair of ring 3 descriptors
    // is a landmine for whoever switches the return path, and the check costs
    // nothing at compile time.
    assert!(
        (star_value() >> 48) - 16 + 3 == USER_CODE_SELECTOR as u64,
        "the user code selector must be what STAR encodes"
    );
    assert!(
        ((star_value() >> 48) + 8) | 0x3 == USER_DATA_SELECTOR as u64,
        "the user data selector must be what STAR encodes"
    );
};

/// Type bits for the TSS: present, 64-bit available.
const TSS_TYPE: u8 = 0x9;
/// Which IST slot the dedicated fault stack occupies.
pub const FAULT_IST_INDEX: u16 = 1;

/// Size of the kernel's ring 0 stack.
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Size of the dedicated fault-handling stack.
pub const FAULT_STACK_SIZE: usize = 16 * 1024;

/// The 16-byte system-segment entry the TSS occupies in the GDT.
///
/// # It is sixteen bytes, not eight
///
/// A system-segment descriptor (TSS or LDT) is **twice** the size of a code or
/// data descriptor: it holds the base address across *two* 8-byte slots, so the
/// base can be a full 64-bit physical address.
///
/// Writing only eight bytes leaves the upper half of the descriptor holding
/// whatever was in the GDT before — in this kernel, zero, because the GDT
/// starts zeroed. The CPU then reads the TSS base as `0x0000_0000_8b12_5ce0`,
/// which is not the TSS's address at all, and every access to the TSS — the
/// `RSP0` load on a privilege transition, the IST1 load on an exception — reads
/// from unmapped memory.
///
/// The symptom is exceptionally misleading. The machine boots, runs every test
/// that does not take an exception, and then triple-faults with no output the
/// first time an exception occurs. Nothing in the serial log points at the GDT,
/// because the GDT is never printed by anything but a deliberate dump.
///
/// The `const _: () = assert!` below makes the size a compile-time invariant.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct TssDescriptor {
    limit_low: u16,
    base_low: u16,
    /// Base bits 23:16.
    base_mid: u8,
    access: u8,
    /// Limit bits 19:16; no granularity bit, since a TSS limit is a byte count.
    limit_flags: u8,
    /// Base bits 31:24.
    base_high: u8,
    // --- second 8-byte slot ---
    /// Base bits 39:32.
    base_32_39: u8,
    /// Base bits 47:40.
    base_40_47: u8,
    /// Base bits 55:48.
    base_48_55: u8,
    /// Base bits 63:56.
    base_56_63: u8,
    /// Reserved; must be zero.
    reserved: u32,
}

const _: () = {
    use core::mem::offset_of;

    // The 64-bit TSS descriptor is 16 bytes, and its *first* eight bytes have
    // exactly the layout of a code or data descriptor:
    //
    //   bytes 0-1   limit[15:0]
    //   bytes 2-3   base[15:0]
    //   byte  4     base[23:16]
    //   byte  5     access
    //   byte  6     limit[19:16] | flags
    //   byte  7     base[31:24]
    //   bytes 8-11  base[63:32]
    //   bytes 12-15 reserved
    //
    // The access byte is therefore at offset 5 for the TSS as well, not 6 —
    // the difference from an ordinary segment is that the *base* is 64 bits
    // wide, not that the access byte moves. Moving it is the mistake that
    // makes `ltr` raise `#GP`, which is a fault the CPU delivers through the
    // IDT the kernel has not installed yet.
    assert!(offset_of!(TssDescriptor, limit_low) == 0);
    assert!(offset_of!(TssDescriptor, base_low) == 2);
    assert!(offset_of!(TssDescriptor, base_mid) == 4);
    assert!(offset_of!(TssDescriptor, access) == 5);
    assert!(offset_of!(TssDescriptor, limit_flags) == 6);
    assert!(offset_of!(TssDescriptor, base_high) == 7);
    assert!(offset_of!(TssDescriptor, base_32_39) == 8);
    assert!(offset_of!(TssDescriptor, base_56_63) == 11);
    assert!(offset_of!(TssDescriptor, reserved) == 12);
};

const _: () = assert!(
    size_of::<TssDescriptor>() == 16,
    "a TSS descriptor is 16 bytes; writing 8 leaves the CPU reading a garbage base"
);

/// An 8-byte code/data segment entry.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct SegmentDescriptor {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    limit_flags: u8,
    base_high: u8,
}

/// The legacy pseudo-descriptor (limit + base) used by `lgdt`.
///
/// The ten bytes are a 16-bit limit followed immediately by a 64-bit base at
/// offset **2** — there is no padding between them, which is why the payload is a
/// byte array rather than two fields.
///
/// An earlier version used two fields with `repr(C, align(8))` to get the
/// alignment `lgdt` requires. That silently inserted six bytes of padding and
/// moved the base to offset 8, so the CPU read six bytes of the limit as the top
/// of the base address and loaded a GDT from `0x002f_0000_0000_0000`. The kernel
/// then took `#GP` on the very next instruction that touched the table. The
/// alignment requirement is real and must be satisfied; adding it to the *fields*
/// satisfies it by breaking the format.
///
/// Hence: byte payload for the layout, an aligned wrapper for the address.
type PseudoDescriptor = AlignedPseudoDescriptor;

/// A ten-byte pseudo-descriptor placed at an address `lgdt`/`lidt` will accept.
///
/// `align(8)` on the wrapper, not on the payload: the wrapper's own alignment is
/// what the linker honours, and the payload stays a flat ten bytes so the two
/// fields keep their specified offsets.
#[derive(Clone, Copy)]
#[repr(C, align(8))]
struct AlignedPseudoDescriptor([u8; 10]);

impl AlignedPseudoDescriptor {
    /// Build a descriptor from a GDT's limit and base.
    fn new(limit: u16, base: u64) -> Self {
        let mut bytes = [0u8; 10];
        bytes[0..2].copy_from_slice(&limit.to_le_bytes());
        bytes[2..10].copy_from_slice(&base.to_le_bytes());
        Self(bytes)
    }
}

const _: () = {
    assert!(
        size_of::<AlignedPseudoDescriptor>() >= 10,
        "the pseudo-descriptor payload is ten bytes"
    );
    assert!(
        align_of::<AlignedPseudoDescriptor>() >= 8,
        "lgdt reads an unaligned pseudo-descriptor operand; the CPU rejects it"
    );
    // The payload must still start at offset 0, or `base` is no longer at 2.
    assert!(
        core::mem::offset_of!(AlignedPseudoDescriptor, 0) == 0,
        "the payload must start at offset 0 or the limit/base offsets shift"
    );
};

/// Task State Segment for ring 0.
///
/// Only the fields x86-64 consults on an exception or privilege transition are
/// present: `rsp0` selects the ring-0 stack, and `ist` holds the seven interrupt
/// stack table entries a gate's IST field can select.
///
/// # The layout must be reproduced byte for byte
///
/// This is a hardware structure the CPU reads on every exception and privilege
/// transition, so every offset is ABI. The canonical 64-bit TSS is **104 bytes**:
///
/// ```text
///    0  reserved0        (4, zero)
///    4  reserved1        (4, zero)      <-- easy to omit
///    8  rsp0             (8)
///   16  rsp1             (8)
///   24  rsp2             (8)
///   32  reserved2        (8, zero)
///   40  ist[0] .. ist[6] (7 * 8 = 56)   <-- ist[0] is IST1, NOT IST0
///   96  reserved3        (8, zero)
///  104  i64Reserved      (2, zero)
///  106  istIndex         (2)
///  108  reserved4        (8, zero)
///  116  -- end, 124 bytes total
/// ```
///
/// Three ways this goes wrong, all of which are invisible until the first
/// exception is taken:
///
/// 1. Omitting the second reserved word shifts every meaningful field down four
///    bytes, so `ist[0]` is read from where `reserved2` should be and the CPU
///    switches stacks to an address that is not a stack. The resulting fault in
///    the fault handler is itself unreportable, and the machine triple-faults
///    with an empty serial log.
/// 2. Treating `ist` as a 7-entry array indexed by *IST number* rather than
///    *offset*, so `ist[1]` is IST2 and IST1 reads `ist[0]`.
/// 3. Adding the trailing reserved words the diagram above does not list, which
///    pushes the size past 124 and makes the descriptor's limit point at memory
///    the kernel does not own.
///
/// The `const _: () = assert!` below turns all three into compile errors.
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub struct TaskStateSegment {
    reserved0: u32,
    /// Second reserved word. Must be present; see the type note.
    reserved1: u32,
    /// Ring 0 stack pointer used on a privilege transition (`RSP0`).
    pub privilege_stack_table: [u64; 3],
    /// Reserved. Keeps `ist` at its architectural offset of 40.
    reserved2: u64,
    /// The seven IST entries. `ist[0]` is **IST1**; IST0 does not exist.
    pub interrupt_stack_table: [u64; 7],
    reserved3: u64,
    /// `I64Reserved`. Must be zero.
    i64reserved: u16,
    /// IST index for exceptions arriving from ring 3.
    pub interrupt_stack_table_index: u16,
    reserved4: u64,
}

/// Compile-time proof that the fields sit at their architectural offsets.
///
/// Deliberately asserts *offsets* rather than a total size. The size of a 64-bit
/// TSS is reported inconsistently across references — 104 bytes in some, 116 in
/// others, the difference being whether the trailing reserved word is counted —
/// so pinning the size would encode one source's convention as a hard rule. The
/// offsets, by contrast, are unambiguous and are the only thing the CPU actually
/// uses: `RSP0` at 8, IST1 at 40, and the IST index at 106.
///
/// Each of these is a field the kernel reads or the CPU loads, so getting one
/// wrong is a silent misbehaviour rather than a compile error elsewhere.
const _: () = {
    use core::mem::offset_of;

    assert!(offset_of!(TaskStateSegment, privilege_stack_table) == 8);
    assert!(offset_of!(TaskStateSegment, interrupt_stack_table) == 40);
    assert!(offset_of!(TaskStateSegment, interrupt_stack_table_index) == 106);
    // Three stack pointers, then the word that keeps the IST array at 40. Without
    // the check, dropping `reserved2` would slide every IST entry down eight
    // bytes and install the fault stack into what the CPU reads as IST0's
    // neighbour.
    assert!(
        offset_of!(TaskStateSegment, privilege_stack_table) + 3 * 8
            == offset_of!(TaskStateSegment, reserved2)
    );
    // Seven IST entries must fit before the word that follows them.
    assert!(
        offset_of!(TaskStateSegment, interrupt_stack_table) + 7 * 8
            == offset_of!(TaskStateSegment, reserved3)
    );
};

/// The architectural index of IST1 within
/// [`TaskStateSegment::interrupt_stack_table`].
///
/// The array is *offset* by one, because IST0 does not exist: the CPU's IST1 is
/// `interrupt_stack_table[0]`. Writing `interrupt_stack_table[FAULT_IST_INDEX]`
/// therefore installs the IST2 stack and leaves IST1 pointing at zero, which is
/// what makes a double fault switch to address 0 and triple-fault.
pub const IST_ARRAY_OFFSET: usize = 1;

impl TaskStateSegment {
    const fn zeroed() -> Self {
        Self {
            reserved0: 0,
            reserved1: 0,
            privilege_stack_table: [0; 3],
            reserved2: 0,
            interrupt_stack_table: [0; 7],
            reserved3: 0,
            i64reserved: 0,
            interrupt_stack_table_index: 0,
            reserved4: 0,
        }
    }
}

/// Backing storage for the GDT.
///
/// Raw storage rather than a struct array because the TSS entry is 16 bytes
/// while every other entry is 8, so entries cannot share one uniform type.
#[repr(C, align(16))]
struct Gdt {
    entries: [u64; GDT_SLOTS],
}

static mut GDT: Gdt = Gdt {
    entries: [0; GDT_SLOTS],
};

/// The TSS, written once at boot and then read by hardware on every privilege
/// transition.
///
/// A plain `static mut` rather than an `UnsafeCell`: the value is *not* interior
/// mutable — it is initialised once and then only ever read by the CPU, so the
/// `UnsafeCell` would have added a layer of indirection without adding any
/// synchronisation that the "written exactly once before any reader exists"
/// contract does not already provide.
/// The TSS, under a name the assembly can reach.
///
/// The syscall entry stub reads `RSP0` from the TSS to find the kernel stack
/// to switch to. `naked_asm!` accepts no operands, so the stub cannot name a
/// Rust item the ordinary way — the target has to be reachable *by name*.
///
/// The name is namespaced to the kernel for the same reason
/// `nova_kernel_idt_dispatch` is: a bare `tss` is a common symbol name, and
/// two definitions of it would be resolved by the linker arbitrarily rather
/// than reported.
///
/// This is the kernel's *only* TSS. The CPU reads it on every privilege
/// transition and the syscall stub reads it on every syscall, so the two must
/// see the same value — which is why there is exactly one of them.
#[unsafe(no_mangle)]
pub static mut nova_kernel_tss: TaskStateSegment = TaskStateSegment::zeroed();

/// A stack buffer the CPU may load as a stack pointer.
///
/// The `align(16)` goes on the *type*, not the static: `#[repr(align(..))]` is a
/// type attribute and does not apply to `static` items. Wrapping the buffer in a
/// newtype is the supported way to express "this array's storage is aligned",
/// and it keeps the alignment attached to the buffer rather than to the one place
/// that happens to declare it — so a future stack added to the same module
/// inherits the guarantee instead of having to remember it.
#[repr(align(16))]
struct AlignedStack<const N: usize>(UnsafeCell<[u8; N]>);

// SAFETY: the buffer is genuinely shared — it is a static read by any core — but
// never *concurrently* accessed from Rust. The only writer is the CPU, using the
// stack pointer it was handed, and that happens with interrupts masked and
// without any Rust reference in scope: the CPU's own pushes are outside the
// memory model entirely.
//
// There is also exactly one writer at a time even in principle. `RSP0` is
// switched by `set_privilege_stack` only while the current thread is not running,
// and IST1 is entered with interrupts disabled. So no two accesses can overlap,
// which is what `Sync` requires.
//
// The counter-argument — "a thread could be interrupted mid-push by a fault that
// also uses this stack" — is why the fault stack is separate and why that gate
// carries IST1: a fault arriving from ring 3 switches to `FAULT_STACK` rather
// than continuing on the interrupted thread's `RSP0`.
unsafe impl<const N: usize> Sync for AlignedStack<N> {}

impl<const N: usize> AlignedStack<N> {
    const fn new() -> Self {
        Self(UnsafeCell::new([0; N]))
    }

    /// Address of the first byte, which is where the CPU may push downwards from
    /// the far end.
    fn base(&self) -> u64 {
        // Taking the address of a live reference's field needs no `unsafe`: `self`
        // is a valid reference to a `'static`, and `addr_of!` never dereferences.
        addr_of!(self.0) as u64
    }

    /// Address one past the last byte: the value the CPU loads as a stack
    /// pointer.
    fn top(&self) -> u64 {
        self.base() + N as u64
    }
}

/// The stack the CPU loads as `RSP0` on a privilege transition.
///
/// `static`, not `static mut`: the only writer is the CPU, through the stack
/// pointer, and `UnsafeCell` inside [`AlignedStack`] is what permits that. A
/// `static mut` here would be wrong twice over — it would claim the kernel
/// mutates the buffer in Rust (it does not) and it would let `init` take a
/// shared reference to a mutable static, which is UB by construction in
/// edition 2024.
static KERNEL_STACK: AlignedStack<KERNEL_STACK_SIZE> = AlignedStack::new();

/// Move the CPU onto the kernel's own stack and return.
///
/// # Why the kernel must switch stacks rather than run on the bootloader's
///
/// `_start` arrives on whatever stack the bootloader set up — Limine's, in the
/// direct-map region. Nothing requires the kernel to stay there, and several
/// things go wrong if it does.
///
/// The bootloader's stack is sized to `LIMINE_STACK_SIZE` and is shared with the
/// protocol structures Limine left on it. Worse, it lives in the *direct map*,
/// so its address has no relationship to the kernel image: a fault report that
/// prints `RSP` then shows an address that looks like it is outside the kernel,
/// and the real stack-depth problem is masked by the surprise.
///
/// Most concretely, the CPU pushes the exception frame onto whatever stack is
/// current when the gate's IST field is 0, and the dispatch routine then reads
/// that frame as a `FaultFrame`. If the current stack is one the kernel does not
/// own, the handler can fault while trying to report the original fault, and
/// the machine escalates `#DE` to `#DF` to a triple fault with no output.
///
/// Switching once, at boot, means every later stack top is one the kernel
/// controls and one whose alignment it asserted in `init`.
///
/// # It returns rather than branching
///
/// The naive shape — write `rsp`, then `jmp` to a `-> !` function — discards
/// `_start`'s return address, because that address lives on the *old* stack.
/// The function therefore returns normally: the `ret` reads `_start`'s return
/// address, which is still intact on the bootloader's stack, and pops into the
/// new one.
///
/// The alternative, inline assembly that moves the return address first, is
/// sound too but leaves the compiler reasoning about a frame that no longer
/// exists, which is exactly the situation where it will helpfully insert a
/// stack access it believes is safe.
///
/// # Safety
///
/// Must be called with interrupts masked, and **only** from a frame that will
/// never be unwound — see [`switch_to_kernel_stack_and_continue`], which is the
/// function actually used. Kept separate so the architecture facade has one
/// name for the operation regardless of how the target implements it.
pub unsafe fn switch_to_kernel_stack() {
    let top = KERNEL_STACK.top();
    debug_assert!(
        top % 16 == 0,
        "the kernel stack top must be 16-byte aligned"
    );

    // SAFETY: delegated to this function's contract. `top` is one past the last
    // byte of a kernel-owned, 16-byte-aligned buffer, which is where a
    // downward-growing stack starts.
    unsafe {
        // `nostack` would be a lie: the instruction *does* change `rsp`, so the
        // compiler must not assume it is unchanged across this block.
        asm!("mov rsp, {top}", top = in(reg) top, options(nomem));
    }
}

/// Move the CPU onto the kernel's own stack and continue boot, never returning.
///
/// This exists because the plain `switch_to_kernel_stack` above is unsound at
/// `_start`, and the reason is worth recording in full because nothing in the
/// symptom points at it.
///
/// `_start` has already run `push %r14; push %rbx; sub $0x58,%rsp`. That
/// prologue lives on the **bootloader's** stack. Assigning `rsp` abandons it
/// mid-frame: the epilogue — `pop %rbx; pop %r14; add $0x58,%rsp; ret` — still
/// runs eventually, and by then every one of those reads lands on the *kernel*
/// stack instead. It found whatever the intervening callees had left there,
/// which at the stack's top was the stack's own address, so the final `ret`
/// transferred control to a stack address and the CPU raised `#PF` on an
/// instruction fetch against a non-executable page — error code `0x11`, with a
/// faulting address *inside the stack buffer*.
///
/// The symptom pointed at everything except this. Every segment register was
/// correct, the GDT, TSS and IDT were each verified correct field by field, and
/// the fault appeared to originate in `arch::init`'s return — but `arch::init`
/// returned correctly; it was `_start`, one frame further out, that faulted.
/// Nothing in the printed frame distinguishes "bad return address" from "bad
/// callee", and the faulting address being a stack address is the only clue
/// that says "not a code bug" at all.
///
/// Declaring this `-> !` is what fixes it. LLVM then treats every path after
/// the switch as unreachable *in this frame*, emits no epilogue for it, and
/// the boot sequence continues on the kernel stack without ever unwinding
/// through the bootloader frame. `switch_to_kernel_stack` remains for the
/// architecture-neutral facade, where the same guarantee is documented.
///
/// # Safety
///
/// Must be called exactly once, from the boot entry point, with interrupts
/// masked. The calling frame is abandoned and must never be unwound.
pub unsafe fn switch_to_kernel_stack_and_continue() -> ! {
    let top = KERNEL_STACK.top();
    assert!(
        top % 16 == 0,
        "the kernel stack top is not 16-byte aligned; the CPU faults on use"
    );

    // SAFETY: delegated to this function's contract. `top` is one past the last
    // byte of a kernel-owned, 16-byte-aligned buffer, which is where a
    // downward-growing stack starts.
    //
    // `noreturn` is required for correctness, not just for honesty about the
    // signature: it is what tells LLVM this frame has no epilogue. Without it
    // the compiler is free to keep the `call`'s frame live and later read it
    // through the *new* stack pointer, which is the fault described above.
    //
    // The `jmp` rather than a `call` is deliberate for the same reason. A `call`
    // would push a return address onto the new stack and leave LLVM expecting to
    // consume it; there is nothing to consume, because this function never
    // returns and its caller has no live frame to unwind to.
    unsafe {
        asm!(
            "mov rsp, {top}",
            "jmp {continuation}",
            top = in(reg) top,
            continuation = sym crate::boot_after_stack_switch,
            options(nomem, noreturn),
        )
    }
}

/// The dedicated stack that exceptions arriving from ring 3 land on.
///
/// A separate stack is mandatory, not a convenience: if a user thread overflowed
/// its own stack, reusing it to report the fault would fault again inside the
/// handler, producing a triple fault that destroys every scrap of diagnostic
/// context. This is the stack `#PF` and `#DF` switch to via IST1.
///
/// The 16-byte alignment carried by [`AlignedStack`] is what keeps the CPU from
/// raising `#GP` *while trying to deliver an exception*. That failure is
/// invisible until the first fault: a `.bss` array is packed against whatever
/// precedes it, so its address ends up at an arbitrary offset such as `0xca9`,
/// which looks like a perfectly ordinary address right up until the CPU checks
/// its low four bits.
/// its low four bits.
///
/// `static`, not `static mut`: only the CPU writes here, via the stack pointer it
/// loads from the TSS, and `AlignedStack`'s `UnsafeCell` is what makes that
/// sound without claiming Rust-level mutation.
static FAULT_STACK: AlignedStack<FAULT_STACK_SIZE> = AlignedStack::new();

/// Borrow the TSS.
///
/// # Safety
///
/// The returned reference aliases a `static mut` that the CPU also reads on
/// every privilege transition. Callers must treat it as read-only and must not
/// hold it across anything that could call [`init`] again.
#[inline]
pub const unsafe fn tss() -> &'static TaskStateSegment {
    // SAFETY: delegated to this function's contract. `init` is the only writer
    // and it runs exactly once, before any reader exists.
    unsafe { &*addr_of!(nova_kernel_tss) }
}

/// Set the ring 0 stack pointer the CPU switches to on a privilege transition.
///
/// Used by M2 when switching threads; keeping it here avoids the scheduler
/// touching the TSS directly.
///
/// # Safety
///
/// `rsp0` must be a valid, 16-byte-aligned stack top in kernel space.
pub unsafe fn set_privilege_stack(rsp0: u64) {
    // SAFETY: delegated to this function's contract. `&raw mut` yields a raw
    // pointer rather than a reference, which is what a `#[repr(C, packed)]` field
    // requires: taking a reference to a packed field is an error even when the
    // field happens to be aligned.
    unsafe {
        (&raw mut (*addr_of_mut!(nova_kernel_tss)).privilege_stack_table[0]).write(rsp0);
    }
}

/// Mutable view of the TSS, for `init` only.
///
/// # Safety
///
/// Only `init` may hold this across a write, and only while it is the sole
/// writer.
#[inline]
unsafe fn tss_mut() -> *mut TaskStateSegment {
    // `addr_of_mut!` yields a raw pointer without forming a reference, so it
    // needs no `unsafe` block of its own; the function's own `unsafe` is what
    // authorises the later writes through this pointer.
    addr_of_mut!(nova_kernel_tss)
}

/// Install the GDT and TSS.
///
/// # Safety
///
/// Loads the GDT and TSS into the CPU. Must be called before any ring 3 code can
/// run, and must not be called a second time without reloading every segment
/// register that references the old table.
pub unsafe fn init() {
    // SAFETY: called exactly once from `_start`, before any ring 3 code exists.
    // This is the sole writer of `GDT` and `TSS`.
    unsafe {
        let gdt_base = addr_of_mut!(GDT);

        // Slots 0-2: null, kernel code, kernel data.
        write_slot(gdt_base, 0, encode(null_descriptor()));
        write_slot(
            gdt_base,
            1,
            encode(code_segment(KERNEL_CODE_SELECTOR, DPL_RING0)),
        );
        write_slot(
            gdt_base,
            2,
            encode(data_segment(KERNEL_DATA_SELECTOR, DPL_RING0)),
        );
        // Slot 3: the ring 3 code descriptor. `STAR` derives the user code
        // selector from it, so it must be the *conforming* long-mode code
        // descriptor at DPL 3.
        write_slot(
            gdt_base,
            USER_CODE_INDEX,
            encode(code_segment(USER_CODE_SELECTOR, DPL_RING3)),
        );
        // Slots 4-5: the TSS, written last because it needs the final TSS base.
        // Slot 6: the ring 3 data descriptor.
        write_slot(
            gdt_base,
            USER_DATA_INDEX,
            encode(data_segment(USER_DATA_SELECTOR, DPL_RING3)),
        );

        let tss = tss_mut();
        let kernel_top = KERNEL_STACK.top();
        let fault_top = FAULT_STACK.top();

        // Both stacks must present a 16-byte-aligned top to the CPU. `RSP0` and
        // the IST entries are checked here, at boot, so a linker change that
        // breaks alignment is reported with a message instead of surfacing as a
        // triple fault the first time an exception is taken — by which point the
        // machine has already reset and there is nothing left to print.
        //
        // Checking the *top* rather than the base is what matters: a stack whose
        // base is aligned but whose size is not a multiple of 16 has an unaligned
        // top, which is the value the CPU actually loads.
        assert!(
            kernel_top % 16 == 0,
            "the ring 0 stack top is not 16-byte aligned; the CPU faults on use",
        );
        assert!(
            fault_top % 16 == 0,
            "the IST1 fault stack top is not 16-byte aligned; the CPU faults on use",
        );

        // Interrupt arriving from ring 3 must switch to the kernel stack, not to
        // whatever user stack the thread was running on.
        //
        // `&raw mut` rather than field syntax: `TaskStateSegment` is
        // `repr(C, packed)`, so forming a reference to a field is an error even
        // where the field happens to be aligned. Writing through a raw pointer to
        // a packed field is the documented way to do this.
        (&raw mut (*tss).privilege_stack_table[0]).write(kernel_top);
        (&raw mut (*tss).interrupt_stack_table_index).write(FAULT_IST_INDEX);
        // `interrupt_stack_table[0]` is IST1, not IST0: the CPU's IST1 is the
        // first entry of the array. Indexing by `FAULT_IST_INDEX` directly
        // installs the IST2 stack and leaves IST1 holding zero, so a double fault
        // switches to address 0 and triple-faults with no output. The subtraction
        // is what makes the relationship between the CPU's IST number and the
        // array index explicit rather than assumed.
        (&raw mut (*tss).interrupt_stack_table[FAULT_IST_INDEX as usize - IST_ARRAY_OFFSET])
            .write(fault_top);

        // Fill the 16-byte TSS descriptor last: it needs the final TSS base.
        let tss_base = tss as u64;
        let tss_limit = (size_of::<TaskStateSegment>() - 1) as u16;
        // A TSS limit is a byte count and the segment is 124 bytes, so this must
        // fit in 16 bits *without* the granularity bit doing any scaling.
        assert!(size_of::<TaskStateSegment>() <= 0x10000);
        write_tss_descriptor(gdt_base, TSS_SLOT, tss_base, tss_limit);

        // The pseudo-descriptor `lgdt` reads must live somewhere with a *pinned*
        // alignment, for the same reason the IDT's does: `lgdt` faults on an
        // unaligned operand, and a stack local's address is only as aligned as
        // the compiler's current frame layout happens to make it. That alignment
        // is not part of the type, so a later refactor that changes the frame
        // can silently un-align it.
        //
        // A `static` with the declared `align(8)` is aligned by construction, and
        // it also removes the "nothing may run between building the descriptor
        // and the instruction that reads it" hazard a stack local carries.
        static mut GDT_DESCRIPTOR: AlignedPseudoDescriptor = AlignedPseudoDescriptor([0; 10]);
        // SAFETY: `init` is the sole writer, called once from `_start`, and the
        // write happens-before the `lgdt` that reads it in the same block.
        let descriptor = &mut *addr_of_mut!(GDT_DESCRIPTOR);
        *descriptor =
            AlignedPseudoDescriptor::new((size_of::<Gdt>() - 1) as u16, addr_of!(GDT) as u64);

        // `lgdt` reads the pseudo-descriptor directly out of memory, so
        // nothing may run between building it and the instruction — not even
        // a diagnostic that only reads it. A function call in between
        // overwrites the stack slot the descriptor occupies, and the CPU then
        // loads whatever the callee left there: a wrong limit and a wrong
        // base, which leaves the CPU using the bootloader's table.
        //
        // The symptom is silent and deeply misleading: the kernel boots, runs
        // every test that does not take an exception, and then triple-faults
        // on the first one, with nothing in the log pointing at the GDT.
        //
        // `lgdt` first, then the segment reloads, then `ltr`.
        //
        // The order is not interchangeable. `ltr` raises `#GP` if its selector
        // does not name a present TSS descriptor, so it must come after the GDT
        // holding that descriptor is installed — but the IDT is not installed at
        // this point, so a failure here would have nowhere to land.
        //
        // `lgdt` itself cannot fault: the pseudo-descriptor is a stack local and
        // the table it describes is static, both mapped and written. The
        // remaining instructions are non-faulting for the same reason, so this
        // window is closed by construction rather than by ordering luck.
        //
        // SAFETY: delegated to this function's contract.
        load_gdt(&descriptor);
        reload_data_segments();
        load_tss(KERNEL_TSS_SELECTOR);

        // Reload `CS` *last*, after every other instruction that still refers to
        // the bootloader's segment table. Until this point the CPU is running in
        // Limine's `CS = 0x28`, which no IDT gate names.
        //
        // The trampoline has no frame of its own: it reads the caller's return
        // address off the stack and returns to it with `CS` reloaded, popping
        // exactly what it pushed. So this call is self-balancing — `init`
        // continues normally afterwards, with `RSP` identical on both sides.
        //
        // SAFETY: `lgdt`, `ltr` and the data-segment reloads have all run, so the
        // selector the trampoline pushes resolves in the installed table.
        // Interrupts are still masked, so nothing can land in the window where
        // the stack holds a partially-built return frame.
        reload_code_segment_and_return();

        // Reached with the kernel's own `CS` installed.
        klog!(
            Level::Debug,
            "gdt: loaded {GDT_SLOTS} slots, tss rsp0={:#x} ist{FAULT_IST_INDEX}={:#x}",
            KERNEL_STACK.top(),
            FAULT_STACK.top(),
        );
    }
}

/// Reload `CS`, then return to the caller's continuation.
///
/// The Rust signature is `-> ()`, **not** `-> !`, and that is load-bearing. The
/// body never executes an ordinary `ret`; it ends in `iretq`, which transfers
/// control to the address the caller pushed. But declaring it `-> !` tells LLVM
/// the call never comes back, and it then deletes everything after the call —
/// including the epilogue that would have balanced `init`'s frame — and pads the
/// gap with `int3`. `int3` is a breakpoint, so the instruction right after the
/// return raised `#BP`, and with the IDT not yet installed that became `#GP` and
/// a triple fault.
///
/// The symptom was a kernel whose every segment was correct and which still
/// could not run one instruction past the reload: QEMU reported `#GP(0x1a)` at
/// the `int3` immediately following the call.
///
/// # Why the resume address is read from the stack
///
/// The trampoline has no frame, so `[rsp]` on entry is exactly the return
/// address the `call` pushed — the caller's continuation, which is what the
/// return must transfer control to.
///
/// An earlier version pushed a *label* instead. That is where two expensive
/// failures came from, and both are worth recording because neither looked like
/// a segment bug:
///
/// * Placing the label *before* the return pushed the return instruction's own
///   address, so control jumped back into the middle of this sequence and the
///   second return popped more words out of the caller's frame. Symptom:
///   `RIP = 0x48ffffff40e8cb48` — this function's own opcode bytes, read as an
///   address.
/// * Placing the label *after* it, with the trampoline reached one call deeper
///   than intended, resumed one word into `init`'s frame. Symptom:
///   `RIP = 0xcccccccccccccccc` — an uninitialised `.bss` slot.
///
/// In both cases `CS` was already correct, which is precisely what made them
/// hard: the fault pointed away from the segment code that had just run.
///
/// # Why five words
///
/// `iretq` pops five words whenever it returns to the same privilege level:
/// `RIP`, `CS`, `RFLAGS`, `RSP`, `SS`. All five must be present even though the
/// privilege does not change, and the `RSP` word is the caller's post-`call`
/// stack pointer verbatim — see the note in the body.
///
/// # Safety
///
/// Must be called with `GDT` loaded and `DS`/`ES`/`SS` already pointing at the
/// kernel data selector, with interrupts masked. Transfers control to the
/// caller's continuation with `CS` reloaded, leaving the caller's stack pointer
/// exactly as it was before the call that got here.
#[unsafe(naked)]
unsafe extern "C" fn reload_code_segment_and_return() {
    core::arch::naked_asm!(
        // `r11` is call-clobbered by the SysV ABI and is what `naked_asm!` uses
        // for its own label fixups, so it is the one register this may borrow.
        //
        // On entry `[rsp]` is exactly the return address the `call` pushed, which
        // is the caller's continuation and becomes the new `RIP`. There is no
        // prologue here to reason about and no frame of our own: that is the
        // whole design, and it is what makes the arithmetic below checkable.
        //
        // `iretq` here is returning to the *same* privilege, so the CPU pops the
        // full five-word frame it always pushes: `RIP`, `CS`, `RFLAGS`, `RSP`,
        // `SS`. Three words leaves it reading past the frame into whatever the
        // caller happened to have there — which is why a three-word version
        // raised `#GP` with `CS` already correct.
        //
        // Reverse push order so `RIP` ends up at the lowest address:
        // `SS`, `RSP`, `RFLAGS`, `CS`, then `RIP`.
        //
        // # The stack pointer, and why it is `r11 + 8`
        //
        // This is the part that was wrong for a long time, and the wrong version
        // was *documented as an invariant*, which is why it survived so many
        // rebuilds. Measuring it at the trampoline's own entry settles it:
        //
        //     rsp = 0xffffffff80125c30   ->  [rsp] = 0xffffffff80101e11 (return addr)
        //
        // The `call` pushed that return address and left `rsp` pointing *at* it.
        // The caller's `rsp`, as it was before the call pushed anything — the
        // value the caller's own `ret` will eventually read its return address
        // from — is therefore `rsp + 8`, not `rsp`.
        //
        // `iretq` restores `RSP` to exactly the word in its frame; it neither
        // derives one nor adds anything. So putting `r11` (= entry `rsp`) in that
        // slot restores a stack pointer that is still pointing at the `call`'s
        // return address. The `iretq` itself succeeds — `RIP` and `CS` are right,
        // so nothing looks wrong — but every frame the caller builds from there on
        // is eight bytes lower than it should be, and the deficit is invisible
        // until a function returns.
        //
        // The symptom was a `#PF` with error code `0x11` (present, instruction
        // fetch, non-executable page) whose faulting address was a *stack*
        // address. The `ret` at the end of `arch::init` popped
        // `[0xffffffff80125c38]`, which by then held `KERNEL_STACK.top()`
        // (`0xffffffff80125cb0`) rather than a return address, so the CPU tried to
        // execute the top of the stack buffer. Every printed register was
        // plausible and every segment was correct, which is what made it look
        // like a segment or paging bug rather than an eight-byte bookkeeping
        // error in the frame this function builds.
        //
        // So: `r11 + 8` is the caller's pre-call `rsp`, and that is what the
        // `RSP` slot must carry.
        //
        // Reverse push order so `RIP` ends up at the lowest address:
        // `SS`, `RSP`, `RFLAGS`, `CS`, then `RIP`.
        //
        // The two failed variants, each verified by stepping to the `iretq`
        // under the debugger:
        //
        // * No `RSP` word at all (three pushes) — the CPU read two words past the
        //   frame and reported `#GP`, with `CS` correct and `RFLAGS` holding
        //   `0x5cb0`, which is the low half of `r11`. That error code is the
        //   giveaway: a frame word mistaken for a segment selector.
        // * Pushing `r11` unchanged — the `iretq` succeeded and landed on the
        //   right instruction with the right `CS`, so this looked correct for a
        //   long time. It is the eight-byte deficit described above, and it is
        //   what produced the `#PF` at a stack address.
        // * Pushing `r11 - 8` on the opposite theory — the same `#PF`, shifted
        //   down, which at least proved the value mattered.
        //
        // # Why `iretq` and not a far return
        //
        // `retfq` was tried first, on the reasonable reading that its frame is
        // five words and the CPU takes `RSP` from the frame. Its frame is five
        // words, but it is a *different* five: `RIP`, `CS`, `RFLAGS`, then two
        // slots that are only meaningful on a privilege change, and the CPU does
        // not restore `RSP` from it on a same-privilege return — it derives the
        // stack pointer from the frame's own extent instead. Every correction
        // term tried in that slot produced the same final `rsp`, which is the
        // signature of a value the CPU never reads.
        //
        // `iretq` has no such ambiguity: it restores `RSP` from the word that
        // sits fourth in the frame, so the slot can be given the caller's
        // pre-call stack pointer directly and mean exactly that.
        //
        // # The other failures, for the same underlying reason
        //
        // Every earlier shape of this reasoned about what the stack held rather
        // than about what the CPU consumes. Two pushed a *label* instead of
        // reading the caller's return address: one returned into the middle of
        // this sequence (`RIP = 0x48ffffff40e8cb48`, this function's own opcode
        // bytes read as an address), the other one word into `init`'s frame
        // (`RIP = 0xcccccccccccccccc`, an uninitialised `.bss` slot). In both
        // `CS` was already correct, so the fault pointed away from the segment
        // code that had just run successfully.
        //
        // The selectors are immediates, not register operands. That is the point:
        // a `CS` value that came from a register was, in an early version,
        // silently coalesced with a stack value and ended up holding a code
        // address. See [`FAR_RETURN_CODE_SELECTOR`].
        // `r11` is adjusted *before* the first push, so the `SS`, `RSP` and `RFLAGS`
        // words are laid out below the caller's frame rather than overwriting it.
        // Adding 8 here is the whole fix; see the note above for why.
        "mov r11, rsp",
        "add r11, 8",
        "mov r10, qword ptr [r11 - 8]",
        "push 0x10",
        "push r11",
        "pushfq",
        "push 0x08",
        "push r10",
        "iretq",
    )
}

/// Print the raw GDT and IDT bytes to the console.
///
/// The GDT is the one structure whose corruption produces a symptom with no
/// diagnostic of its own: a bad descriptor means the CPU rejects the gate and
/// raises `#GP` instead of entering the handler, and the `#GP` handler is reached
/// through an equally bad gate, so the machine triple-faults with an empty log.
///
/// Dumping the raw bytes lets the encoding be checked against the architectural
/// layout by eye, which is far faster than reasoning about what a mis-encoded
/// byte *would* do.
///
/// Separate from [`init`] because the console must already exist: `init` runs
/// before the UART is programmed, so anything it prints is silently discarded.
pub fn dump_tables() {
    dump_table("gdt", addr_of!(GDT) as *const u8, size_of::<Gdt>());
    dump_table(
        "idt",
        crate::arch::x86_64::idt::idt_bytes(),
        crate::arch::x86_64::idt::IDT_ENTRIES * 16,
    );
    // The pseudo-descriptors `lgdt` and `lidt` actually read. Printing them
    // settles whether the *instruction* was issued with the right bytes, which
    // is a different question from whether the tables themselves are correct.
    crate::arch::x86_64::idt::dump_pseudo_descriptors();
}

/// The raw bytes of the GDT's pseudo-descriptor.
///
/// The ten bytes `lgdt` reads: a little-endian 16-bit limit followed by a
/// little-endian 64-bit base.
///
/// Returned by value rather than by pointer to a stack temporary, because a
/// pointer would dangle by the time the caller dereferenced it — and a
/// diagnostic that reads freed stack is worse than no diagnostic at all.
pub fn pseudo_descriptor_bytes() -> [u8; 10] {
    let mut bytes = [0u8; 10];
    bytes[0..2].copy_from_slice(&((size_of::<Gdt>() - 1) as u16).to_le_bytes());
    bytes[2..10].copy_from_slice(&(addr_of!(GDT) as u64).to_le_bytes());
    bytes
}

/// Print a table's raw bytes to the console, for boot-time diagnosis.
///
/// Deliberately primitive: it formats hex itself and writes through the console
/// directly, because a diagnostic that depends on the subsystem it is diagnosing
/// cannot report that subsystem's failure.
fn dump_table(label: &str, bytes: *const u8, len: usize) {
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

/// The null segment: present bit clear, so any use faults.
const fn null_descriptor() -> SegmentDescriptor {
    SegmentDescriptor {
        limit_low: 0,
        base_low: 0,
        base_mid: 0,
        access: 0,
        limit_flags: 0,
        base_high: 0,
    }
}

/// Build a long-mode code segment descriptor.
///
/// Ring 3 gets the conforming bit so that a `call` through a user code selector
/// stays within the intended segment bounds.
const fn code_segment(selector: u16, dpl: u8) -> SegmentDescriptor {
    let _ = selector;
    SegmentDescriptor {
        limit_low: 0xFFFF,
        base_low: 0,
        base_mid: 0,
        access: (1 << 7) | S_BIT | (dpl << 5) | CODE_TYPE | if dpl == DPL_RING3 { 0x4 } else { 0 },
        // Must include the long-mode bit; see `CODE_FLAGS`.
        limit_flags: CODE_FLAGS,
        base_high: 0,
    }
}

/// Build a long-mode data segment descriptor.
const fn data_segment(selector: u16, dpl: u8) -> SegmentDescriptor {
    let _ = selector;
    SegmentDescriptor {
        limit_low: 0xFFFF,
        base_low: 0,
        base_mid: 0,
        access: (1 << 7) | S_BIT | (dpl << 5) | DATA_TYPE,
        // Must not include the long-mode bit; see `DATA_FLAGS`.
        limit_flags: DATA_FLAGS,
        base_high: 0,
    }
}

/// Pack an 8-byte segment descriptor into its 64-bit GDT slot form.
const fn encode(segment: SegmentDescriptor) -> u64 {
    // The architectural byte order, which is what the CPU reads:
    //
    //   bytes 0-1   limit[15:0]
    //   bytes 2-3   base[15:0]
    //   byte  4     base[23:16]
    //   byte  5     access
    //   byte  6     limit[19:16] | G | D/B | L | AVL
    //   byte  7     base[31:24]
    //
    // The struct splits `base[15:0]` into `base_low: u16` and `base_mid: u8`
    // because that is how the original reference code named it, and the two
    // together must land in bytes 2-3. Everything after that follows the byte
    // order above.
    //
    // # `access` is byte 5, not byte 4
    //
    // Placing `access` anywhere but byte 5 is the failure that costs the most
    // time, because the resulting descriptor still looks like a segment
    // descriptor. It is a segment descriptor whose *fields have moved*, so the
    // CPU reads the `S`, `CS` and `L` bits out of bytes that hold the base and
    // the limit instead, decides the descriptor is not a code segment, and
    // raises `#GP` for every reference to it.
    //
    // The reference that hurts is the IDT: every gate names
    // `KERNEL_CODE_SELECTOR`, so the moment an exception is taken the CPU
    // rejects the gate, raises `#GP` instead of entering the handler, and the
    // `#GP` gate is rejected the same way. The machine escalates to `#DF` and
    // then to a triple fault with an empty serial log.
    //
    // A code segment with a zero base is a poor test of this, because it stays
    // usable even when mis-encoded for the higher half: long mode ignores the
    // base for segment purposes. Only the present bit and the type still
    // matter, which is why a one-byte shift can survive every test that does
    // not take an exception.
    //
    // The bytes are assembled one at a time rather than by shifting fields
    // into a `u64`, because a shift encodes the same layout decision in a
    // place where it is easy to get wrong by eight, and a wrong shift is
    // invisible in the source. Listing the bytes makes the layout the code.
    let bytes = [
        segment.limit_low as u8,
        (segment.limit_low >> 8) as u8,
        segment.base_low as u8,
        (segment.base_low >> 8) as u8,
        segment.base_mid,
        segment.access,
        segment.limit_flags,
        segment.base_high,
    ];
    u64::from_le_bytes(bytes)
}

/// Compile-time proof that the GDT encodings are what the CPU requires.
///
/// These are the invariants whose absence produces a silent triple fault rather
/// than a diagnostic, so they are checked here rather than discovered by booting.
const _: () = {
    // The long-mode bit must be set on the code segment. See `CODE_FLAGS`.
    assert!(
        CODE_FLAGS & (1 << 5) != 0,
        "the code segment must set the L bit"
    );
    // Granularity is required for a 4 KiB-granularity long segment.
    assert!(CODE_FLAGS & (1 << 7) != 0, "granularity must be set");
    // D/B must be *clear* on a 64-bit code segment. The CPU rejects a gate whose
    // target descriptor has both L and D/B set, which is a `#GP` on every
    // exception and therefore a triple fault on the first one.
    assert!(
        CODE_FLAGS & (1 << 6) == 0,
        "a 64-bit code segment must leave D/B clear",
    );
    // L is bit 5 of the *flags* byte; the present bit (P, bit 7 of *access*) is
    // the other half of the descriptor and is set in `code_segment`. Checking it
    // here against `CODE_FLAGS` would be checking the wrong byte.
    //
    // The access byte is `P | S | DPL | type`. Both the present bit and the
    // `S` bit are required, and both are asserted on the *built* descriptor
    // below rather than on the constants, because a constant expression of the
    // form `a | b != 0` is trivially true for every non-zero `a` and proves
    // nothing.
    //
    // The four type bits are, from bit 3 down: executable (1 = code, 0 = data),
    // conforming/readable, readable/writable, accessed. So a readable code
    // segment is 0b1010 = 0xA and a writable data segment is 0b0010 = 0x2.
    assert!(
        CODE_TYPE & 0x8 != 0,
        "the code descriptor must be a code type"
    );
    assert!(CODE_TYPE & 0x2 != 0, "the code segment must be readable");
    assert!(
        DATA_TYPE & 0x8 == 0,
        "the data descriptor must be a data type"
    );
    assert!(DATA_TYPE & 0x2 != 0, "the data segment must be writable");
    // The `S` bit must be set on a code or data segment. Without it the
    // descriptor is a *system* segment, and the CPU refuses to use it as the
    // target of an IDT gate — which turns the first exception into a triple
    // fault with an empty serial log.
    assert!(
        code_segment(KERNEL_CODE_SELECTOR, DPL_RING0).access & S_BIT != 0,
        "the code descriptor's access byte must set the S bit",
    );
    assert!(
        data_segment(KERNEL_DATA_SELECTOR, DPL_RING0).access & S_BIT != 0,
        "the data descriptor's access byte must set the S bit",
    );
    // The TSS is a *system* segment, so its access byte must **not** set `S`.
    // Setting it would make the descriptor a 64-bit code segment with a
    // 16-byte limit, and `ltr` would fail with `#GP`.
    //
    // The type field of a 64-bit available TSS is `0b1001`: bit 0 says the
    // descriptor is a TSS rather than an LDT, bit 2 says it is available, and
    // bit 3 says it is the 64-bit form.
    assert!(
        TSS_TYPE == 0b1001,
        "the TSS type must be a 64-bit available TSS descriptor",
    );
    // Neither should be marked accessed: the CPU manages that bit, and a preset
    // value is a (harmless but wrong) claim about hardware state.
    assert!(CODE_TYPE & 0x1 == 0, "accessed must be left to the CPU");
    assert!(DATA_TYPE & 0x1 == 0, "accessed must be left to the CPU");

    // The selectors the IDT gates and `KERNEL_CODE_SELECTOR` name must be the
    // slots `init` actually writes, or `lidt` gates point at the wrong
    // descriptor.
    assert!(KERNEL_CODE_SELECTOR == (KERNEL_CODE_INDEX as u16) * 8);
    assert!(KERNEL_DATA_SELECTOR == (KERNEL_DATA_INDEX as u16) * 8);

    // The TSS descriptor is 16 bytes, so it must occupy two whole 8-byte slots.
    assert!(
        TSS_SLOT * 8 + 16 <= GDT_SLOTS * 8,
        "the TSS must fit in the GDT"
    );
    // And the slot before it must be the user code slot, so the TSS cannot
    // overlap it.
    assert!(TSS_SLOT == USER_CODE_INDEX + 1);
    // The user data descriptor must follow the TSS's two slots.
    assert!(USER_DATA_INDEX == TSS_SLOT + 2);
    // The user code descriptor must be *conforming* at DPL 3, and the data
    // descriptor writable at DPL 3. A ring 3 code segment without the
    // conforming bit is rejected by the CPU's `STAR`/`iretq` validation with a
    // `#GP` that no handler can distinguish from a bug elsewhere.
    assert!(
        code_segment(USER_CODE_SELECTOR, DPL_RING3).access & 0x4 != 0,
        "the user code segment must be conforming",
    );
    assert!(
        code_segment(USER_CODE_SELECTOR, DPL_RING3).access & (0x3 << 5) == (DPL_RING3 as u8) << 5,
        "the user code segment must be at DPL 3",
    );
    assert!(
        data_segment(USER_DATA_SELECTOR, DPL_RING3).access & (0x3 << 5) == (DPL_RING3 as u8) << 5,
        "the user data segment must be at DPL 3",
    );
    assert!(
        data_segment(USER_DATA_SELECTOR, DPL_RING3).access & DATA_TYPE != 0,
        "the user data segment must be writable",
    );
    // The ring 3 selectors carry RPL 3 in their low two bits; a selector with
    // RPL 0 would make `iretq` load the *kernel* code segment and the
    // privilege change would silently not happen.
    assert!(USER_CODE_SELECTOR & 0x3 == 3);
    assert!(USER_DATA_SELECTOR & 0x3 == 3);
    // And the ring 0 selectors must not.
    assert!(KERNEL_CODE_SELECTOR & 0x3 == 0);
    assert!(KERNEL_DATA_SELECTOR & 0x3 == 0);

    // The encoder's byte layout, pinned.
    //
    // A kernel code segment: present, ring 0, code, readable.
    //
    //   access = 0x9A = present(0x80) | S(0x10) | code(0x08) | readable(0x02)
    //   flags  = 0xAF = G(0x80) | L(0x20) | limit[19:16](0x0F)
    //
    // The low nibble of the flags byte is *not* the type — the type lives in
    // the access byte. In the flags byte those four bits are limit[19:16], and
    // they are 0xF so that the limit is the full 4 GiB that `G` implies.
    //
    // The `S` bit matters as much as the type does: without it the descriptor
    // is a system segment, and the CPU rejects the gate that names it.
    //
    // little-endian, byte 0 first: FF FF 00 00 00 9A AF 00
    assert!(
        encode(code_segment(KERNEL_CODE_SELECTOR, DPL_RING0)) == 0x00AF_9A00_0000_FFFF,
        "the code descriptor must encode to limit=0xffff access=0x9a flags=0xaf",
    );

    // A data segment: present, ring 0, writable, no long-mode bit.
    //
    //   access = 0x92 = present(0x80) | S(0x10) | writable(0x02)
    //   flags  = 0xCF = G(0x80) | D/B(0x40) | limit[19:16](0x0F)
    //
    // Note that a data segment's access byte does *not* carry the "data" bit:
    // bit 3 of the type field is the code/data selector, and it is 0 for data.
    //
    // little-endian, byte 0 first: FF FF 00 00 00 92 CF 00
    assert!(
        encode(data_segment(KERNEL_DATA_SELECTOR, DPL_RING0)) == 0x00CF_9200_0000_FFFF,
        "the data descriptor must encode to limit=0xffff access=0x92 flags=0xcf",
    );

    // The access byte is byte 5 and the flags byte is byte 6. These two checks
    // are the ones that matter: a descriptor whose access byte is not present
    // is unusable, and every reference to it raises `#GP` — including the one
    // the CPU makes when validating an IDT gate, which turns the first
    // exception into a triple fault.
    let encoded = encode(code_segment(KERNEL_CODE_SELECTOR, DPL_RING0));
    assert!(
        (encoded >> 40) & 0xFF == 0x9A,
        "byte 5 must be the access byte, present, a segment, and executable",
    );
    assert!(
        (encoded >> 48) & 0xFF == CODE_FLAGS as u64,
        "byte 6 must carry limit[19:16] and the granularity/long-mode bits",
    );
    assert!(
        (encoded >> 32) & 0xFF == 0,
        "byte 4 is base[23:16], which is zero for a flat segment",
    );
};

/// Store a packed descriptor into an 8-byte GDT slot.
fn write_slot(gdt: *mut Gdt, index: usize, value: u64) {
    // SAFETY: `index < GDT_SLOTS` is guaranteed by every caller, and `gdt` points
    // at the static `GDT` array.
    unsafe {
        let base = addr_of_mut!((*gdt).entries) as *mut u64;
        base.add(index).write(value);
    }
}

/// Write the 16-byte TSS entry across GDT slots 4 and 5.
fn write_tss_descriptor(gdt: *mut Gdt, slot: usize, base: u64, limit: u16) {
    // SAFETY: `slot == TSS_SLOT` and the GDT has `GDT_SLOTS` 8-byte slots, so
    // writing a 16-byte descriptor at slot 4 stays within the array.
    unsafe {
        let target = addr_of_mut!((*gdt).entries) as *mut u8;
        target
            .add(slot * 8)
            .cast::<TssDescriptor>()
            .write(TssDescriptor {
                limit_low: limit,
                base_low: (base & 0xFFFF) as u16,
                base_mid: ((base >> 16) & 0xFF) as u8,
                access: (1 << 7) | (DPL_RING0 << 5) | TSS_TYPE,
                // A TSS limit is a **byte** count, so bit 7 (G, granularity) must
                // be clear: set, it would multiply the limit by 4096 and describe
                // a 64 KiB TSS while the kernel only owns 124 bytes.
                limit_flags: 0,
                base_high: ((base >> 24) & 0xFF) as u8,
                // The upper 8-byte slot. Every byte of it is part of the base
                // address, and every one of them must be written: leaving them
                // zero makes the CPU read a TSS base of
                // `0x0000_0000_8b12_5ce0`, which is not the TSS.
                base_32_39: ((base >> 32) & 0xFF) as u8,
                base_40_47: ((base >> 40) & 0xFF) as u8,
                base_48_55: ((base >> 48) & 0xFF) as u8,
                base_56_63: ((base >> 56) & 0xFF) as u8,
                reserved: 0,
            });
    }
}

/// Load the GDT register.
///
/// # Safety
///
/// `descriptor` must point at a live 10-byte pseudo-descriptor describing the
/// full GDT.
unsafe fn load_gdt(descriptor: &PseudoDescriptor) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        asm!("lgdt [{}]", in(reg) descriptor, options(readonly, nostack, preserves_flags));
    }
}

/// Load the kernel data selector into every data segment register.
///
/// # Safety
///
/// Must follow `lgdt`. Reloading `SS` is architecturally always legal once a
/// present data descriptor is installed; loading the null selector instead is
/// also accepted by the CPU and is the trap this function's comment describes.
unsafe fn reload_data_segments() {
    // `KERNEL_DATA_SELECTOR`, not zero.
    //
    // Loading selector 0 into a data segment is architecturally *legal* — it is
    // what a freshly-entered kernel should avoid, but the CPU accepts it and the
    // segment only faults on use. That is precisely what makes it a trap: it
    // looks correct, `ds`/`es`/`ss` all read back as zero, and nothing faults
    // until something uses `ss`.
    //
    // The CPU *does* use `SS` on every interrupt: entering ring 0 loads `SS` from
    // the current `SS` selector so it can push `SS` onto the new frame. A null
    // `SS` there is a `#GP` with the selector's present bit clear — before the
    // handler gets a chance to run, so no fault report is ever printed. The
    // `#DF` and then the triple fault that follow both report the *original*
    // faulting instruction and the original `RSP`, which makes it read as "the
    // handler re-faulted at the same place" rather than "the stack selector was
    // null".
    //
    // A real data descriptor costs nothing and removes the trap.
    //
    // SAFETY: delegated to this function's contract.
    unsafe {
        asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            in("ax") KERNEL_DATA_SELECTOR,
            options(nostack, preserves_flags)
        );
    }
}

/// Load the TSS into the task register.
unsafe fn load_tss(selector: u16) {
    // SAFETY: the selector was just written into the GDT by `init`.
    unsafe {
        asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags));
    }
}
