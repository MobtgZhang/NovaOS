//! Rust port of `ede-bug-report`, EDE's bug filing tool.
//!
//! Upstream logs into a Bugzilla instance over XML-RPC — with
//! credentials embedded in the binary, which this port drops on
//! principle: nothing in NovaOS carries a login, and this crate opens
//! no sockets. What remains is the part worth keeping: the report form
//! (product, component, version, severity, summary, description,
//! hardware, OS), its validation rules, and the serialization the
//! privileged filing helper consumes later.
//!
//! The form is what the crash handler's Report button opens, so the
//! two crates meet here: a crash fills what it knows, the user writes
//! the summary, and the report is queued as text.

#![no_std]
#![forbid(unsafe_code)]

/// The longest summary kept.
pub const SUMMARY_MAX: usize = 80;
/// The longest description kept.
pub const DESCRIPTION_MAX: usize = 1024;
/// The longest value for a single-line field.
pub const FIELD_MAX: usize = 48;
/// How many reports one queue holds.
pub const MAX_QUEUE: usize = 4;

/// Severity, upstream's Bugzilla scale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// Cosmetic.
    Trivial,
    /// Minor annoyance.
    Minor,
    /// Normal bug.
    Normal,
    /// Major breakage.
    Major,
    /// Unusable / data loss.
    Critical,
}

impl Severity {
    /// The string Bugzilla stores.
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Trivial => "trivial",
            Severity::Minor => "minor",
            Severity::Normal => "normal",
            Severity::Major => "major",
            Severity::Critical => "critical",
        }
    }

    /// Parse the stored form; unknown values are `Normal`.
    pub fn parse(text: &str) -> Severity {
        match text {
            "trivial" => Severity::Trivial,
            "minor" => Severity::Minor,
            "major" => Severity::Major,
            "critical" => Severity::Critical,
            _ => Severity::Normal,
        }
    }
}

/// One bug report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BugReport {
    /// The product the bug belongs to, e.g. `NovaOS EDE`.
    pub product: [u8; FIELD_MAX],
    product_len: usize,
    /// The component, e.g. `ede-calc`.
    pub component: [u8; FIELD_MAX],
    component_len: usize,
    /// The reported version, e.g. `0.1.0`.
    pub version: [u8; FIELD_MAX],
    version_len: usize,
    /// The severity.
    pub severity: Severity,
    /// The one-line summary — required.
    pub summary: [u8; SUMMARY_MAX],
    summary_len: usize,
    /// The full description — required.
    pub description: [u8; DESCRIPTION_MAX],
    description_len: usize,
    /// The hardware description, e.g. `x86_64 QEMU`.
    pub hardware: [u8; FIELD_MAX],
    hardware_len: usize,
    /// The OS description, e.g. `NovaOS 0.1.0`.
    pub os: [u8; FIELD_MAX],
    os_len: usize,
}

impl Default for BugReport {
    fn default() -> Self {
        let mut report = Self {
            product: [0; FIELD_MAX],
            product_len: 0,
            component: [0; FIELD_MAX],
            component_len: 0,
            version: [0; FIELD_MAX],
            version_len: 0,
            severity: Severity::Normal,
            summary: [0; SUMMARY_MAX],
            summary_len: 0,
            description: [0; DESCRIPTION_MAX],
            description_len: 0,
            hardware: [0; FIELD_MAX],
            hardware_len: 0,
            os: [0; FIELD_MAX],
            os_len: 0,
        };
        let product = b"NovaOS EDE";
        report.product[..product.len()].copy_from_slice(product);
        report.product_len = product.len();
        let version = b"0.1.0";
        report.version[..version.len()].copy_from_slice(version);
        report.version_len = version.len();
        report
    }
}

impl BugReport {
    /// A report with NovaOS's product and version pre-filled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a single-line field by name. Returns false for unknown
    /// names and overlong values.
    pub fn set_field(&mut self, name: &str, value: &str) -> bool {
        let bytes = value.as_bytes();
        let (slot, len, cap) = match name {
            "product" => (&mut self.product, &mut self.product_len, FIELD_MAX),
            "component" => (&mut self.component, &mut self.component_len, FIELD_MAX),
            "version" => (&mut self.version, &mut self.version_len, FIELD_MAX),
            "hardware" => (&mut self.hardware, &mut self.hardware_len, FIELD_MAX),
            "os" => (&mut self.os, &mut self.os_len, FIELD_MAX),
            _ => return false,
        };
        if bytes.len() > cap {
            return false;
        }
        slot[..bytes.len()].copy_from_slice(bytes);
        *len = bytes.len();
        true
    }

    /// Set the summary. Overlong text is clipped, not rejected — the
    /// user should not lose a report to a long line.
    pub fn set_summary(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let take = bytes.len().min(SUMMARY_MAX);
        self.summary[..take].copy_from_slice(&bytes[..take]);
        self.summary_len = take;
    }

