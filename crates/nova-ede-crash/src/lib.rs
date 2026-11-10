//! Rust port of `ede-crasher`, EDE's crash handler.
//!
//! Upstream's `ede-crasher` is what the session execs when a program
//! dies: it parses the crashed program's details off the command line
//! (`--appname`, `--apppath`, `--pid`, `--signal`, `--bugaddress`,
//! `--edeapp`), shows a dialog — "Program 'x' quit unexpectedly" with a
//! Show details page — and offers a Report button only to EDE
//! applications, which knows the bug address to file against.
//!
//! This crate is that model: the command line parser, the dialog
//! content, and the decision of what the dialog may offer. It never
//! opens a network connection — filing is [`nova_ede_bug_report`]'s
//! business, and only ever when the user asks.

#![no_std]
#![forbid(unsafe_code)]

/// The longest field the crash details keep.
pub const FIELD_MAX: usize = 64;

/// The details of the crashed program, as the command line supplies
/// them. Every field is optional; upstream degrades to generic text
/// when one is missing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProgramDetails {
    /// The program's display name, e.g. `ede-calc`.
    pub name: [u8; FIELD_MAX],
    name_len: usize,
    /// The executable's path.
    pub path: [u8; FIELD_MAX],
    path_len: usize,
    /// The process id as decimal text.
    pub pid: [u8; FIELD_MAX],
    pid_len: usize,
    /// The caught signal number as decimal text.
    pub signal: [u8; FIELD_MAX],
    signal_len: usize,
    /// The bug address to report against, when the program knows one.
    pub bugaddress: [u8; FIELD_MAX],
    bugaddress_len: usize,
    /// Whether the crashed program is an EDE application — only these
    /// get the Report button.
    pub ede_app: bool,
}

impl ProgramDetails {
    /// An empty record.
    pub const fn new() -> Self {
        Self {
            name: [0; FIELD_MAX],
            name_len: 0,
            path: [0; FIELD_MAX],
            path_len: 0,
            pid: [0; FIELD_MAX],
            pid_len: 0,
            signal: [0; FIELD_MAX],
            signal_len: 0,
            bugaddress: [0; FIELD_MAX],
            bugaddress_len: 0,
            ede_app: false,
        }
    }

    /// The program name, when one was given.
    pub fn name(&self) -> Option<&str> {
        field(&self.name, self.name_len)
    }

    /// The executable path, when one was given.
    pub fn path(&self) -> Option<&str> {
        field(&self.path, self.path_len)
    }

    /// The pid text, when one was given.
    pub fn pid(&self) -> Option<&str> {
        field(&self.pid, self.pid_len)
    }

    /// The signal text, when one was given.
    pub fn signal(&self) -> Option<&str> {
        field(&self.signal, self.signal_len)
    }

    /// The bug address, when one was given.
    pub fn bugaddress(&self) -> Option<&str> {
        field(&self.bugaddress, self.bugaddress_len)
    }
}

impl Default for ProgramDetails {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the crash handler's command line. `Err` carries the message
/// the shell should print; `Ok(None)` means help was requested or no
/// arguments were given (upstream prints usage and exits 0).
pub fn parse_args(args: &[&str]) -> Result<Option<ProgramDetails>, &'static str> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut details = ProgramDetails::new();
    let mut index = 0usize;
    while index < args.len() {
        let argument = args[index];
        let takes_value = matches!(
            argument,
            "-b" | "--bugaddress"
                | "-p"
                | "--pid"
                | "-a"
                | "--appname"
                | "-t"
                | "--apppath"
                | "-s"
                | "--signal"
        );
        match argument {
            "-h" | "--help" => return Ok(None),
            "-e" | "--edeapp" => {
                details.ede_app = true;
                index += 1;
            }
            _ if takes_value => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("Missing parameter");
                };
                set_field(&mut details, argument, value)?;
                index += 1;
            }
            _ => {
                return Err("Unknown option");
            }
        }
    }
    Ok(Some(details))
}

/// Store one value into the field its option names.
fn set_field(details: &mut ProgramDetails, option: &str, value: &str) -> Result<(), &'static str> {
    let bytes = value.as_bytes();
    if bytes.len() > FIELD_MAX {
        return Err("Parameter too long");
    }
    let (slot, len) = match option {
        "-a" | "--appname" => (&mut details.name, &mut details.name_len),
        "-t" | "--apppath" => (&mut details.path, &mut details.path_len),
        "-p" | "--pid" => (&mut details.pid, &mut details.pid_len),
        "-s" | "--signal" => (&mut details.signal, &mut details.signal_len),
        "-b" | "--bugaddress" => (&mut details.bugaddress, &mut details.bugaddress_len),
        _ => return Err("Unknown option"),
    };
    slot[..bytes.len()].copy_from_slice(bytes);
    *len = bytes.len();
    Ok(())
}

/// Read a field as text when present.
fn field(bytes: &[u8; FIELD_MAX], len: usize) -> Option<&str> {
    if len == 0 {
        return None;
    }
    core::str::from_utf8(&bytes[..len]).ok()
}

/// What the crash dialog shows and offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CrashDialog {
    /// The one-line headline, e.g. `Program 'ede-calc' quit unexpectedly.`
    pub headline: [u8; FIELD_MAX],
    headline_len: usize,
    /// The detail lines under the headline.
    pub details: [u8; FIELD_MAX * 2],
    details_len: usize,
    /// Whether the Report button is offered — only for EDE apps with
    /// a bug address.
    pub can_report: bool,
    /// Whether the details page is expanded.
    pub details_shown: bool,
}

