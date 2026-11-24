//! RISC-V 64 backend.
//!
//! Everything here is RISC-V 64-specific; nothing outside this module names
//! it. The module's job is to expose the four things every backend exposes
//! — `NAME`, `ADDRESS_SPACE`, `ConsoleImpl`, `ExitImpl`, `Interrupts` — plus
//! `arch_init` to bring the machine into the state the kernel assumes.
//!
//! # Machine bring-up
//!
//! `_start` (see [`entry`]) switches onto the kernel stack and enters the
//! shared bring-up. [`arch_init`] installs the fatal trap vector and masks
//! supervisor interrupts. [`verify_privilege_transition`] can then run a
//! user-mode syscalls and timer preemption in four-level Sv48 address spaces.

pub mod console;
pub mod entry;
pub mod exit;
mod privilege;
mod user;

pub use console::ConsoleImpl;
pub use exit::ExitImpl;

use crate::arch::shared::console::Console;
use crate::arch::shared::cpu::InterruptControl;
use crate::arch::shared::hw::AddressSpace;

/// Architecture name used in the boot banner and log tags.
pub const NAME: &str = "riscv64";

/// Address-space geometry reported for RISC-V `virt` with Sv48 support.
///
/// The `virt` machine (QEMU's `hw/riscv/virt.c`) can run with `sv48`,
/// giving 56 physical bits and 48 virtual bits; the kernel reports the
/// same geometry as x86-64's 4-level paging so the rest of the tree can
/// share the limit arithmetic unchanged.
pub const ADDRESS_SPACE: AddressSpace = match AddressSpace::new(0x1000, Some(56), Some(48)) {
    Some(space) => space,
    None => unreachable!(),
};

/// A monotonic cycle counter for bounding how long a wait has run.
///
/// `rdcycle` is a wall-time-adjacent counter. It is not a wall clock and
/// nothing converts it to seconds; a deadline derived from it is only
/// ever compared against itself.
#[inline]
pub fn cycles() -> u64 {
    let cycle: u64;
    // SAFETY: `rdcycle` reads the machine-timer CSRs; RISC-V defines them for
    // every implementation with the Zicntr extension, which the virt machine has.
    unsafe {
        core::arch::asm!("rdcycle {0}", out(reg) cycle, options(nostack, nomem, preserves_flags));
    }
    cycle
}

/// RISC-V interrupts, backed by the `S-mode` interrupt-enable bit.
///
/// The bit lives in `sstatus` at bit 1 (`SIE`); reading and writing it is
/// an `csrrs`/`csrrc` pair whose `csrr` form is the standard read-and-set
/// (`csrsi`/`csrci`), with the mask variant `sstatus.SIE = 0x2`. The mask
/// is a single bit, so the previous state is simply its inverse captured
/// before the clear — there is no other interrupt mask to save.
pub struct Interrupts;

/// The `sstatus.SIE` bit.
const SSTATUS_SIE: u64 = 0x2;

impl InterruptControl for Interrupts {
    fn mask(&self) -> bool {
        let previous: u64;
        // SAFETY: `csrrc` writes `SSTATUS.SIE` and returns the previous value;
        // it has no operands other than the CSR name and the clear mask.
        unsafe {
            core::arch::asm!(
                "csrrc {0}, sstatus, {1}",
                out(reg) previous,
                in(reg) SSTATUS_SIE,
                options(nostack, nomem, preserves_flags),
            );
        }
        previous & SSTATUS_SIE == 0
    }

    fn restore(&self, was_masked: bool) {
        if was_masked {
            // Re-mask: clear SIE again.
            // SAFETY: `csrrc` writes `SSTATUS.SIE` with no other operands.
            unsafe {
                core::arch::asm!(
                    "csrrc x0, sstatus, {0}",
                    in(reg) SSTATUS_SIE,
                    options(nostack, nomem, preserves_flags),
                );
            }
        } else {
            // Unmask: set SIE.
            // SAFETY: `csrrs` writes `SSTATUS.SIE` with no other operands.
            unsafe {
                core::arch::asm!(
                    "csrrs x0, sstatus, {0}",
                    in(reg) SSTATUS_SIE,
                    options(nostack, nomem, preserves_flags),
                );
            }
        }
    }