    /// Set the description, clipped the same way.
    pub fn set_description(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let take = bytes.len().min(DESCRIPTION_MAX);
        self.description[..take].copy_from_slice(&bytes[..take]);
        self.description_len = take;
    }

    /// The summary text.
    pub fn summary(&self) -> &str {
        core::str::from_utf8(&self.summary[..self.summary_len]).unwrap_or("")
    }

    /// The description text.
    pub fn description(&self) -> &str {
        core::str::from_utf8(&self.description[..self.description_len]).unwrap_or("")
    }

    /// A single-line field's value.
    pub fn field(&self, name: &str) -> Option<&str> {
        match name {
            "product" => text(&self.product, self.product_len),
            "component" => text(&self.component, self.component_len),
            "version" => text(&self.version, self.version_len),
            "hardware" => text(&self.hardware, self.hardware_len),
            "os" => text(&self.os, self.os_len),
            _ => None,
        }
    }

    /// Whether the report is complete enough to file: a summary and a
    /// description are mandatory, everything else has a default.
    pub fn is_valid(&self) -> bool {
        self.summary_len > 0 && self.description_len > 0 && self.product_len > 0
    }

    /// The validation problem, when there is one.
    pub fn problem(&self) -> Option<&'static str> {
        if self.summary_len == 0 {
            return Some("A summary is required");
        }
        if self.description_len == 0 {
            return Some("A description is required");
        }
        if self.product_len == 0 {
            return Some("A product is required");
        }
        None
    }

    /// Serialize the report as the `key: value` text the filing helper
    /// reads — one field per line, blank line, then the description.
    /// Returns the bytes written.
    pub fn to_text(&self, out: &mut [u8]) -> usize {
        let mut cursor = 0usize;
        for (key, value) in [
            ("Product", self.field("product")),
            ("Component", self.field("component")),
            ("Version", self.field("version")),
            ("Severity", Some(self.severity.as_str())),
            ("Hardware", self.field("hardware")),
            ("OS", self.field("os")),
            ("Summary", Some(self.summary())),
        ] {
            let Some(value) = value else {
                continue;
            };
            cursor = line(out, cursor, key.as_bytes(), value.as_bytes());
        }
        cursor = copy(out, cursor, b"\n");
        copy(out, cursor, self.description().as_bytes())
    }

    /// Parse a report back from [`BugReport::to_text`] bytes — the
    /// filing helper's side of the wire.
    pub fn from_text(bytes: &[u8]) -> Option<Self> {
        let mut report = Self::new();
        let mut body_start = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            // Find the line end.
            let line_end = bytes[index..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|offset| index + offset)
                .unwrap_or(bytes.len());
            let line = &bytes[index..line_end];
            if line.is_empty() {
                body_start = line_end + 1;
                break;
            }
            let colon = line.iter().position(|&byte| byte == b':')?;
            let key = core::str::from_utf8(&line[..colon]).ok()?;
            let value = core::str::from_utf8(line[colon + 1..].trim_ascii()).ok()?;
            match key {
                "Product" => report.set_field("product", value),
                "Component" => report.set_field("component", value),
                "Version" => report.set_field("version", value),
                "Severity" => {
                    report.severity = Severity::parse(value);
                    true
                }
                "Hardware" => report.set_field("hardware", value),
                "OS" => report.set_field("os", value),
                "Summary" => {
                    report.set_summary(value);
                    true
                }
                _ => false,
            };
            index = line_end + 1;
        }
        if body_start <= bytes.len()
            && let Ok(body) = core::str::from_utf8(&bytes[body_start..])
        {
            report.set_description(body);
        }
        Some(report)
    }
}

/// A queue of reports waiting to be filed by a privileged helper.
#[derive(Clone, Copy)]
pub struct ReportQueue {
    reports: [Option<BugReport>; MAX_QUEUE],
    count: usize,
}

impl ReportQueue {
    /// An empty queue.
    pub const fn new() -> Self {
        Self {
            reports: [None; MAX_QUEUE],
            count: 0,
        }
    }

    /// Queue a complete report; incomplete ones are refused with their
    /// problem statement.
    pub fn push(&mut self, report: &BugReport) -> Result<(), &'static str> {
        if let Some(problem) = report.problem() {
            return Err(problem);
        }
        if self.count >= MAX_QUEUE {
            return Err("The report queue is full");
        }
        self.reports[self.count] = Some(*report);
        self.count += 1;
        Ok(())
    }

    /// How many reports are queued.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The report at `index`.
    pub fn report(&self, index: usize) -> Option<&BugReport> {
        self.reports.get(index)?.as_ref()
    }

    /// File the oldest report: remove it from the queue and hand it to
    /// the caller. The caller is the privileged helper — this crate
    /// never sends anything itself.
    pub fn take_oldest(&mut self) -> Option<BugReport> {
        if self.count == 0 {
            return None;
        }
        let report = self.reports[0].take()?;
        for index in 1..self.count {
            self.reports[index - 1] = self.reports[index];
        }
        self.count -= 1;
        Some(report)
    }
}

