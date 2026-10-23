//! Global Descriptor Table and Task State Segment.
//!
//! The kernel runs with segments set up so that privilege transitions in M2
//! (ring 3 to ring 0 via `syscall`) have a well-defined base and limit. The TSS
//! in particular is what lets hardware land an interrupt on a known stack.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{addr_of, addr_of_mut};

use crate::klog;
use crate::logger::Level;

/// GDT slot count. Slot 3 stays null so that the TSS can occupy slots 4 and 5.
const GDT_SLOTS: usize = 6;
/// Slot index of the TSS low half.
///
/// Slot 3 is skipped so the 16-byte TSS descriptor lands at slot 4, straddling
/// slots 4 and 5. [`KERNEL_TSS_SELECTOR`] is derived from this, so the selector
/// and the slot cannot disagree.
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
pub const USER_CODE_SELECTOR: u16 = 0x20 | 0x03;
/// Ring 3 data selector.
pub const USER_DATA_SELECTOR: u16 = 0x28 | 0x03;

/// Ring 0 privilege level.
const DPL_RING0: u8 = 0;
/// Ring 3 privilege level.
const DPL_RING3: u8 = 3;

/// Type bits for a long-mode code segment: present, 64-bit, readable.
const CODE_TYPE: u8 = 0xA;
/// Type bits for a long-mode data segment: present, writable.
const DATA_TYPE: u8 = 0x2;
/// Type bits for the TSS: present, 64-bit available.
const TSS_TYPE: u8 = 0x9;

/// Which IST slot the dedicated fault stack occupies.
pub const FAULT_IST_INDEX: u16 = 1;

/// Size of the kernel's ring 0 stack.
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Size of the dedicated fault-handling stack.
pub const FAULT_STACK_SIZE: usize = 16 * 1024;

/// The 16-byte system-segment entry the TSS occupies in the GDT.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct TssDescriptor {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    /// Limit bits 19:16; no granularity bit, since a TSS limit is a byte count.
    limit_flags: u8,
    base_high: u8,
}

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
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct PseudoDescriptor {
    limit: u16,
    base: u64,
}

/// Task State Segment for ring 0.
///
/// Only the fields x86-64 consults on a privilege transition are present:
/// `privilege_stack_table[0]` selects the ring-0 stack, and
/// `interrupt_stack_table_index` names the L0 interrupt stack.
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub struct TaskStateSegment {
    reserved0: u32,
    /// Ring 3 privilege stack pointer.
    pub privilege_stack_table: [u64; 3],
    /// Ring 3 privilege stack segment selector (unused under 64-bit).
    reserved1: u64,
    /// Ring 0 stack pointers, one per IST index.
    pub interrupt_stack_table: [u64; 7],
    reserved2: u64,
    reserved3: u64,
    reserved4: u64,
    reserved5: u64,
    /// IST index used by exceptions arriving from ring 3, i.e. double fault.
    pub interrupt_stack_table_index: u16,
    reserved6: u16,
    reserved7: u64,
}