    fn is_masked(&self) -> bool {
        let sstatus: u64;
        // SAFETY: `csrr` reads `sstatus` with no operands or side effects.
        unsafe {
            core::arch::asm!(
                "csrr {0}, sstatus",
                out(reg) sstatus,
                options(nostack, nomem, preserves_flags),
            );
        }
        sstatus & SSTATUS_SIE == 0
    }
}

/// Report an unexpected trap and park the hart.
#[unsafe(no_mangle)]
pub extern "C" fn trap_entry() -> ! {
    let (cause, pc, address): (usize, usize, usize);
    unsafe {
        core::arch::asm!(
            "csrr {cause}, scause",
            "csrr {pc}, sepc",
            "csrr {address}, stval",
            cause = out(reg) cause,
            pc = out(reg) pc,
            address = out(reg) address,
            options(nostack, nomem),
        );
    }
    panic!("RISC-V trap at {pc:#x}: scause={cause:#x}, stval={address:#x}");
}

core::arch::global_asm!(
    ".pushsection .text.nova_riscv64_fatal_vector, \"ax\", @progbits",
    ".balign 4",
    ".global nova_riscv64_fatal_vector",
    "nova_riscv64_fatal_vector:",
    "tail {fatal}",
    ".popsection",
    fatal = sym trap_entry,
);

unsafe extern "C" {
    fn nova_riscv64_fatal_vector();
}

/// Bring the machine into the state the rest of the kernel assumes.
///
/// Keep supervisor interrupts masked at the source and install the direct
/// fatal vector for supervisor boot code.
///
/// # Safety
///
/// Must run once, from the boot entry, with interrupts masked.
pub unsafe fn arch_init() {
    // SAFETY: Boot runs in S-mode on the kernel stack. The assembly vector
    // is four-byte aligned for direct mode and tails into the fatal handler.
    unsafe {
        core::arch::asm!(
            "csrc sstatus, {sie_bit}",
            "csrw sie, zero",
            "csrw stvec, {vector}",
            sie_bit = in(reg) SSTATUS_SIE,
            vector = in(reg) nova_riscv64_fatal_vector as *const (),
            options(nostack, nomem, preserves_flags),
        );
    }
}

/// Exercise U-mode syscalls, timer preemption and context resume on QEMU `virt`.
///
/// Run after [`arch_init`] on the boot stack with paging disabled.
pub(super) fn verify_privilege_transition(
    frames: &mut crate::arch_bringup::EarlyFrames,
    stack_pages: Option<[usize; 2]>, init: &[crate::early_init::Image],
) -> Result<(), &'static str> {
    privilege::verify(frames, stack_pages, init)
}

/// The stack pointer privilege transitions to this CPU use.
///
/// RISC-V has no TSS; the privilege-stack concept does not exist here yet.
/// Returns `0` — a caller that tries to use it gets it refused downstream.
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
/// The `virt` machine has a PLIC; until the PLIC driver exists, interrupts
/// stay masked at the source (`sie` is cleared in [`arch_init`]).
///
/// # Safety
///
/// Must run once on the bootstrap processor, with interrupts masked,
/// after the direct map exists.
pub unsafe fn init_interrupt_controller(_direct_map_offset: u64) {}

/// Wake the application processor in kernel slot `cpu`.
///
/// Not implemented: no AP trampoline exists yet. Returns `false`, which
/// the SMP bring-up treats as a machine that runs single-processor.
pub fn bring_up_application_processor(_cpu: usize) -> bool {
    false
}

/// A monotonic cycle counter for bounding how long a wait has run.
#[inline]
pub fn current_cpu() -> usize {
    0
}
