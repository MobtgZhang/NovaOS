//! A memory-mapped 16550 UART, shared by the RISC-V and LoongArch backends.
//!
//! # Why this is shared
//!
//! RISC-V and LoongArch have nothing in common architecturally, but the
//! machines QEMU emulates for them both put a *memory-mapped* 16550 on the
//! bus. The register layout is the PC's 16550 layout; only the base address
//! differs. Sharing the driver means the register offsets are written once
//! and cannot drift between the two backends.
//!
//! x86-64 does **not** use this driver: on a PC the UART is a legacy I/O
//! device at port 0x3f8, reached with `in`/`out`, and dereferencing the port
//! number raises `#PF`. See `arch/x86_64/port_io.rs`.
//!
//! # Where the base addresses come from
//!
//! - RISC-V (`virt` machine): `0x1000_0000`, from QEMU's `hw/riscv/virt.c`
//!   (`[VIRT_UART0] = { 0x10000000, 0x100 }`), installed with
//!   `serial_mm_init`, i.e. a memory-mapped device with a one-byte register
//!   stride. The device tree QEMU generates agrees: the node is
//!   `/soc/serial@10000000`, `compatible = "ns16550a"`, `reg = <0x0 0x10000000
//!   0x0 0x100>`.
//! - LoongArch (`virt` machine): `0x1fe0_01e0`, from QEMU's
//!   `include/hw/loongarch/virt.h` (`#define VIRT_UART_BASE 0x1fe001e0UL`,
//!   `#define VIRT_UART_SIZE 0x100`), again with `serial_mm_init`, and the
//!   device tree node is `/soc/serial@1fe001e0` with `compatible =
//!   "ns16550a"`.
//!
//! Both use the same eight registers, at a one-byte stride:
//!
//! | Offset | Register |
//! | --- | --- |
//! | 0x0 | receive buffer / transmit holding |
//! | 0x1 | interrupt enable / divisor latch high |
//! | 0x2 | FIFO control / interrupt identification |
//! | 0x3 | line control / divisor latch low |
//! | 0x4 | modem control |
//! | 0x5 | line status |
//! | 0x6 | modem status |
//! | 0x7 | scratch |
//!
//! # Memory ordering
//!
//! The device is memory-mapped, so every access is `read_volatile` /
//! `write_volatile`. A plain read would be hoisted out of a loop by the
//! optimiser, and a spin on the transmit-ready bit would then never observe
//! the device changing the register.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::shared::console::Console;

/// Offset of the receive buffer / transmit holding register.
pub const DATA: u64 = 0x0;
/// Offset of the interrupt enable register.
pub const IER: u64 = 0x1;
/// Offset of the FIFO control register.
pub const FCR: u64 = 0x2;
/// Offset of the line control register.
pub const LCR: u64 = 0x3;
/// Offset of the modem control register.
pub const MCR: u64 = 0x4;
/// Offset of the line status register.
pub const LSR: u64 = 0x5;
/// Offset of the modem status register.
pub const MSR: u64 = 0x6;

/// Line status bit 0: a received byte is waiting.
pub const LSR_DATA_READY: u8 = 0x01;
/// Line status bit 5: the transmit holding register is empty.
pub const LSR_THR_EMPTY: u8 = 0x20;

/// Line control bit 7: the divisor latch is visible at offsets 0 and 1.
pub const LCR_DLAB: u8 = 0x80;
/// Line control bits 1:0: 8 data bits, no parity, one stop bit.
pub const LCR_8N1: u8 = 0x03;

/// Modem control bits: DTR, RTS and OUT2.
pub const MCR_DTR_RTS: u8 = 0x0B;

/// FIFO control: enable and clear both FIFOs, 14-byte trigger level.
pub const FCR_ENABLE_CLEAR: u8 = 0xC7;

/// Bound on the transmit-stall spin.
///
/// Comfortably longer than any real transfer time, short enough that a wedged
/// UART costs microseconds rather than hanging the boot.
const TRANSMIT_SPIN_LIMIT: u32 = 100_000;

/// A memory-mapped 16550 at `base`.
#[derive(Clone, Copy)]
pub struct Uart16550 {
    /// Physical address of the register block.
    base: u64,
    /// The divisor to program, derived from the board's UART clock.
    divisor: u16,
}

