//! The architecture facade.
//!
//! # The rule
//!
//! Code outside this module reaches a machine only through the items re-exported
//! here. Inside, per-architecture modules may do whatever the hardware requires.
//! That single rule is what makes a second target possible: adding AArch64 meant
//! writing one backend, not auditing the whole kernel for `#ifdef`.
//!
//! If logic needs to be architecture-specific it goes in a backend behind a named
//! *capability* — [`shared::console::Console`], [`shared::cpu::InterruptControl`],
//! [`shared::exit::MachineExit`] — never as an `#[cfg]` sprinkled through callers.
//! A `cfg` at a call site means the caller knows the architecture, which is
//! exactly the coupling this module exists to prevent.
//!
//! # Current targets
//!
//! | Backend | Target | Boot path | Console | Exit |
//! | --- | --- | --- | --- | --- |
//! | [`x86_64`] | `x86_64-unknown-none` | NovaBoot UEFI → long mode | 16550 @ 0x3f8 | `isa-debug-exit` @ 0xf4 |
//! | [`aarch64`] | `aarch64-unknown-none` | NovaBoot UEFI → EL1 (planned) | PL011 @ 0x0900_0000 | PSCI `SYSTEM_OFF` |
//! | [`riscv64`] | `riscv64gc-unknown-none-elf` | QEMU `virt` + OpenSBI → S-mode | 16550 @ 0x1000_0000 | SBI `SRST` ecall |
//! | [`loongarch64`] | `loongarch64-unknown-none` | QEMU `virt` → direct `_start` | 16550 @ 0x1fe0_01e0 | park the CPU |
//!
//! # Porting status
//!
//! Only x86-64 has the complete kernel: paging, memory management, SMP,
//! services, and the desktop. The two newest ports bring their machines up
//! as far as the banner — the console is programmed and prints, an exception
//! vector is installed, and the CPU parks. Everything the boot sequence
//! *does* after those points is x86-64-only, and `arch/mod.rs` gates it out
//! rather than promising a half-ported machine.
//!
//! # Adding a target
//!
//! 1. Create `arch/<name>/` providing `init`, `ADDRESS_SPACE`, `NAME`, and types
//!    implementing the capabilities in [`shared`].
//! 2. Add a linker script plus a `[target.<triple>]` block in `.cargo/config.toml`.
//! 3. Add one `#[cfg]` arm below. Needing more than one arm means the capability
//!    being added is not architecture-neutral yet — fix that first.

pub mod shared;

use shared::console::Console;
use shared::cpu::InterruptControl;
use shared::exit::MachineExit;
use shared::hw::AddressSpace;

// Exactly one backend is compiled. The `cfg` arms below are the only place in the
// kernel where the target architecture is named; everything else sees these
// re-exports and cannot tell which one it got.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    ADDRESS_SPACE as BACKEND_ADDRESS_SPACE, ConsoleImpl, ExitImpl, Interrupts, NAME, arch_init,
};

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    ADDRESS_SPACE as BACKEND_ADDRESS_SPACE, ConsoleImpl, ExitImpl, Interrupts, NAME, arch_init,
};
#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::{
    ADDRESS_SPACE as BACKEND_ADDRESS_SPACE, ConsoleImpl, ExitImpl, Interrupts, NAME, arch_init,
};
#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::{
    ADDRESS_SPACE as BACKEND_ADDRESS_SPACE, ConsoleImpl, ExitImpl, Interrupts, NAME, arch_init,
};

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
compile_error!(
    "NovaOS supports x86_64, aarch64, riscv64 and loongarch64. Adding a \
     target means implementing the capabilities in `arch::shared` and adding \
     one `cfg` arm in `arch/mod.rs`."
);

/// The executing processor's kernel slot index.
///
/// High-level code must ask the facade rather than name a backend module.
/// x86-64 reads the per-CPU data block the processor's `GS` base points at;
/// aarch64 currently runs single-processor builds and names slot 0.
#[cfg(target_arch = "x86_64")]
pub fn current_cpu() -> usize {
    x86_64::percpu::cpu_index()
}

