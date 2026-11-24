//! QEMU virt powers off through the ACPI GED after a successful test.
//! The serial result remains authoritative. The GED cannot encode failure
//! statuses, so failures park for the harness to collect the diagnostic.
#![allow(dead_code)]

use crate::arch::shared::exit::{EXIT_SUCCESS, MachineExit};

const GED_SLEEP_CONTROL: *mut u8 = 0x100e_001c as *mut u8;
const GED_REQUEST_S5: u8 = (5 << 2) | (1 << 5);

/// Terminate the machine, or park it forever if there is no better way.
///
/// # Safety
///
/// The boot probe restores the QEMU virt direct address space before exit.
pub struct ExitImpl;

impl MachineExit for ExitImpl {
    unsafe fn exit(status: u32) -> ! {
        if status == EXIT_SUCCESS {
            unsafe { GED_SLEEP_CONTROL.write_volatile(GED_REQUEST_S5) };
        }
        loop {
            unsafe { core::arch::asm!("idle 0", options(nostack, nomem, preserves_flags)) };
        }
    }
}
