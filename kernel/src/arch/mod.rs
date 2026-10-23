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
//! | [`x86_64`] | `x86_64-unknown-none` | Limine BIOS/UEFI → long mode | 16550 @ 0x3f8 | `isa-debug-exit` @ 0xf4 |
//! | [`aarch64`] | `aarch64-unknown-none` | Limine UEFI → EL1 | PL011 @ 0x0900_0000 | PSCI `SYSTEM_OFF` |
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

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "NovaOS supports x86_64 and aarch64. Adding a target means implementing the \
     capabilities in `arch::shared` and adding one `cfg` arm in `arch/mod.rs`."
);

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

/// Bring up the console. Safe to call more than once.
pub fn console_init() {
    console().init();
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
    #[allow(unused_imports)]
    pub use super::shared::console::{ByteSink, Console, ConsoleWriter};
    #[allow(unused_imports)]
    pub use super::shared::cpu::{InterruptControl, MaskGuard};
    #[allow(unused_imports)]
    pub use super::shared::exit::MachineExit;
    #[allow(unused_imports)]
    pub use super::shared::fault::{FaultInfo, FaultKind};
    #[allow(unused_imports)]
    pub use super::shared::hw::AddressSpace;
}

#[cfg(test)]
mod tests;