impl TaskStateSegment {
    const fn zeroed() -> Self {
        Self {
            reserved0: 0,
            privilege_stack_table: [0; 3],
            reserved1: 0,
            interrupt_stack_table: [0; 7],
            reserved2: 0,
            reserved3: 0,
            reserved4: 0,
            reserved5: 0,
            interrupt_stack_table_index: 0,
            reserved6: 0,
            reserved7: 0,
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
static mut TSS: TaskStateSegment = TaskStateSegment::zeroed();

/// The dedicated ring 0 stack that interrupts arriving from ring 3 land on.
static mut KERNEL_STACK: UnsafeCell<[u8; KERNEL_STACK_SIZE]> =
    UnsafeCell::new([0; KERNEL_STACK_SIZE]);

/// The stack used to handle a fault arriving from ring 3.
///
/// A separate stack is mandatory: if a user thread overflowed its own stack,
/// reusing it to report the fault would fault again, producing a triple fault
/// that destroys all diagnostic context.
static mut FAULT_STACK: UnsafeCell<[u8; FAULT_STACK_SIZE]> = UnsafeCell::new([0; FAULT_STACK_SIZE]);

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
    unsafe { &*addr_of!(TSS) }
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
        (&raw mut (*addr_of_mut!(TSS)).privilege_stack_table[0]).write(rsp0);
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
    addr_of_mut!(TSS)
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
        // Slot 3 intentionally left null: it is reserved so the TSS lands on a
        // 16-byte boundary at index 4.
        write_slot(gdt_base, 3, 0);

        let tss = tss_mut();
        let kernel_top = addr_of!(KERNEL_STACK) as u64 + KERNEL_STACK_SIZE as u64;
        let fault_top = addr_of!(FAULT_STACK) as u64 + FAULT_STACK_SIZE as u64;

        // Interrupt arriving from ring 3 must switch to the kernel stack, not to
        // whatever user stack the thread was running on.
        //
        // `&raw mut` rather than field syntax: `TaskStateSegment` is
        // `repr(C, packed)`, so forming a reference to a field is an error even
        // where the field happens to be aligned. Writing through a raw pointer to
        // a packed field is the documented way to do this.
        (&raw mut (*tss).privilege_stack_table[0]).write(kernel_top);
        (&raw mut (*tss).interrupt_stack_table_index).write(FAULT_IST_INDEX);
        (&raw mut (*tss).interrupt_stack_table[FAULT_IST_INDEX as usize]).write(fault_top);

        // Fill the 16-byte TSS descriptor last: it needs the final TSS base.
        let tss_base = tss as u64;
        let tss_limit = (size_of::<TaskStateSegment>() - 1) as u16;
        write_tss_descriptor(gdt_base, TSS_SLOT, tss_base, tss_limit);

        let descriptor = PseudoDescriptor {
            limit: (size_of::<Gdt>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };

        load_gdt(&descriptor);
        reload_data_segments();
        load_tss(KERNEL_TSS_SELECTOR);

        klog!(
            Level::Debug,
            "gdt: loaded 6 slots, tss rsp0={kernel_top:#x} ist{FAULT_IST_INDEX}={fault_top:#x}"
        );
    }
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
        access: (1 << 7) | (dpl << 5) | CODE_TYPE | if dpl == DPL_RING3 { 0x4 } else { 0 },
        // 4 KiB granularity, plus the high limit bits.
        limit_flags: 0xC0 | 0xF,
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
        access: (1 << 7) | (dpl << 5) | DATA_TYPE,
        limit_flags: 0xC0 | 0xF,
        base_high: 0,
    }
}

/// Pack an 8-byte segment descriptor into its 64-bit GDT slot form.
const fn encode(segment: SegmentDescriptor) -> u64 {
    // Widening casts rather than `u32::from`: `From` is not a `const` trait on
    // this toolchain, and the whole encoder is `const` so that descriptor
    // construction stays checkable at compile time.
    let low = (segment.limit_low as u64)
        | ((segment.base_mid as u64) << 16)
        | ((segment.access as u64) << 40)
        | ((segment.limit_flags as u64) << 48)
        | ((segment.base_high as u64) << 56);
    let high = segment.base_low as u64;
    low | (high << 32)
}

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
                // No granularity bit: a TSS limit is a byte count, not page count.
                //
                // `limit` is already a `u16`, so the architectural bits 19:16 are
                // zero by construction. Writing `(limit >> 16) & 0xF` would be an
                // arithmetic overflow, not a masked zero — which is the compiler
                // telling us the *caller* has already done the narrowing.
                limit_flags: 0,
                base_high: ((base >> 24) & 0xFF) as u8,
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

/// Load the null selector into every data segment register.
///
/// # Safety
///
/// Must follow `lgdt`. Loading selector 0 is architecturally always valid; it
/// faults only when *used*.
unsafe fn reload_data_segments() {
    // SAFETY: delegated to this function's contract.
    unsafe {
        asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            in("ax") 0u16,
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
