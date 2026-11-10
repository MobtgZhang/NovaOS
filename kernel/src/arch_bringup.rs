//! The RISC-V 64 and LoongArch 64 bring-up.
//!
//! # The honest status
//!
//! NovaOS's full boot sequence — the UEFI NovaBoot boundary, the memory map,
//! paging, SMP bring-up, services, and the desktop — is ported only to
//! x86-64. The two newest ports bring their machines up as far as a bare
//! kernel can be observed to come up: the console is programmed, the banner
//! is printed, an exception vector is installed, and the CPU is parked.
//!
//! Everything documented as ported on x86-64 is **not** ported here yet:
//! no paging, no memory manager, no SMP bring-up, no initramfs, no services,
//! no desktop. The `NovaOS` banner is printed so the console path is
//! verifiable end to end, and that is exactly the work that is done.

use crate::arch;
use crate::arch::capabilities::Console;

/// The entry point both architectures call from their `_start` stub.
///
/// # Safety
///
/// Reached only from the assembly stub with interrupts masked, on the
/// kernel's own stack, with no allocator and no services running.
#[unsafe(no_mangle)]
pub extern "C" fn bringup_entry() -> ! {
    // The console goes first: everything after this prints, and a console
    // that is not initialised silently drops bytes.
    crate::arch::console_init();

    let mut sink = crate::arch::capabilities::ByteSink::<256>::new();
    use core::fmt::Write as _;
    let _ = write!(sink, "[NOVA] INFO  | NovaOS {} boot\r\n", arch::arch_name());
    let _ = write!(
        sink,
        "[NOVA] INFO  | arch: {} page {} B\r\n",
        arch::arch_name(),
        arch::ADDRESS_SPACE.page_size
    );
    let _ = write!(
        sink,
        "[NOVA] INFO  | abi: {}.{} ({} syscalls)\r\n",
        nova_abi::ABI_VERSION.major,
        nova_abi::ABI_VERSION.minor,
        nova_abi::nr::ALL.len()
    );
    arch::console().send_bytes(sink.as_bytes());

    // Install the trap vector. The handler parks the CPU with a one-line
    // report; a fault before paging exists is a bug, not a condition to
    // recover from.
    //
    // SAFETY: the entry contract hands execution over exactly once, with
    // interrupts masked; `arch_init` may assume that.
    unsafe { arch::init() };

    arch::console().send_bytes(b"[NOVA] INFO  | bring-up complete; halting\r\n");

    // Nothing to do: park the machine forever.
    loop {
        #[cfg(target_arch = "riscv64")]
        // SAFETY: `wfi` is a hint with no operands or side effects.
        unsafe {
            core::arch::asm!("wfi");
        }
        #[cfg(target_arch = "loongarch64")]
        // SAFETY: `idle 0` is a hint with no operands or side effects.
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}