#[cfg(not(target_arch = "x86_64"))]
pub fn current_cpu() -> usize {
    0
}

/// Geometry of this target's address space.
///
/// Re-exported under its own name rather than by re-exporting the backend's
/// `ADDRESS_SPACE` directly: a `pub use` of the same name as a local item
/// shadows it into a cycle that the const evaluator reports as
/// "cycle detected when checking if `arch::ADDRESS_SPACE` is a trivial const".
/// The alias above breaks the cycle while keeping one spelling for callers.
pub const ADDRESS_SPACE: AddressSpace = BACKEND_ADDRESS_SPACE;

/// Bring the machine into the state the rest of the kernel assumes.
///
/// # Safety
///
/// Called exactly once, from the boot entry point, before interrupts are enabled
/// and before any operation that could fault. A backend must install its
/// exception table before returning, because the very next thing the kernel does
/// is dereference data the bootloader gave us.
pub unsafe fn init() {
    // SAFETY: delegated to this function's contract.
    unsafe { arch_init() };
}

/// Bring up per-CPU table state on an application processor.
///
/// The bootstrap processor has already installed the shared IDT and its own
/// GDT/TSS. This installs the AP's own GDT/TSS, loads the shared IDT, and
/// configures the AP's local interrupt controller.
///
/// # Safety
///
/// Must run on the AP, with interrupts masked, after it has switched to its own
/// kernel stack and after the kernel address space is active.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub unsafe fn init_cpu(_cpu: usize, _direct_map_offset: u64) {
    // No per-CPU tables yet; the single-processor machine has none to install.
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn init_cpu(cpu: usize, direct_map_offset: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe { x86_64::init_cpu(cpu, direct_map_offset) }
}

/// Configure this target's interrupt controller on the bootstrap processor.
///
/// On x86-64 this maps and arms the local APIC; a different backend has a
/// different controller and the same job.
///
/// # Safety
///
/// Must run once on the bootstrap processor, with interrupts masked, after
/// the direct map exists.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub unsafe fn init_interrupt_controller(_direct_map_offset: u64) {
    // Interrupts stay masked until the interrupt-controller driver exists.
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn init_interrupt_controller(direct_map_offset: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe { x86_64::apic::init(direct_map_offset) }
}

/// The stack pointer privilege transitions to `cpu` use.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub fn privilege_stack_for_cpu(_cpu: usize) -> u64 {
    0
}

#[cfg(target_arch = "x86_64")]
pub fn privilege_stack_for_cpu(cpu: usize) -> u64 {
    x86_64::privilege_stack_for_cpu(cpu)
}

/// Wake the application processor in kernel slot `cpu`.
///
/// Returns whether the processor reached its startup path within the
/// backend's bounded waits. The mechanism is the backend's own — INIT–SIPI
/// on x86-64, PSCI `CPU_ON` on AArch64, SBI hart start on RISC-V — but the
/// contract is one line: after this returns `true`, the slot's per-CPU
/// scheduler, tables, and idle thread all exist.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub fn bring_up_application_processor(_cpu: usize) -> bool {
    // No AP trampoline exists yet; the machine runs single-processor.
    false
}

#[cfg(target_arch = "x86_64")]
pub fn bring_up_application_processor(cpu: usize) -> bool {
    x86_64::bring_up_application_processor(cpu)
}

/// Bring up the console. Safe to call more than once.
pub fn console_init() {
    console().init();
}

