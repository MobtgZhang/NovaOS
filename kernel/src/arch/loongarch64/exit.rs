//! LoongArch 64 exit: park the CPU forever.
//!
//! The `virt` machine has no `isa-debug-exit` device and no SBI, and a bare
//! `-kernel` boot gives the kernel no firmware call that means "terminate
//! the machine". The honest answer to "the run is over" is to stop
//! executing new work and wait; the test harness terminates the emulator
//! and treats the log's `[NOVA-TEST] RESULT` line as the source of truth.
#![allow(dead_code)]

use crate::arch::shared::exit::MachineExit;

/// Terminate the machine, or park it forever if there is no better way.
///
/// # Safety
///
/// Only appropriate once the run is over. The `idle` loop is a hint; the
/// machine cannot reach any further work from here, so the harness owns
/// termination.
pub struct ExitImpl;

impl MachineExit for ExitImpl {
    unsafe fn exit(_status: u32) -> ! {
        loop {
            // SAFETY: `idle 0` is a hint with no operands or side effects.
            unsafe { core::arch::asm!("idle 0", options(nostack, nomem, preserves_flags)) };
        }
    }
}
