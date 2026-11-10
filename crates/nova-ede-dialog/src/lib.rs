//! Rust port of `ede-dialog`, EDE's shell-facing message dialog.
//!
//! Upstream is a `kdialog`/`xmessage`-style utility: shell scripts ask
//! questions and get an exit code back — `--yesno`, `--yesnocancel`,
//! `--error`, `--sorry`, `--msgbox`, `--inputbox`, `--password`, plus
//! `--title`. This crate is the model: parsing the command line into a
//! [`Dialog`] and mapping the user's answer to the exit code, with the
//! conventions NovaOS scripts already use:
//!
//! | answer        | exit code |
//! |---------------|-----------|
//! | yes / OK      | 0         |
//! | no            | 1         |
//! | cancel        | 2         |
//! | escape/timeout| 254       |

#![no_std]
#![forbid(unsafe_code)]

/// The longest message or input a dialog keeps.
pub const MESSAGE_MAX: usize = 256;
/// The longest title a dialog keeps.
pub const TITLE_MAX: usize = 48;

/// Exit codes the shell reads back.
pub const CODE_YES: u8 = 0;
pub const CODE_NO: u8 = 1;
pub const CODE_CANCEL: u8 = 2;
pub const CODE_ESCAPE: u8 = 254;

/// What kind of dialog to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Yes / No question.
    YesNo,
    /// Yes / No / Cancel question.
    YesNoCancel,
    /// An error message with an OK button.
    Error,
    /// A "sorry" message with an OK button.
    Sorry,
    /// A plain message with an OK button.
    MsgBox,
    /// A question with a text entry.
    InputBox,
    /// A question with a masked text entry.
    Password,
}

impl Kind {
    /// Whether the dialog shows a Yes/No pair.
    pub const fn is_question(self) -> bool {
        matches!(self, Kind::YesNo | Kind::YesNoCancel)
    }

    /// Whether the dialog collects text.
    pub const fn collects_input(self) -> bool {
        matches!(self, Kind::InputBox | Kind::Password)
    }
}

/// A parsed dialog request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dialog {
    /// The dialog kind.
    pub kind: Kind,
    /// The window title.
    pub title: [u8; TITLE_MAX],
    title_len: usize,
    /// The message text.
    pub message: [u8; MESSAGE_MAX],
    message_len: usize,
}

impl Dialog {
    /// A dialog with the default title `EDE Dialog`.
    pub const fn new(kind: Kind) -> Self {
        Self {
            kind,
            title: [0; TITLE_MAX],
            title_len: 0,
            message: [0; MESSAGE_MAX],
            message_len: 0,
        }
    }

    /// Set the message; long text is clipped.
    pub fn set_message(&mut self, message: &str) -> bool {
        let bytes = message.as_bytes();
        if bytes.len() > MESSAGE_MAX {
            return false;
        }
        self.message[..bytes.len()].copy_from_slice(bytes);
        self.message_len = bytes.len();
        true
    }

    /// Set the title; long text is clipped.
    pub fn set_title(&mut self, title: &str) -> bool {
        let bytes = title.as_bytes();
        if bytes.len() > TITLE_MAX {
            return false;
        }
        self.title[..bytes.len()].copy_from_slice(bytes);
        self.title_len = bytes.len();
        true
    }

    /// The message text.
    pub fn message(&self) -> &str {
        core::str::from_utf8(&self.message[..self.message_len]).unwrap_or("")
    }

    /// The title text, falling back to the default.
    pub fn title(&self) -> &str {
        if self.title_len == 0 {
            return "EDE Dialog";
        }
        core::str::from_utf8(&self.title[..self.title_len]).unwrap_or("EDE Dialog")
    }
}

/// Parse the command line into a dialog. Returns `Err` with a message
/// for a usage error — the shell prints it and exits 1.
pub fn parse_args(args: &[&str]) -> Result<Dialog, &'static str> {
    let mut kind: Option<Kind> = None;
    let mut message: Option<&str> = None;
    let mut title = "";
    let mut index = 0usize;
    while index < args.len() {
        let argument = args[index];
        let option_kind = match argument {
            "--yesno" => Some(Kind::YesNo),
            "--yesnocancel" => Some(Kind::YesNoCancel),
            "--error" => Some(Kind::Error),
            "--sorry" => Some(Kind::Sorry),
            "--msgbox" => Some(Kind::MsgBox),
            "--inputbox" => Some(Kind::InputBox),
            "--password" => Some(Kind::Password),
            _ => None,
        };
        if let Some(found) = option_kind {
            kind = Some(found);
            index += 1;
            message = Some(
                args.get(index)
                    .copied()
                    .ok_or("Missing message parameter")?,
            );
            index += 1;
        } else {
            match argument {
                "--title" => {
                    index += 1;
                    title = args.get(index).copied().ok_or("Missing title parameter")?;
                    index += 1;
                }
                "-h" | "--help" => return Err("usage"),
                _ => return Err("Unknown option"),
            }
        }
    }
    let Some(kind) = kind else {
        return Err("No dialog specified");
    };
    let message = message.ok_or("Missing message parameter")?;
    let mut dialog = Dialog::new(kind);
    if !title.is_empty() {
        dialog.set_title(title);
    }
    dialog.set_message(message);
    Ok(dialog)
}

