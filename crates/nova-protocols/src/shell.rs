//! Parsing for the NovaOS shell's built-in commands.
//!
//! The shell binary owns I/O and execution; this module owns the syntax. That
//! keeps the parser testable on the host while the shell stays free of the
//! standard library's text tools.

/// A parsed built-in command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellCommand<'a> {
    /// Print the built-in command list.
    Help,
    /// List a directory; `None` means the shell's current directory, `/`.
    Ls {
        /// The directory to list, or `/` when omitted.
        path: Option<&'a str>,
    },
    /// Print the contents of a file.
    Cat {
        /// The file to print.
        path: &'a str,
    },
    /// Copy a line to the console.
    Echo {
        /// The text after the command name.
        text: &'a str,
    },
    /// Report the number of live processes.
    Ps,
}

impl ShellCommand<'_> {
    /// A short, stable name for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            ShellCommand::Help => "help",
            ShellCommand::Ls { .. } => "ls",
            ShellCommand::Cat { .. } => "cat",
            ShellCommand::Echo { .. } => "echo",
            ShellCommand::Ps => "ps",
        }
    }
}

/// Parse one built-in command.
///
/// Extra whitespace is ignored, but command arguments are otherwise taken
/// literally: a filename with spaces is one argument to `cat`, and quoted
/// arguments are not part of v0.1.0. `None` means an empty line or an
/// unknown command; the shell prints its own diagnostic without allocating.
pub fn parse(input: &str) -> Option<ShellCommand<'_>> {
    let input = input.trim();
    let (name, argument) = match input.split_once(' ') {
        Some(parts) => parts,
        None => (input, ""),
    };

    match name {
        "help" if argument.is_empty() => Some(ShellCommand::Help),
        "ls" => Some(ShellCommand::Ls {
            path: non_empty(argument),
        }),
        "cat" => {
            let path = argument.trim();
            if path.is_empty() {
                None
            } else {
                Some(ShellCommand::Cat { path })
            }
        }
        "echo" => Some(ShellCommand::Echo {
            text: argument.trim(),
        }),
        "ps" if argument.is_empty() => Some(ShellCommand::Ps),
        _ => None,
    }
}

fn non_empty(argument: &str) -> Option<&str> {
    if argument.is_empty() {
        None
    } else {
        Some(argument)
    }
}

#[cfg(test)]
mod tests {
    use super::{ShellCommand, parse};

    #[test]
    fn every_builtin_is_recognised() {
        assert_eq!(parse("help"), Some(ShellCommand::Help));
        assert_eq!(parse("ps"), Some(ShellCommand::Ps));
    }

    #[test]
    fn paths_and_text_are_preserved() {
        assert_eq!(
            parse("cat /docs/notes.txt"),
            Some(ShellCommand::Cat {
                path: "/docs/notes.txt"
            })
        );
        assert_eq!(
            parse("echo   hello nova "),
            Some(ShellCommand::Echo { text: "hello nova" })
        );
    }

    #[test]
    fn incomplete_or_unknown_commands_are_refused() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("cat   "), None);
        assert_eq!(parse("rm /"), None);
    }
}
