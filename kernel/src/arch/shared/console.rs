//! Arch-neutral console interface.
//!
//! The kernel prints from a dozen places — boot banner, logs, fault reports,
//! test harness — and none of them may know whether the machine underneath is an
//! x86 PC with a 16550 UART or an ARM board with a PL011. They talk to the
//! [`Console`] trait instead, and [`crate::arch`] picks the implementation.
//!
//! # Why a trait rather than a function pointer table
//!
//! A `static` dispatch through a trait object would work too, but the fault and
//! panic paths need a *bound* guarantee: they must be able to emit bytes even if
//! the console was never initialised, because initialising it is itself the kind
//! of code that can fault. The trait's methods are therefore total — they never
//! return an error and never assume `init` ran — and each implementation
//! decides what "not initialised" means (on both platforms it means "drop the
//! byte", which turns a premature print into a no-op rather than a fault).
//!
//! # Interrupt safety
//!
//! [`Console::send_byte`] may be called from an interrupt handler, a panic, or a
//! fault reporter, potentially re-entered by a fault that happened inside a
//! previous print. Implementations must therefore tolerate re-entrancy; see the
//! re-entrancy guard in each backend.

/// A byte-oriented character device usable as the kernel console.
///
/// # Contract
///
/// - Every method must be callable before [`Console::init`], from an interrupt
///   handler, and from a fault handler. Nothing here may allocate, may block
///   indefinitely, or may fault on its own.
pub trait Console: Sync {
    /// Program the device. Must be idempotent: later calls are no-ops.
    fn init(&self);

    /// Transmit one byte, blocking only for a bounded time.
    ///
    /// Implementations must not spin forever: a wedged UART that never raises
    /// its status bit has to degrade into a dropped byte, not a hung machine
    /// with no diagnostics.
    fn send_byte(&self, byte: u8);

    /// Transmit a slice of bytes.
    ///
    /// The default is fine for every backend; a port-mapped device overrides it
    /// only if it can do meaningfully better (none can, at this stage).
    fn send_bytes(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.send_byte(byte);
        }
    }

    /// Take a non-blocking byte from the input side, if one is pending.
    ///
    /// Returning `None` is the normal case; the kernel polls rather than
    /// taking UART interrupts, so a backend need not wire up reception at all.
    fn try_recv(&self) -> Option<u8> {
        None
    }
}

/// A `core::fmt` sink over any [`Console`].
///
/// Formatting is arch-neutral, so this lives here rather than in a backend: the
/// two platforms emit byte-identical log lines, which is what lets the test
/// runner parse a single serial log without knowing the architecture.
pub struct ConsoleWriter<'a, C: Console> {
    console: &'a C,
}

impl<'a, C: Console> ConsoleWriter<'a, C> {
    /// Wrap `console` as a formatting sink.
    pub const fn new(console: &'a C) -> Self {
        Self { console }
    }

    /// Access the underlying console.
    pub const fn console(&self) -> &'a C {
        self.console
    }
}

impl<C: Console> core::fmt::Write for ConsoleWriter<'_, C> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.console.send_bytes(s.as_bytes());
        Ok(())
    }
}

/// Stack-allocated byte sink, used by fault and panic reporters.
///
/// The reporters must not depend on a console object being reachable through a
/// `&'static` (they run before, or instead of, normal initialisation), so they
/// accumulate into a bounded buffer and hand the finished slice to
/// [`Console::send_bytes`] in one call. Bounded because the fault path runs on
/// a stack whose exhaustion is itself a fault we are trying to report.
pub struct ByteSink<const N: usize> {
    buffer: [u8; N],
    len: usize,
}

impl<const N: usize> ByteSink<N> {
    /// A new, empty sink.
    pub const fn new() -> Self {
        Self {
            buffer: [0; N],
            len: 0,
        }
    }

    /// Append bytes, truncating instead of overflowing.
    ///
    /// Truncation is deliberate: a fault report must never be able to smash the
    /// stack buffer it is being printed from, which would turn a readable fault
    /// into an unreportable one.
    pub fn push(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(N.saturating_sub(self.len));
        self.buffer[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    /// Append a NUL-terminated string.
    pub fn push_str(&mut self, text: &str) {
        self.push(text.as_bytes());
    }

    /// Append `value` as `0x`-prefixed lowercase hex with no leading zeros.
    pub fn push_hex(&mut self, value: u64) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";

        if value == 0 {
            self.push(b"0x0");
            return;
        }

        self.push(b"0x");

        // Emit from the top nibble down, so no reversal buffer is needed.
        let mut started = false;
        for shift in (0..16).rev() {
            let nibble = ((value >> (shift * 4)) & 0xF) as usize;
            if nibble != 0 || started {
                started = true;
                self.push(&[DIGITS[nibble]]);
            }
        }
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer[..self.len]
    }

    /// Number of bytes written so far.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been written.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for ByteSink<N> {
    fn default() -> Self {
        Self::new()
    }
}