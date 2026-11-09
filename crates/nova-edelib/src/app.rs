//! The EDE application base — the thing every EDE binary initialises on
//! startup.
//!
//! edelib's `EDE_APPLICATION` macro sets the application's name, version,
//! and logging backend before `main` runs. The Rust equivalent is
//! [`EdeApplication::new`]: a small value carrying the name, the version,
//! and the logger. Applications keep it in a static; tests construct their
//! own and capture lines for assertion.

use crate::log::{EdeLog, Level};

/// The desktop's application base.
///
/// `name` is the tag printed on every log line, and the conventional path
/// segment under `~/.ede/` for this application's private state. `version`
/// is the semantic version of the application binary.
#[derive(Clone, Copy)]
pub struct EdeApplication {
    /// The application's short name, e.g. `ede-panel`.
    name: &'static str,
    /// The application's version, e.g. `0.1.0`.
    version: &'static str,
    /// The application's logger.
    log: EdeLog,
}

impl EdeApplication {
    /// An application with logging wired into `sink`, tagging lines with
    /// `name`, printing `min_level` and above.
    pub const fn new(
        name: &'static str,
        version: &'static str,
        min_level: Level,
        sink: fn(&str),
    ) -> Self {
        Self {
            name,
            version,
            log: EdeLog::new(name, min_level, sink),
        }
    }

    /// The application's log sink, ready to use.
    pub const fn logger(&self) -> &EdeLog {
        &self.log
    }

    /// The application's version string.
    pub const fn version(&self) -> &'static str {
        self.version
    }

    /// The application's name.
    pub const fn name(&self) -> &'static str {
        self.name
    }
}