impl CrashDialog {
    /// Build the dialog for a set of crash details, exactly as the
    /// upstream dialog assembles it.
    pub fn for_details(details: &ProgramDetails) -> Self {
        let mut dialog = Self {
            headline: [0; FIELD_MAX],
            headline_len: 0,
            details: [0; FIELD_MAX * 2],
            details_len: 0,
            can_report: details.ede_app && details.bugaddress().is_some(),
            details_shown: false,
        };
        // The headline, clipped to the field.
        let mut cursor = 0usize;
        let mut push = |dialog: &mut CrashDialog, text: &[u8]| {
            for &byte in text {
                if cursor >= FIELD_MAX {
                    break;
                }
                dialog.headline[cursor] = byte;
                cursor += 1;
            }
        };
        match details.name() {
            Some(name) => {
                push(&mut dialog, b"Program '");
                push(&mut dialog, name.as_bytes());
                push(&mut dialog, b"' quit unexpectedly.");
            }
            None => push(&mut dialog, b"Program quit unexpectedly."),
        }
        dialog.headline_len = cursor;

        // The details page: name, path, pid, signal, bug address.
        let mut cursor = 0usize;
        for (label, value) in [
            ("Name: ", details.name()),
            ("Path: ", details.path()),
            ("PID: ", details.pid()),
            ("Signal: ", details.signal()),
            ("Report to: ", details.bugaddress()),
        ] {
            let Some(value) = value else {
                continue;
            };
            let line = label.as_bytes();
            let total = line.len() + value.len() + 1;
            if cursor + total > FIELD_MAX * 2 {
                break;
            }
            dialog.details[cursor..cursor + line.len()].copy_from_slice(line);
            cursor += line.len();
            dialog.details[cursor..cursor + value.len()].copy_from_slice(value.as_bytes());
            cursor += value.len();
            dialog.details[cursor] = b'\n';
            cursor += 1;
        }
        dialog.details_len = cursor;
        dialog
    }

    /// The headline text.
    pub fn headline(&self) -> &str {
        core::str::from_utf8(&self.headline[..self.headline_len]).unwrap_or("")
    }

    /// The details text, newline-separated.
    pub fn details(&self) -> &str {
        core::str::from_utf8(&self.details[..self.details_len]).unwrap_or("")
    }

    /// Toggle the details page.
    pub fn toggle_details(&mut self) {
        self.details_shown = !self.details_shown;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_means_usage() {
        assert_eq!(parse_args(&[]), Ok(None));
    }

    #[test]
    fn help_means_usage() {
        assert_eq!(parse_args(&["-h"]), Ok(None));
        assert_eq!(parse_args(&["--help"]), Ok(None));
    }

    #[test]
    fn full_details_parse() {
        let details = parse_args(&[
            "-e",
            "--appname",
            "ede-calc",
            "--apppath",
            "/usr/bin/ede-calc",
            "--pid",
            "4242",
            "--signal",
            "11",
            "--bugaddress",
            "bugs@example.org",
        ])
        .unwrap()
        .unwrap();
        assert!(details.ede_app);
        assert_eq!(details.name(), Some("ede-calc"));
        assert_eq!(details.path(), Some("/usr/bin/ede-calc"));
        assert_eq!(details.pid(), Some("4242"));
        assert_eq!(details.signal(), Some("11"));
        assert_eq!(details.bugaddress(), Some("bugs@example.org"));
    }

    #[test]
    fn missing_values_are_reported() {
        assert_eq!(
            parse_args(&["--appname"]),
            Err("Missing parameter"),
            "upstream's 'Missing application name'"
        );
    }

    #[test]
    fn unknown_options_are_reported() {
        assert_eq!(parse_args(&["--nonsense"]), Err("Unknown option"));
    }

    #[test]
    fn the_dialog_headline_names_the_program() {
        let details = parse_args(&["-e", "-a", "ede-calc"]).unwrap().unwrap();
        let dialog = CrashDialog::for_details(&details);
        assert_eq!(dialog.headline(), "Program 'ede-calc' quit unexpectedly.");
    }

    #[test]
    fn an_unnamed_crash_gets_generic_text() {
        let dialog = CrashDialog::for_details(&ProgramDetails::new());
        assert_eq!(dialog.headline(), "Program quit unexpectedly.");
    }

    #[test]
    fn only_ede_apps_get_the_report_button() {
        let mut details = parse_args(&["-a", "x", "-b", "bugs@example.org"])
            .unwrap()
            .unwrap();
        assert!(!details.ede_app);
        assert!(!CrashDialog::for_details(&details).can_report);
        details.ede_app = true;
        let dialog = CrashDialog::for_details(&details);
        assert!(dialog.can_report, "ede app with a bug address");
    }

    #[test]
    fn the_details_page_lists_what_it_knows() {
        let details = parse_args(&["-a", "ede-calc", "-s", "11", "-p", "7"])
            .unwrap()
            .unwrap();
        let mut dialog = CrashDialog::for_details(&details);
        let text = dialog.details();
        assert!(text.contains("Name: ede-calc"));
        assert!(text.contains("Signal: 11"));
        assert!(text.contains("PID: 7"));
        assert!(!text.contains("Path:"), "an unset field is absent");
        assert!(!dialog.details_shown);
        dialog.toggle_details();
        assert!(dialog.details_shown);
    }
}
