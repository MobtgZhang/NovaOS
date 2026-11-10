//! RISC-V 64 exit: SBI system reset.
//!
//! The test harness turns "the kernel says it is finished" into a process
//! exit status. On an OpenSBI-powered `virt` machine the M-mode firmware
//! owns the machine control state, so the honest mechanism is the SBI
//! System Reset Extension (`SRST`): an `ecall` with the SRST extension ID
//! and a `system_reset` function code. OpenSBI answers the call by
//! powering off the machine or resetting it; QEMU reports the machine's
//! exit to the harness by terminating the emulator process.
//!
//! # Status codes and QEMU
//!
//! QEMU exits with status `0` for every SBI shutdown, so a PASS and a FAIL
//! are indistinguishable from the exit code alone. The distinction is
//! carried by the serial log: the harness knows which suites it ran and
//! reads `[NOVA-TEST] RESULT` out of the console stream. Until the test
//! suites are ported to this machine, `exit` only needs to be a clean
//! terminator, and it is exactly that.

use crate::arch::shared::exit::MachineExit;

/// SBI extension ID for the System Reset Extension.
///
/// ASCII `SRST` — the extension is named after the ASCII codes of
/// `S`, `R`, `S`, `T`.
const SBI_EXT_SRST: u64 = 0x5352_5354;
/// SRST function: system reset.
const SBI_SRST_SYSTEM_RESET: u64 = 0;
/// SRST reset type: shutdown.
const SBI_SRST_TYPE_SHUTDOWN: u64 = 0;
/// SRST reset type: cold reboot.
const SBI_SRST_TYPE_COLD_REBOOT: u64 = 1;
/// SRST reason: none.
const SBI_SRST_REASON_NONE: u64 = 0;

pub struct ExitImpl;

impl MachineExit for ExitImpl {
    /// # Safety
    ///
    /// Only appropriate once the run is over. The SBI reset extension
    /// terminates the machine rather than returning; a firmware without
    /// the extension returns from the `ecall` and the loop below parks
    /// the CPU, which is the only safe answer a failed reset can take.
    unsafe fn exit(status: u32) -> ! {
        let (reset_type, reason) = if status == 0 {
            (SBI_SRST_TYPE_SHUTDOWN, SBI_SRST_REASON_NONE)
        } else {
            (SBI_SRST_TYPE_COLD_REBOOT, SBI_SRST_REASON_NONE)
        };

        // SAFETY: `ecall` with SRST is a firmware call; registers a0/a1/a6/a7
        // are the call's inputs and the firmware does not return on success.
        // The `ecall` instruction has no data memory operands, so the compiler
        // must not fold any of the inputs into a larger instruction.
        unsafe {
            core::arch::asm!(
                "ecall",
                in("a7") SBI_EXT_SRST,
                in("a6") SBI_SRST_SYSTEM_RESET,
                in("a0") reset_type,
                in("a1") reason,
                options(nostack, nomem, preserves_flags),
            );
        }

        // A firmware without SRST returns here instead of resetting; the only
        // safe thing left is to sleep forever with interrupts masked.
        loop {
            // SAFETY: `wfi` is a hint with no operands or side effects.
            unsafe { core::arch::asm!("wfi") };
        }
    }
}