impl Default for ReportQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a fixed field as text.
fn text(bytes: &[u8], len: usize) -> Option<&str> {
    if len == 0 {
        return None;
    }
    core::str::from_utf8(&bytes[..len]).ok()
}

/// Append `key: value\n`.
fn line(out: &mut [u8], cursor: usize, key: &[u8], value: &[u8]) -> usize {
    let mut next = copy(out, cursor, key);
    next = copy(out, next, b": ");
    next = copy(out, next, value);
    copy(out, next, b"\n")
}

/// Copy bytes in at `cursor`, returning the new cursor.
fn copy(out: &mut [u8], cursor: usize, value: &[u8]) -> usize {
    let take = value.len().min(out.len().saturating_sub(cursor));
    out[cursor..cursor + take].copy_from_slice(&value[..take]);
    cursor + take
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> BugReport {
        let mut report = BugReport::new();
        report.set_field("component", "ede-calc");
        report.set_summary("Calculator shows Error on divide by zero");
        report.set_description("Dividing by zero shows Error but keeps the tape.");
        report
    }

    #[test]
    fn defaults_are_novaos() {
        let report = BugReport::new();
        assert_eq!(report.field("product"), Some("NovaOS EDE"));
        assert_eq!(report.field("version"), Some("0.1.0"));
        assert_eq!(report.severity, Severity::Normal);
        assert!(
            !report.is_valid(),
            "summary and description are still empty"
        );
    }

    #[test]
    fn validation_names_the_missing_field() {
        let mut report = BugReport::new();
        assert_eq!(report.problem(), Some("A summary is required"));
        report.set_summary("x");
        assert_eq!(report.problem(), Some("A description is required"));
        report.set_description("y");
        assert!(report.is_valid());
        assert_eq!(report.problem(), None);
    }

    #[test]
    fn summary_and_description_clip_instead_of_refusing() {
        let mut report = BugReport::new();
        report.set_summary(&"s".repeat(SUMMARY_MAX + 100));
        assert_eq!(report.summary().len(), SUMMARY_MAX);
        report.set_description(&"d".repeat(DESCRIPTION_MAX + 100));
        assert_eq!(report.description().len(), DESCRIPTION_MAX);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut report = BugReport::new();
        assert!(!report.set_field("password", "hunter2"));
        assert!(!report.set_field("component", &"c".repeat(FIELD_MAX + 1)));
    }

    #[test]
    fn severity_parses_back() {
        assert_eq!(Severity::parse("critical"), Severity::Critical);
        assert_eq!(Severity::parse("nonsense"), Severity::Normal);
        assert_eq!(Severity::Major.as_str(), "major");
    }

    #[test]
    fn a_report_round_trips_through_text() {
        let mut report = filled();
        report.severity = Severity::Major;
        let mut buffer = [0u8; 2048];
        let length = report.to_text(&mut buffer);
        let reloaded = BugReport::from_text(&buffer[..length]).unwrap();
        assert_eq!(reloaded.field("component"), Some("ede-calc"));
        assert_eq!(reloaded.severity, Severity::Major);
        assert_eq!(reloaded.summary(), report.summary());
        assert_eq!(reloaded.description(), report.description());
    }

    #[test]
    fn the_text_form_starts_with_the_fields() {
        let report = filled();
        let mut buffer = [0u8; 2048];
        let length = report.to_text(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap();
        assert!(text.starts_with("Product: NovaOS EDE\n"));
        assert!(text.contains("Summary: Calculator shows Error on divide by zero\n"));
        assert!(text.ends_with("\n\nDividing by zero shows Error but keeps the tape."));
    }

    #[test]
    fn the_queue_refuses_incomplete_reports() {
        let mut queue = ReportQueue::new();
        let empty = BugReport::new();
        assert_eq!(queue.push(&empty), Err("A summary is required"));
        assert_eq!(queue.count(), 0);
        assert!(queue.push(&filled()).is_ok());
        assert_eq!(queue.count(), 1);
    }

    #[test]
    fn the_queue_is_bounded_and_fifo() {
        let mut queue = ReportQueue::new();
        for _ in 0..MAX_QUEUE {
            assert!(queue.push(&filled()).is_ok());
        }
        assert_eq!(
            queue.push(&filled()),
            Err("The report queue is full"),
            "bounded, never grows"
        );
        let oldest = queue.take_oldest().unwrap();
        assert_eq!(oldest.field("component"), Some("ede-calc"));
        assert_eq!(queue.count(), MAX_QUEUE - 1);
        while queue.take_oldest().is_some() {}
        assert_eq!(queue.take_oldest(), None);
    }
}
