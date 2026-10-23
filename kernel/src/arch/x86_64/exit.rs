//! x86-64 machine exit: QEMU's `isa-debug-exit` device.
//!
//! # The `iobase` trap
//!
//! The device's base port is whatever `iobase=` says on the QEMU command line;
//! `scripts/qemu.sh` uses `iobase=0xf4`, so the port to write *is* 0xf4. Writing
//! the device's other common default, 0x501, does not exit: QEMU keeps running
//! until the harness timeout, and a passing test suite looks like a hang.
//!
//! # Status encoding
//!
//! QEMU maps the written value to the guest exit status as `(value << 1) | 1`, so
//! the harness sees an odd status for every value. The [`shared`] exit codes are
//! therefore chosen knowing that QEMU, not the kernel, does the shifting.

use core::arch::asm;

use crate::arch::shared::exit::{EXIT_PANIC, MachineExit};

/// QEMU's `isa-debug-exit` port, matching `iobase=0xf4` on the command line.
const DEBUG_EXIT_PORT: u16 = 0xF4;

/// The x86-64 exit mechanism.
pub struct ExitImpl;

impl MachineExit for ExitImpl {
    unsafe fn exit(status: u32) -> ! {
        // SAFETY: delegated to this function's contract. `out` to the
        // isa-debug-exit port is the documented way to halt the VM; the value
        // only influences QEMU's exit status.
        unsafe {
            asm!(
                "out dx, eax",
                in("dx") DEBUG_EXIT_PORT,
                in("eax") status,
                options(nostack, preserves_flags),
            );
        }

        // The device is only wired up when `xtask run` passes it, so a machine
        // booted without it would otherwise spin here forever. Spinning is
        // deliberate: returning would execute with no defined exit status, and
        // QEMU's `-no-reboot` would then report success for a dead kernel.
        loop {
            core::hint::spin_loop();
        }
    }
}

/// The port this backend writes to, exposed for the boot self-test.
pub const PORT: u16 = DEBUG_EXIT_PORT;

/// Status the harness observes for a kernel panic.
pub const OBSERVED_PANIC_STATUS: u32 = (EXIT_PANIC << 1) | 1;