/// The user's answer to a dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Yes / OK.
    Yes,
    /// No.
    No,
    /// Cancel.
    Cancel,
    /// Escape or timeout: no decision was made.
    Escape,
    /// Typed text from an input or password dialog, accepted.
    Text,
}

impl Answer {
    /// The exit code for this answer.
    pub const fn code(self) -> u8 {
        match self {
            Answer::Yes | Answer::Text => CODE_YES,
            Answer::No => CODE_NO,
            Answer::Cancel => CODE_CANCEL,
            Answer::Escape => CODE_ESCAPE,
        }
    }
}

/// Whether an answer is legal for a kind — e.g. No is meaningless on a
/// message box, and Cancel on a Yes/No question.
pub fn answer_allowed(kind: Kind, answer: Answer) -> bool {
    match kind {
        Kind::YesNo => matches!(answer, Answer::Yes | Answer::No | Answer::Escape),
        Kind::YesNoCancel => matches!(
            answer,
            Answer::Yes | Answer::No | Answer::Cancel | Answer::Escape
        ),
        Kind::InputBox | Kind::Password => matches!(answer, Answer::Text | Answer::Escape),
        Kind::Error | Kind::Sorry | Kind::MsgBox => {
            matches!(answer, Answer::Yes | Answer::Escape)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yesno_parses_and_codes() {
        let dialog = parse_args(&["--yesno", "Overwrite the file?"]).unwrap();
        assert_eq!(dialog.kind, Kind::YesNo);
        assert_eq!(dialog.message(), "Overwrite the file?");
        assert_eq!(Answer::Yes.code(), 0);
        assert_eq!(Answer::No.code(), 1);
    }

    #[test]
    fn title_can_follow_the_kind() {
        let dialog = parse_args(&["--error", "Disk full", "--title", "File manager"]).unwrap();
        assert_eq!(dialog.title(), "File manager");
        assert_eq!(dialog.kind, Kind::Error);
    }

    #[test]
    fn title_can_precede_the_kind() {
        let dialog = parse_args(&["--title", "Updater", "--msgbox", "Done."]).unwrap();
        assert_eq!(dialog.title(), "Updater");
        assert_eq!(dialog.kind, Kind::MsgBox);
    }

    #[test]
    fn the_default_title_is_used_when_unset() {
        let dialog = parse_args(&["--msgbox", "Saved."]).unwrap();
        assert_eq!(dialog.title(), "EDE Dialog");
    }

    #[test]
    fn inputbox_collects_text() {
        let dialog = parse_args(&["--inputbox", "Enter a name:", "--title", "New"]).unwrap();
        assert!(dialog.kind.collects_input());
        assert!(!dialog.kind.is_question());
        assert_eq!(Answer::Text.code(), 0);
    }

    #[test]
    fn password_masks_and_input_does_not() {
        let password = parse_args(&["--password", "Unlock:"]).unwrap();
        assert_eq!(password.kind, Kind::Password);
        let input = parse_args(&["--inputbox", "Name:"]).unwrap();
        assert_eq!(input.kind, Kind::InputBox);
    }

    #[test]
    fn usage_errors_are_reported() {
        assert!(parse_args(&[]).is_err());
        assert_eq!(parse_args(&["--yesno"]), Err("Missing message parameter"));
        assert_eq!(
            parse_args(&["--title", "x"]),
            Err("No dialog specified"),
            "a title without a kind is not a dialog"
        );
        assert_eq!(parse_args(&["--nonsense"]), Err("Unknown option"));
        assert_eq!(parse_args(&["-h"]), Err("usage"));
    }

    #[test]
    fn answers_are_filtered_per_kind() {
        assert!(answer_allowed(Kind::YesNo, Answer::No));
        assert!(!answer_allowed(Kind::YesNo, Answer::Cancel));
        assert!(answer_allowed(Kind::YesNoCancel, Answer::Cancel));
        assert!(!answer_allowed(Kind::MsgBox, Answer::No));
        assert!(!answer_allowed(Kind::InputBox, Answer::Yes));
        assert!(answer_allowed(Kind::Error, Answer::Escape));
    }

    #[test]
    fn escape_always_codes_254() {
        assert_eq!(Answer::Escape.code(), CODE_ESCAPE);
    }

    #[test]
    fn the_message_is_clipped_not_rejected() {
        let mut dialog = Dialog::new(Kind::MsgBox);
        let long = "x".repeat(MESSAGE_MAX + 10);
        assert!(!dialog.set_message(&long));
        assert!(dialog.message().is_empty());
        let fits = "y".repeat(MESSAGE_MAX);
        assert!(dialog.set_message(&fits));
        assert_eq!(dialog.message().len(), MESSAGE_MAX);
    }
}
