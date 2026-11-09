//! The logger every EDE application bootstraps at startup.
//!
//! edelib's `Debug` module gives every EDE component the same line format
//! (`[app] level: message`), one shared switch, and no dependency on a C
//! library. NovaOS's kernel has its own `klog` for ring 0; this is the ring
//! 3 twin. Applications write through [`EdeLog`]; [`EdeApplication::new`]
//! installs a sensible default.

use core::fmt::Write as _;

/// A log severity, ordered from chatty to fatal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Everything, including developer chatter.
    Trace,
    /// Development diagnostics.
    Debug,
    /// Routine operational milestones.
    Info,
    /// Something is off, but the program continues.
    Warn,
    /// Something failed; the program may not survive.
    Error,
}

impl Level {
    /// The level's short tag, as printed on every line.
    pub const fn tag(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// The application's log line writer.
///
/// The sink is a `fn(&str)` so the application decides where lines go —
/// ring 3 writes to the console service, tests capture into a buffer, and
/// the host runner can assert on exact bytes without a framebuffer.
#[derive(Clone, Copy)]
pub struct EdeLog {
    /// The tag printed in brackets on every line, e.g. `ede-panel`.
    name: &'static str,
    /// The minimum level that is printed; lower-severity lines are dropped.
    min_level: Level,
    /// The sink that receives complete lines (tag + colon + text).
    sink: fn(&str),
}

impl EdeLog {
    /// A logger tagging lines with `name`, printing `min_level` and above,
    /// writing into `sink`.
    pub const fn new(name: &'static str, min_level: Level, sink: fn(&str)) -> Self {
        Self {
            name,
            min_level,
            sink,
        }
    }

    /// A logger that drops everything — for quiet sessions.
    pub const fn silent(name: &'static str, sink: fn(&str)) -> Self {
        Self::new(name, Level::Error, sink)
    }

    /// The application's short tag, as it appears on every line.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Write one line at `level`. The line is dropped silently if its level
    /// is below the configured minimum.
    pub fn log(&self, level: Level, message: core::fmt::Arguments<'_>) {
        if level < self.min_level {
            return;
        }
        // Format into a fixed stack buffer; a truncated line is still a
        // well-formed prefix, and the alternative (allocating) is the exact
        // thing ring 3 avoids.
        let mut buffer = [0u8; 256];
        let mut writer = LineWriter {
            buffer: &mut buffer,
            used: 0,
        };
        let _ = write!(writer, "[{}] {}: ", self.name, level.tag());
        let _ = writer.write_fmt(message);
        let used = writer.used;
        let line = core::str::from_utf8(&buffer[..used]).unwrap_or("?");
        (self.sink)(line);
    }

    /// Convenience: `trace`-level line.
    pub fn trace(&self, message: core::fmt::Arguments<'_>) {
        self.log(Level::Trace, message);
    }

    /// Convenience: `debug`-level line.
    pub fn debug(&self, message: core::fmt::Arguments<'_>) {
        self.log(Level::Debug, message);
    }

    /// Convenience: `info`-level line.
    pub fn info(&self, message: core::fmt::Arguments<'_>) {
        self.log(Level::Info, message);
    }

    /// Convenience: `warn`-level line.
    pub fn warn(&self, message: core::fmt::Arguments<'_>) {
        self.log(Level::Warn, message);
    }

    /// Convenience: `error`-level line.
    pub fn error(&self, message: core::fmt::Arguments<'_>) {
        self.log(Level::Error, message);
    }
}

impl core::fmt::Debug for EdeLog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdeLog")
            .field("name", &self.name)
            .field("min_level", &self.min_level)
            .finish()
    }
}

/// A bounded writer used by [`EdeLog::log`].
struct LineWriter<'a> {
    buffer: &'a mut [u8],
    used: usize,
}

impl core::fmt::Write for LineWriter<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let room = self.buffer.len().saturating_sub(self.used);
        let take = bytes.len().min(room);
        self.buffer[self.used..self.used + take].copy_from_slice(&bytes[..take]);
        self.used += take;
        Ok(())
    }
}
