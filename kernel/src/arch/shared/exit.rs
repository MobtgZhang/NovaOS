//! Emulator/debug exit.
//!
//! How a kernel tells "I am finished" to a test harness is architecture-neutral
//! in intent and wildly different in mechanism, which makes it the clearest
//! example of why the facade exists:
//!
//! | Architecture | Mechanism | QEMU device | Guest status seen by the harness |
//! | --- | --- | --- | --- |
//! | x86-64 | `out dx, eax` to port 0xf4 | `isa-debug-exit` | `(value << 1) \| 1` |
//! | AArch64 | PSCI `SYSTEM_OFF` via HVC | built into `virt` | the QEMU exit code |
//!
//! Both must land on the *same* status in `scripts/qemu.sh`, so the encoding lives
//! here and each backend supplies only the mechanism.

/// Exit status meaning "all tests passed".
pub const EXIT_SUCCESS: u32 = 0x00;

/// Exit status meaning "the kernel panicked".
pub const EXIT_PANIC: u32 = 0x10;

/// Exit status meaning "at least one kernel test failed".
pub const EXIT_TEST_FAILURE: u32 = 0x20;

/// The mechanism for terminating the machine.
///
/// Implemented once per architecture. The statuses above are the *interface*;
/// a backend translates them into whatever its emulator observes.
pub trait MachineExit {
    /// Terminate the machine, reporting `status`.
    ///
    /// # Safety
    ///
    /// Only appropriate once the run is over — from the test harness on
    /// completion, or from the panic handler. On a backend with no exit device
    /// this must fall back to halting the CPU rather than returning, because
    /// resuming a machine that has just declared itself finished is undefined
    /// behaviour and would corrupt the harness's exit status.
    ///
    /// # Panics
    ///
    /// Never. A backend with no exit device spins forever.
    unsafe fn exit(status: u32) -> !;
}