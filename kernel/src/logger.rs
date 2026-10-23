//! Kernel logging over the console.
//!
//! Log lines are written directly to the console in a fixed, machine-parseable
//! shape so that `scripts/qemu.sh` can assert on them regardless of which
//! architecture produced them:
//!
//! ```text
//! [NOVA] <level> | <message>
//! ```
//!
//! Level is one of `TRACE`, `DEBUG`, `INFO `, `WARN `, `ERR  `.
//!
//! # Architecture neutrality
//!
//! This module names no UART and no memory-mapped device. It writes through the
//! `Console` capability from `arch::shared`, which the facade resolves to a 16550
//! on x86-64 and a PL011 on AArch64. The log *format* is therefore a property of
//! NovaOS rather than of the machine, which is what allows a single CI job to
//! parse both targets' output with the same greps.

/// Emit a kernel log line.
///
/// Defined before its first use because `macro_rules!` scoping is textual: a
/// call site above the definition does not see the macro, and `#[macro_export]`
/// only fixes the crate-root *path*, not the order of definitions.
#[macro_export]
macro_rules! klog {
    ($level:expr, $($arg:tt)*) => {
        $crate::logger::log($level, core::format_args!($($arg)*))
    };
}

use core::fmt::Write as _;

use crate::arch::capabilities::{Console, ConsoleWriter};

/// Severity of a log line.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Extremely detailed tracing, off by default.
    Trace = 0,
    /// Debugging aid, off by default.
    Debug = 1,
    /// Normal operational messages.
    Info = 2,
    /// Recoverable anomalies.
    Warn = 3,
    /// Errors and faults.
    Error = 4,
}

impl Level {
    /// Fixed-width tag so serial logs stay column-aligned.
    pub const fn tag(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERR  ",
        }
    }

    /// Numeric severity, used by the runtime level filter.
    pub const fn value(self) -> u8 {
        self as u8
    }
}

/// Messages below this level are discarded.
///
/// `Info` keeps normal boot output visible while staying quiet enough that
/// `[NOVA-TEST]` lines are easy to pick out of the serial log.
pub const MIN_LEVEL: Level = Level::Info;

/// Initialise the console. Safe to call more than once.
pub fn init() {
    crate::arch::console_init();
}

/// Emit one log line.
///
/// # Re-entrancy
///
/// If an interrupt handler or a nested panic tries to log while a message is
/// mid-format, the nested line is dropped rather than interleaving bytes into the
/// outer one. The console backend has its own guard for the same hazard; the
/// filter here avoids doing the formatting work at all when it would be discarded.
pub fn log(level: Level, args: core::fmt::Arguments<'_>) {
    if level.value() < MIN_LEVEL.value() {
        return;
    }

    let mut writer = ConsoleWriter::new(crate::arch::console());
    let _ = write!(writer, "[NOVA] {} | ", level.tag());
    // Write the body directly so a formatting panic inside `args` cannot recurse
    // back through the logger.
    let _ = writer.write_fmt(args);
    let _ = writer.write_str("\r\n");
}

/// Write a line with no level prefix and no filtering.
///
/// Used on the panic path, where re-entering the level filter could recurse.
pub fn panic_report(message: &str) {
    let console = crate::arch::console();
    console.send_bytes(message.as_bytes());
    console.send_bytes(b"\r\n");
}

/// Render a [`core::panic::PanicInfo`] into `sink`.
///
/// Kept out of the handler itself so that formatting cannot fail for a reason the
/// handler would then have to report. The whole output goes into a bounded sink
/// that truncates rather than overflowing: a panic on a nearly-exhausted stack
/// must degrade into a short message, not into a second fault.
pub fn format_panic<const N: usize>(
    sink: &mut crate::arch::capabilities::ByteSink<N>,
    info: &core::panic::PanicInfo<'_>,
) {
    sink.push_str("[NOVA] ERR   | panic: ");

    let mut writer = SinkWriter { sink };
    let _ = writer.write_fmt(format_args!("{}", info.message()));

    sink.push_str("\r\n");
}

/// [`core::fmt::Write`] adapter that appends into a [`ByteSink`].
///
/// A thin adapter because `ByteSink` deliberately does not implement `Write`:
/// it is also handed to `Console::send_bytes` as a plain slice, and implementing
/// `Write` for it would let a caller treat it as an infallible sink without
/// noticing that writes truncate instead of erroring.
struct SinkWriter<'a, const N: usize> {
    sink: &'a mut crate::arch::capabilities::ByteSink<N>,
}

impl<const N: usize> core::fmt::Write for SinkWriter<'_, N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.sink.push_str(s);
        Ok(())
    }
}

/// Print the boot banner.
pub fn print_banner() {
    klog!(
        Level::Info,
        "{} kernel v{} on {}",
        crate::KERNEL_NAME,
        crate::KERNEL_VERSION,
        crate::arch::arch_name()
    );
}