/// Move the CPU onto the kernel's own stack.
///
/// # Safety
///
/// Interrupts must be masked. The caller's frame is abandoned, so this must be
/// called only from `_start`.
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn switch_to_kernel_stack() {
    // The RISC-V 64 and LoongArch 64 entries switch onto the kernel's own
    // stack in their assembly stubs before any Rust runs; there is no frame
    // for this function to abandon, and there is nothing to do.
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn switch_to_kernel_stack() {
    // SAFETY: delegated to this function's contract.
    unsafe { x86_64::gdt::switch_to_kernel_stack() }
}

/// Move the CPU onto the kernel's own stack and continue boot from there.
///
/// This is the form `_start` uses. It is `-> !` deliberately: `_start` has
/// already built a frame on the bootloader's stack, and abandoning that frame
/// while still unwinding through it later corrupts the return path in a way that
/// surfaces as a fault in an unrelated callee. Returning `!` tells LLVM the
/// frame has no epilogue, so nothing ever reads the abandoned stack back.
///
/// # Safety
///
/// Interrupts must be masked. Called exactly once, from the boot entry point.
/// Does not return.
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn switch_to_kernel_stack_and_continue() -> ! {
    // Unreached by the RISC-V 64 and LoongArch 64 bring-up; the assembly
    // stub enters directly into `bringup_entry` with no x86-style boot flow.
    loop {
        // SAFETY: `wfi`/`idle` are hints with no operands or side effects.
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("wfi");
        }
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn switch_to_kernel_stack_and_continue() -> ! {
    // SAFETY: delegated to this function's contract.
    unsafe { x86_64::gdt::switch_to_kernel_stack_and_continue() }
}
/// The process-wide console.
#[inline]
pub fn console() -> &'static ConsoleImpl {
    &ConsoleImpl
}

/// The interrupt-mask control interface for this target.
#[inline]
pub fn interrupts() -> &'static Interrupts {
    &Interrupts
}

/// Terminate the machine, reporting `status` to the harness.
///
/// # Safety
///
/// Only after the run is over; see [`MachineExit::exit`].
pub unsafe fn exit(status: u32) -> ! {
    // SAFETY: delegated to this function's contract.
    unsafe { ExitImpl::exit(status) }
}

/// Human-readable architecture name for the boot banner.
#[inline]
pub const fn arch_name() -> &'static str {
    NAME
}

/// A monotonic cycle counter for bounding how long a wait has run.
///
/// The rate is the backend's own — callers derive a deadline from it
/// and never present it as time to anyone. As with `init_cpu` and the
/// other machine helpers above, this names the x86 backend directly:
/// the aarch64 backend is still the stub [`aarch64`] describes, and
/// giving it an arm is part of giving it an implementation.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn cycles() -> u64 {
    x86_64::cycles()
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub fn cycles() -> u64 {
    #[cfg(target_arch = "riscv64")]
    {
        return riscv64::cycles();
    }
    #[cfg(target_arch = "loongarch64")]
    {
        return loongarch64::cycles();
    }
    #[cfg(target_arch = "aarch64")]
    {
        return 0;
    }
    #[allow(unreachable_code)]
    0
}

/// Compile-time proof that each backend provides every capability.
///
/// A backend that forgets `Console` or `MachineExit` should fail here, at the
/// facade, where the error message can explain what is missing — not at some
/// distant call site where it explains nothing.
const _: fn() = || {
    fn assert_console<T: Console>() {}
    fn assert_interrupts<T: InterruptControl>() {}
    fn assert_exit<T: MachineExit>() {}

    assert_console::<ConsoleImpl>();
    assert_interrupts::<Interrupts>();
    assert_exit::<ExitImpl>();
};

pub mod capabilities {
    // Each re-export is `pub`, so an "unused import" warning here is a compiler
    // artefact rather than dead code: these are the public spelling of the
    // capability traits, used by callers that must not name a backend.
    //
    // The `unused_imports` lint is switched off for this module *in the lint
    // table* rather than with an inline `#[allow]`, because the project bans
    // inline allow attributes outright. The lint table is the one place a
    // policy belongs; an inline attribute would be the policy being applied at
    // the point of application, which is what the ban exists to prevent.
    pub use super::shared::console::{ByteSink, Console, ConsoleWriter};
    pub use super::shared::cpu::{InterruptControl, MaskGuard};
    pub use super::shared::exit::MachineExit;
    pub use super::shared::fault::{FaultInfo, FaultKind};
    pub use super::shared::hw::AddressSpace;
}

#[cfg(test)]
mod tests;
