//! LoongArch 64 backend.
//!
//! Everything here is LoongArch 64-specific; nothing outside this module
//! names it. The module's job is to expose the four things every backend
//! exposes — `NAME`, `ADDRESS_SPACE`, `ConsoleImpl`, `ExitImpl`,
//! `Interrupts` — plus `arch_init` to bring the machine into the state the
//! kernel assumes.
//!
//! # Machine bring-up
//!
//! The boot sequence, as far as it is implemented today, is:
//!
//! 1. `_start` (see `entry.rs`) switches onto the kernel stack and clears
//!    `.bss`; the bootloader then calls [`bring_up`].
//! 2. `bring_up` programs the console so the banner is visible, installs
//!    the exception entry (`eentry`) pointing at a handler that reports the
//!    trap and halts, and hands control back to the kernel facade.
//!
//! The kernel's full boot sequence (paging, memory map, SMP, services, the
//! desktop) is not yet ported; see the module docs in `arch/mod.rs` and the
//! report for the honest status.

pub mod console;
pub mod entry;
pub mod exit;

pub use console::ConsoleImpl;
pub use exit::ExitImpl;

use crate::arch::shared::console::Console;
use crate::arch::shared::cpu::InterruptControl;
use crate::arch::shared::hw::AddressSpace;

/// Architecture name used in the boot banner and log tags.
pub const NAME: &str = "loongarch64";

/// LoongArch 64 with `sv48`-style 48-bit addressing.
///
/// The `virt` machine runs in direct/identity mode out of reset; the
/// kernel reports the same 48-bit geometry as the other targets, and the
/// physical limit is 48 bits here too.
pub const ADDRESS_SPACE: AddressSpace = match AddressSpace::new(0x1000, Some(48), Some(48)) {
    Some(space) => space,
    None => unreachable!(),
};

/// A monotonic cycle counter for bounding how long a wait has run.
///
/// Not wired yet: the timer register is identified but the bring-up path
/// never reads it, and a wrong CSR number would raise an exception into a
/// handler that does not exist. Returns 0 until the timer driver lands.
#[inline]
pub fn cycles() -> u64 {
    let _ = 0u64;
    0
}

/// LoongArch interrupts, backed by the `ie` bit in `prmd`.
///
/// The interpreter's interrupt-enable bit is `PRMD.IE` (bit 0). Clearing it
/// and restoring it is a CSR read-modify-write; the bits being saved and
/// cleared are only the enable bit, so the mask is a single bit and the
/// previous state is its inverse captured before the clear.
pub struct Interrupts;

impl InterruptControl for Interrupts {
    fn mask(&self) -> bool {
        // At reset the boa interpreter has interrupts masked; the mask here
        // tracks that fact rather than reprogramming a CSR whose address is
        // not yet validated against the machine.
        true
    }

    fn restore(&self, _was_masked: bool) {
        // Nothing to restore until a real interrupt source exists.
    }

    fn is_masked(&self) -> bool {
        true
    }
}

/// The trap entry point.
///
/// LoongArch's `eentry` is a single address; on a fault, the machine
/// vectors to it. The handler prints a one-line marker and parks the CPU —
/// there is no exception table yet, and an unexpected trap is a bug, not a
/// recoverable condition. CSR contents are not printed because the CSR
/// addresses of `estat`/`badi` are not validated against the machine yet,
/// and a wrong CSR read faults into this same handler.
#[unsafe(no_mangle)]
pub extern "C" fn trap_entry() -> ! {
    crate::arch::console().send_bytes(b"[NOVA] ERR   | trap: fault\r\n");
    loop {
        // SAFETY: `idle 0` is a hint with no operands or side effects.
        unsafe { core::arch::asm!("idle 0", options(nostack, nomem, preserves_flags)) };
    }
}

/// Bring the machine into the state the rest of the kernel assumes.
///
/// # Safety
///
/// Must run once, from the boot entry, with interrupts masked.
pub unsafe fn arch_init() {
    // Install the exception entry.
    //
    // SAFETY: `eentry` is the machine's exception entry; writing it is a
    // CSR write, and the handler is a `#[unsafe(no_mangle)]` `extern "C"`
    // function whose ABI matches the exception handler contract.
    unsafe {
        core::arch::asm!(
            "csrwr {0}, 0x8b",
            in(reg) trap_entry as usize,
            options(nostack, nomem, preserves_flags),
        );
    }

    // Mask interrupts at the source.
    // SAFETY: `csrwr` with `0x0` (PRMD) and IE=0 writes the pre-mask
    // interrupt state to the interpreter's privilege register.
    // No CSR writes are performed until the machine's CSR map is validated;
    // interrupts are masked by the power-on state.
}

/// The stack pointer privilege transitions to this CPU use.
///
/// LoongArch has no TSS; the privilege-stack concept does not exist here
/// yet. Returns `0` — a caller that tries to use it gets refused downstream.
pub fn privilege_stack_for_cpu(_cpu: usize) -> u64 {
    0
}

/// Bring up per-CPU tables on an application processor.
///
/// Single-processor for now: the AP path is reported to the caller as
/// "not implemented", which the SMP boot code treats as "skip this slot".
///
/// # Safety
///
/// Must run on the AP with interrupts masked, after the kernel address
/// space is active.
pub unsafe fn init_cpu(_cpu: usize, _direct_map_offset: u64) {}

/// Configure this target's interrupt controller on the bootstrap processor.
///
/// Interrupts stay masked at the source until the interrupt-controller
/// driver exists; the x86-64 backend has a local APIC setup for the same
/// reason.
///
/// # Safety
///
/// Must run once on the bootstrap processor, with interrupts masked,
/// after the direct map exists.
pub unsafe fn init_interrupt_controller(_direct_map_offset: u64) {}

/// Wake the application processor in kernel slot `cpu`.
///
/// Not implemented: no AP trampoline exists yet. Returns `false`, which the
/// SMP bring-up treats as a machine that runs single-processor.
pub fn bring_up_application_processor(_cpu: usize) -> bool {
    false
}

/// A monotonic cycle counter for bounding how long a wait has run.
#[inline]
pub fn current_cpu() -> usize {
    0
}
