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
//! 1. `_start` switches onto the kernel stack and enters the Rust boot path.
//! 2. [`arch_init`] installs the normal exception and TLB refill vectors.
//! 3. [`verify_privilege_transition`] exercises PLV0 → PLV3 → PLV0 using a
//!    boot-only address space, a controlled syscall, and a one-shot timer.
//!
//! The kernel's full boot sequence (paging, memory map, SMP, services, the
//! desktop) is not yet ported; see the module docs in `arch/mod.rs` and the
//! report for the honest status.

pub mod console;
pub mod entry;
pub mod exit;
mod user;

pub use console::ConsoleImpl;
pub use exit::ExitImpl;

use crate::arch::shared::cpu::InterruptControl;
use crate::arch::shared::hw::AddressSpace;

unsafe extern "C" {
    fn loongarch64_install_trap_vectors();
}

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

/// The boot kernel runs with `CRMD.IE` clear. The timer probe enables it only
/// during its controlled PLV3 loop and returns with it masked again.
pub struct Interrupts;

const CRMD_IE: usize = 1 << 2;

impl InterruptControl for Interrupts {
    fn mask(&self) -> bool {
        let previous: usize;
        unsafe {
            core::arch::asm!("csrxchg {value}, {mask}, 0x0",
                value = inout(reg) 0_usize => previous, mask = in(reg) CRMD_IE, options(nostack));
        }
        previous & CRMD_IE == 0
    }

    fn restore(&self, was_masked: bool) {
        let bits = if was_masked { 0 } else { CRMD_IE };
        unsafe {
            core::arch::asm!("csrxchg {value}, {mask}, 0x0",
                value = inout(reg) bits => _, mask = in(reg) CRMD_IE, options(nostack));
        }
    }

    fn is_masked(&self) -> bool {
        let mode: usize;
        unsafe {
            core::arch::asm!("csrrd {mode}, 0x0", mode = out(reg) mode, options(nostack, nomem));
        }
        mode & CRMD_IE == 0
    }
}

#[unsafe(no_mangle)]
extern "C" fn loongarch64_fatal_trap(estat: u64, era: u64, badv: u64, refill: u64) -> ! {
    crate::klog!(
        crate::logger::Level::Error,
        "trap: fault refill={} estat={:#x} era={:#x} badv={:#x}",
        refill,
        estat,
        era,
        badv
    );
    loop {
        unsafe { core::arch::asm!("idle 0", options(nostack, nomem, preserves_flags)) };
    }
}

/// Run the single-CPU boot probe after the exception vectors are installed.
///
/// A temporary PLV0-only direct window keeps kernel code and the trusted trap
/// stack accessible in paged mode. PLV3 gets one paired TLB entry for an RX
/// trampoline and an RW/NX stack. This address space exists for the probe only.
///
/// The initial syscall returns the cookie written and read by PLV3. Two
/// contexts then use different private stack pages at the same user address.
/// A CPU timer preempts the first context and the second yields back to it.
pub(super) fn verify_privilege_transition(
    frames: &mut crate::arch_bringup::EarlyFrames,
    stack_pages: Option<[usize; 2]>, init: &[crate::early_init::Image],
) -> Result<(), &'static str> {
    user::verify(frames, stack_pages, init)
}

/// Bring the machine into the state the rest of the kernel assumes.
///
/// # Safety
///
/// Must run once, from the boot entry, with interrupts masked.
pub unsafe fn arch_init() {
    unsafe { loongarch64_install_trap_vectors() };
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
