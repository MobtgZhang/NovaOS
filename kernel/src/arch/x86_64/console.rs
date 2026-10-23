//! x86-64 console: a 16550 UART reached through port IO.
//!
//! # Why port IO and never MMIO
//!
//! On a PC the UART is a *legacy I/O* device at 0x3f8, not a memory-mapped
//! range. Dereferencing 0x3f8 raises `#PF` with `CR2 = 0x3f8`; taken before the
//! IDT is installed that becomes a triple fault and the machine resets with no
//! output whatsoever. Every access below therefore goes through
//! [`crate::arch::x86_64::port_io`], which is the only module in the kernel
//! permitted to issue a bare `in`/`out`.
//!
//! See `docs/adr/0001-boot.md`.
//!
//! # Re-entrancy
//!
//! A fault inside a `write!`, or a panic raised by the logger while the logger is
//! mid-message, re-enters [`Console::send_byte`]. Two guards make that survivable:
//! an initialisation flag (a print before `init` is dropped, not a fault) and a
//! bounded transmit spin (a wedged UART degrades to a dropped byte instead of a
//! hung machine with no diagnostics).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::shared::console::Console;
use crate::arch::x86_64::port_io;

/// Data port: write a byte here to transmit.
const COM1: u16 = 0x3F8;

/// Interrupt enable register.
const IER: u16 = COM1 + 1;
/// Divisor latch low byte.
const DLL: u16 = COM1 + 0;
/// Divisor latch high byte.
const DLM: u16 = COM1 + 1;
/// FIFO control register.
const FCR: u16 = COM1 + 2;
/// Line control register.
const LCR: u16 = COM1 + 3;
/// Modem control register.
const MCR: u16 = COM1 + 4;
/// Line status register.
const LSR: u16 = COM1 + 5;

/// LSR bit 0: data ready.
const LSR_DR: u8 = 0x01;
/// LSR bit 5: transmit holding register empty.
const LSR_THRE: u8 = 0x20;
/// LCR bit 7: enable divisor latch access.
const LCR_DLAB: u8 = 0x80;
/// LCR bits 1:0: 8 data bits, no parity, one stop bit.
const LCR_8N1: u8 = 0x03;

/// Divisor for 115200 baud from a 1.8432 MHz clock: 115200 / 115200 = 1.
const DIVISOR: u16 = 1;

/// Bound on the transmit-stall spin.
///
/// Comfortably longer than any real transfer time, short enough that a wedged
/// UART costs microseconds rather than hanging the boot.
const TRANSMIT_SPIN_LIMIT: u32 = 100_000;

/// Set once [`ConsoleImpl::init`] has programmed the UART.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Guards against a fault inside a log call re-entering the formatter and
/// interleaving bytes into the outer message.
static IN_CRITICAL: AtomicBool = AtomicBool::new(false);

/// The x86-64 console.
pub struct ConsoleImpl;

impl Console for ConsoleImpl {
    fn init(&self) {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return;
        }

        // SAFETY: writing IER disables UART interrupts. The kernel polls instead
        // of taking them, and an unmasked line during initialisation would
        // deliver a byte into a driver that is not yet ready for it.
        unsafe { port_io::outb(IER, 0x00) };

        // SAFETY: the standard 16550 bring-up sequence. `DLAB` must be set while
        // the divisor latch is visible and cleared afterwards so COM1 is the data
        // port again.
        unsafe {
            port_io::outb(LCR, LCR_DLAB);
            port_io::outb(DLL, (DIVISOR & 0xFF) as u8);
            port_io::outb(DLM, (DIVISOR >> 8) as u8);
            port_io::outb(LCR, LCR_8N1);
            // Enable and clear FIFOs, 14-byte trigger level.
            port_io::outb(FCR, 0xC7);
            // DTR | RTS | OUT2. OUT2 must be high for the UART to raise an IRQ at
            // all; without it an interrupt-driven console would never fire.
            port_io::outb(MCR, 0x0B);
        }
    }

    fn send_byte(&self, byte: u8) {
        // A print before `init` is dropped rather than faulting. This is what
        // makes the panic path safe: it may run before the console exists.
        if !INITIALIZED.load(Ordering::Acquire) {
            return;
        }

        // Drop rather than interleave if this is a re-entry from a fault raised
        // inside the outer call's formatting.
        if IN_CRITICAL.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut spins: u32 = 0;
        while spins < TRANSMIT_SPIN_LIMIT {
            // SAFETY: reading LSR is side-effect free on a 16550.
            let status = unsafe { port_io::inb(LSR) };
            if status & LSR_THRE != 0 {
                break;
            }
            spins += 1;
        }

        // SAFETY: the data port accepts byte writes; bounded by the spin above so
        // a wedged device cannot hang the caller.
        unsafe { port_io::outb(COM1, byte) };

        IN_CRITICAL.store(false, Ordering::Release);
    }

    fn try_recv(&self) -> Option<u8> {
        if !INITIALIZED.load(Ordering::Acquire) {
            return None;
        }

        // SAFETY: reading LSR is side-effect free.
        let status = unsafe { port_io::inb(LSR) };
        if status & LSR_DR == 0 {
            return None;
        }

        // SAFETY: the receive buffer is only meaningful when LSR says a byte is
        // ready, which was just checked.
        Some(unsafe { port_io::inb(COM1) })
    }
}

/// Read the line status register without disturbing the console.
///
/// Exposed for the boot self-test, which needs to distinguish "a serial device
/// exists" from "nothing is wired to 0x3f8" by looking for the all-ones
/// signature an unclaimed floating bus returns.
///
/// # Safety
///
/// None. Reading LSR has no side effect on a 16550 and cannot fault, so this is
/// safe to call from anywhere — including a test.
pub fn line_status() -> u8 {
    // SAFETY: reading LSR has no side effect and cannot fault, which is exactly
    // the condition `inb` asks the caller to establish.
    unsafe { port_io::inb(LSR) }
}