//! The console backed by this port's memory-mapped 16550.
//!
//! The device is the same one x86-64 emulates behind port I/O, reached
//! through memory-mapped registers here; sharing the driver with the other
//! bare-metal targets means the register offsets cannot drift.

use crate::arch::shared::console::Console;
use crate::arch::shared::uart16550::MmioConsole;

/// The physical address of UART0 on the machine.
pub const UART_BASE: u64 = 0x1000_0000;

/// The clock the machine wires to the UART.
pub const UART_CLOCK_HZ: u64 = 3_686_400;

/// The console handle.
///
/// A zero-sized type; the [`Console`] implementation delegates to a const
/// [`MmioConsole`], so the device's base address is a compile-time constant
/// of the backend and call sites stay statically dispatched.
pub struct ConsoleImpl;

impl Console for ConsoleImpl {
    fn init(&self) {
        MmioConsole::new(UART_BASE, UART_CLOCK_HZ).init();
    }

    fn send_byte(&self, byte: u8) {
        MmioConsole::new(UART_BASE, UART_CLOCK_HZ).send_byte(byte);
    }

    fn send_bytes(&self, bytes: &[u8]) {
        MmioConsole::new(UART_BASE, UART_CLOCK_HZ).send_bytes(bytes);
    }

    fn try_recv(&self) -> Option<u8> {
        MmioConsole::new(UART_BASE, UART_CLOCK_HZ).try_recv()
    }
}