impl Uart16550 {
    /// A UART at physical address `base`, clocked at `clock_hz`.
    ///
    /// The divisor is the 16550 formula for 115200 baud: `clock / (16 *
    /// baud)`. A clock of zero would produce a zero divisor, so it is clamped
    /// to one; the device then runs at whatever rate the firmware left it at,
    /// which is harmless for a console.
    pub const fn new(base: u64, clock_hz: u64) -> Self {
        let divisor = if clock_hz == 0 {
            1
        } else {
            let raw = clock_hz / (16 * 115_200);
            if raw == 0 { 1 } else { raw as u16 }
        };
        Self { base, divisor }
    }

    /// The physical address of the register block.
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// Address of the register at `offset`.
    #[inline]
    const fn reg(&self, offset: u64) -> u64 {
        self.base + offset
    }

    /// Read one register.
    ///
    /// # Safety
    ///
    /// `offset` must name a register inside the device's register block, and
    /// the block must be mapped.
    #[inline]
    unsafe fn read(&self, offset: u64) -> u8 {
        // SAFETY: delegated to this function's contract.
        unsafe { core::ptr::read_volatile(self.reg(offset) as *const u8) }
    }

    /// Write one register.
    ///
    /// # Safety
    ///
    /// `offset` must name a register inside the device's register block, and
    /// the block must be mapped.
    #[inline]
    unsafe fn write(&self, offset: u64, value: u8) {
        // SAFETY: delegated to this function's contract.
        unsafe { core::ptr::write_volatile(self.reg(offset) as *mut u8, value) }
    }

    /// The line status register, read without disturbing the device.
    ///
    /// Reading LSR has no side effect on a 16550, so this is safe to call
    /// from anywhere, including a diagnostic.
    pub fn line_status(&self) -> u8 {
        // SAFETY: reading LSR is side-effect free and cannot fault, which is
        // exactly the condition the caller must establish.
        unsafe { self.read(LSR) }
    }

    /// Program the device for 8N1 at 115200 baud.
    ///
    /// # Safety
    ///
    /// The register block must be mapped at `base`.
    unsafe fn program(&self) {
        // SAFETY: delegated to this function's contract.
        unsafe {
            // Interrupts off: the kernel polls the device, and an unmasked
            // line would deliver a byte into a driver that is not ready.
            self.write(IER, 0x00);

            // `DLAB` must be set while the divisor latch is visible and cleared
            // afterwards so that offset 0 is the data port again.
            self.write(LCR, LCR_DLAB);
            self.write(DATA, (self.divisor & 0xFF) as u8);
            self.write(IER, (self.divisor >> 8) as u8);
            self.write(LCR, LCR_8N1);

            self.write(FCR, FCR_ENABLE_CLEAR);
            self.write(MCR, MCR_DTR_RTS);
        }
    }
}

/// Set once the UART has been programmed.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Guards against a fault inside a log call re-entering the formatter and
/// interleaving bytes into the outer message.
static IN_CRITICAL: AtomicBool = AtomicBool::new(false);

/// A [`Console`] backed by a memory-mapped 16550.
///
/// The type is a zero-sized handle: the device's address is a compile-time
/// constant of the backend, and the console trait is implemented on the
/// handle rather than on a trait object so the call sites stay statically
/// dispatched.
pub struct MmioConsole {
    /// The device this console drives.
    uart: Uart16550,
}

impl MmioConsole {
    /// A console over the UART at `base`, clocked at `clock_hz`.
    pub const fn new(base: u64, clock_hz: u64) -> Self {
        Self {
            uart: Uart16550::new(base, clock_hz),
        }
    }

    /// The underlying device.
    pub const fn uart(&self) -> &Uart16550 {
        &self.uart
    }
}

impl Console for MmioConsole {
    fn init(&self) {
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return;
        }

        // SAFETY: the device's register block is mapped at the backend's
        // constant base address for the whole lifetime of the machine.
        unsafe { self.uart.program() };
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
            let status = unsafe { self.uart.read(LSR) };
            if status & LSR_THR_EMPTY != 0 {
                break;
            }
            spins += 1;
        }

        // SAFETY: the data register accepts byte writes; bounded by the spin
        // above so a wedged device cannot hang the caller.
        unsafe { self.uart.write(DATA, byte) };

        IN_CRITICAL.store(false, Ordering::Release);
    }

    fn try_recv(&self) -> Option<u8> {
        if !INITIALIZED.load(Ordering::Acquire) {
            return None;
        }

        // SAFETY: reading LSR is side-effect free.
        let status = unsafe { self.uart.read(LSR) };
        if status & LSR_DATA_READY == 0 {
            return None;
        }

        // SAFETY: the receive buffer is only meaningful when LSR says a byte
        // is ready, which was just checked.
        Some(unsafe { self.uart.read(DATA) })
    }
}
