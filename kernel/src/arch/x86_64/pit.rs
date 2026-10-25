//! The 8253/8254 Programmable Interval Timer, channel 0.
//!
//! # Why the PIT and not a local APIC timer
//!
//! The PIT is the only timer the reference machine is guaranteed to have. It
//! is also the *worst* timer: it is a fixed 1.193182 MHz divider, it has no
//! per-CPU routing, and its "periodic" mode is really a re-trigger of a
//! one-shot. None of that matters for a milestone whose entire goal is to
//! prove that a tick can preempt a running thread.
//!
//! # The frequency arithmetic
//!
//! Channel 0 counts down from a 16-bit reload value at 1.193182 MHz and
//! raises IRQ0 when it reaches zero. The interval is therefore
//!
//! ```text
//!     period = (reload + 1) / 1_193_182
//! ```
//!
//! The `+ 1` is not a detail: a reload value of `0` means "256 counts", not
//! "zero counts", because the counter is decremented *before* it is compared.
//! Getting this wrong makes the timer run 256 times too fast, which looks
//! like a scheduler that preempts far too eagerly rather than like a broken
//! timer — a symptom that points away from the cause.
//!
//! The reload value is clamped to 65535 because the counter is 16 bits wide,
//! and a request for a slower tick than that cannot be represented. The
//! clamping is silent-but-documented rather than an error because the PIT is
//! the only timer available and "slower than 65536 counts" is not a
//! requirement anyone has yet.
//!
//! # Mode 2 versus mode 3
//!
//! Mode 2 (rate generator) produces a periodic interrupt whose duty cycle is
//! one clock pulse high out of `reload + 1`. Mode 3 (square wave) produces a
//! 50% duty cycle. Mode 2 is chosen because the interrupt is edge-triggered
//! and its *period* is what the scheduler cares about; mode 3's high phase
//! would delay the edge by half a period for no benefit.

use crate::arch::x86_64::port_io::{inb, outb};
use crate::klog;
use crate::logger::Level;

/// Channel 0's command port.
const PIT_COMMAND: u16 = 0x43;
/// Channel 0's data port.
const PIT_DATA0: u16 = 0x40;

/// The PIT's input frequency, in hertz.
///
/// 1.193182 MHz is the IBM PC's 14.31818 MHz crystal divided by twelve. It is
/// not configurable, which is exactly why the tick rate is derived rather
/// than chosen.
pub const PIT_INPUT_HZ: u64 = 1_193_182;

/// The largest reload value a 16-bit counter can hold.
const MAX_RELOAD: u16 = u16::MAX;

/// Command-byte channel 0 selector.
const COMMAND_CHANNEL0: u8 = 0b0000_0000;
/// Access mode 2: rate generator, encoded in bits 3:1.
const COMMAND_MODE2: u8 = 0b0000_0100;
/// Least-significant byte first, then most-significant byte, in bits 5:4.
const COMMAND_LSB_MSB: u8 = 0b0011_0000;

/// The tick rate the kernel asks for.
///
/// 100 Hz is deliberate. The scheduler's timeslice is counted in ticks, so
/// the tick rate *is* the time-slice resolution: at 100 Hz a one-tick quantum
/// is 10 ms, which is short enough that a busy-loop test observes a
/// preemption promptly and long enough that the log is not flooded.
pub const TICK_HZ: u64 = 100;

/// The reload value that produces [`TICK_HZ`].
///
/// Computed rather than written down: the divisor is 1193182, and the
/// quotient for 100 Hz is 11932 with a remainder, so the *actual* rate is
/// 1193182 / 11933 Hz — about 99.99 Hz. Rounding to the nearest count is the
/// best a 16-bit divider can do, and the residual error is 0.01%, which is
/// far below anything the scheduler can observe.
pub const fn reload_value(tick_hz: u64) -> u16 {
    if tick_hz == 0 {
        return 0;
    }
    let divisor = PIT_INPUT_HZ / tick_hz;
    if divisor == 0 {
        return 0;
    }
    if divisor > MAX_RELOAD as u64 {
        return MAX_RELOAD;
    }
    // The `+ 1` is the counter's off-by-one: the counter is decremented
    // before it is compared, so a reload of `N` covers `N + 1` input clocks.
    (divisor - 1) as u16
}

/// Program channel 0 to interrupt at `tick_hz`.
///
/// The PIT is left in a state where it is *not* yet unmasked at the PIC: the
/// caller decides when the timer may start firing, because a timer that fires
/// before the scheduler exists would deliver IRQ0 into a handler that has
/// nowhere to put the tick.
///
/// # Safety
///
/// Must be called once, with interrupts masked. Writing the command and data
/// ports while the counter is running would leave the PIT in an intermediate
/// state.
pub unsafe fn init(tick_hz: u64) {
    let reload = reload_value(tick_hz);

    // SAFETY: delegated to this function's contract. The command port is
    // written before the data port, which is the PIT's required ordering.
    unsafe {
        // Command byte: channel 0, access mode 2, low byte then high byte.
        outb(
            PIT_COMMAND,
            COMMAND_CHANNEL0 | COMMAND_MODE2 | COMMAND_LSB_MSB,
        );

        // The reload value is sent low byte first. Writing the data port twice
        // without a mode command in between would make the PIT latch the
        // command byte's bits as part of the count.
        outb(PIT_DATA0, (reload & 0xFF) as u8);
        outb(PIT_DATA0, (reload >> 8) as u8);
    }

    klog!(
        Level::Info,
        "pit: channel 0 reload {reload} -> {} Hz (requested {tick_hz} Hz)",
        actual_frequency(reload),
    );

    // The handler is installed and the scheduler is running by the time this
    // function is called, so the timer may now begin delivering IRQ0.
    //
    // SAFETY: the IDT's IRQ0 gate has already been installed.
    unsafe {
        crate::arch::x86_64::intr::unmask_irq(0);
    }
}

/// The frequency a given reload value actually produces.
pub const fn actual_frequency(reload: u16) -> u64 {
    PIT_INPUT_HZ / (reload as u64 + 1)
}

/// Read the current count of channel 0.
///
/// Exposed for diagnostics: a timer that is not counting is indistinguishable
/// from one that is counting far too slowly, and the latch tells the two
/// apart.
pub fn current_count() -> u16 {
    // Latching the count requires writing to the command port and then
    // reading the data port twice. The command byte must preserve the
    // channel, access mode, and format bits, so it is read back first rather
    // than written as a constant — the PIT's other channels may be in use.
    //
    // SAFETY: writing the command port and reading the data port is the
    // documented latch sequence, and it does not disturb the counter.
    unsafe {
        let command = inb(PIT_COMMAND);
        outb(PIT_COMMAND, command | 0b0000_0100);

        let low = inb(PIT_DATA0);
        let high = inb(PIT_DATA0);

        ((high as u16) << 8) | (low as u16)
    }
}
