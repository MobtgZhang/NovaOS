//! A parser for freedesktop `.desktop` files — the launcher and icon format
//! EDE builds its desktop and panel on.
//!
//! EDE's desktop icons, its autostart list, and its application menus are
//! all driven by `.desktop` files. This module reads the subset of the
//! format NovaOS uses: `Type`, `Name`, `Exec`, `Icon`, `Comment`, and
//! `Terminal`. Parsing problems degrade to defaults, never to panics.

use core::cmp;

/// The longest string this parser keeps from a `.desktop` file.
pub const MAX_FIELD: usize = 128;

/// The longest argument list a parsed `Exec=` line expands to.
pub const MAX_ARGV: usize = 8;
/// The longest single argument kept after expansion.
pub const MAX_ARG: usize = 64;

/// One parsed `.desktop` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopFile {
    /// The `Exec=` command, e.g. `terminal`.
    pub exec: [u8; MAX_FIELD],
    exec_len: usize,
    /// The `Exec=` command with field codes stripped, e.g. `terminal`.
    exec_stripped: [u8; MAX_FIELD],
    exec_stripped_len: usize,
    /// The `Name=` label, e.g. `Terminal`.
    pub name: [u8; MAX_FIELD],
    name_len: usize,
    /// The `GenericName=` label, when present.
    pub generic_name: [u8; MAX_FIELD],
    generic_name_len: usize,
    /// The `Icon=` theme name, e.g. `utilities-terminal`.
    pub icon: [u8; MAX_FIELD],
    icon_len: usize,
    /// The `Comment=` description, when present.
    pub comment: [u8; MAX_FIELD],
    comment_len: usize,
    /// The `Path=` working directory for the launched program.
    pub path: [u8; MAX_FIELD],
    path_len: usize,
    /// The `URL=` target of a `Type=Link` entry.
    pub url: [u8; MAX_FIELD],
    url_len: usize,
    /// The `Categories=` semicolon list, verbatim.
    pub categories: [u8; MAX_FIELD],
    categories_len: usize,
    /// The `OnlyShowIn=` semicolon list, verbatim.
    pub only_show_in: [u8; MAX_FIELD],
    only_show_in_len: usize,
    /// The `NotShowIn=` semicolon list, verbatim.
    pub not_show_in: [u8; MAX_FIELD],
    not_show_in_len: usize,
    /// Whether `Terminal=true` was set: run inside a terminal window.
    pub terminal: bool,
    /// Whether `StartupNotify=true` was set.
    pub startup_notify: bool,
    /// Whether `Hidden=true` was set: a leftover from a removed install.
    pub hidden: bool,
    /// Whether `NoDisplay=true` was set: not shown in menus.
    pub no_display: bool,
    /// The `Type=` of the entry.
    pub kind: EntryKind,
}

/// The `Type=` field of a `.desktop` entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// `Type=Application` — something the launcher can run.
    Application,
    /// `Type=Directory` — a submenu directory.
    Directory,
    /// `Type=Link` — a URL link.
    Link,
    /// A missing or unrecognised type.
    Unknown,
}

impl DesktopFile {
    /// An entry with no fields set; `is_valid` returns false.
    pub const fn empty() -> Self {
        Self {
            exec: [0; MAX_FIELD],
            exec_len: 0,
            exec_stripped: [0; MAX_FIELD],
            exec_stripped_len: 0,
            name: [0; MAX_FIELD],
            name_len: 0,
            generic_name: [0; MAX_FIELD],
            generic_name_len: 0,
            icon: [0; MAX_FIELD],
            icon_len: 0,
            comment: [0; MAX_FIELD],
            comment_len: 0,
            path: [0; MAX_FIELD],
            path_len: 0,
            url: [0; MAX_FIELD],
            url_len: 0,
            categories: [0; MAX_FIELD],
            categories_len: 0,
            only_show_in: [0; MAX_FIELD],
            only_show_in_len: 0,
            not_show_in: [0; MAX_FIELD],
            not_show_in_len: 0,
            terminal: false,
            startup_notify: false,
            hidden: false,
            no_display: false,
            kind: EntryKind::Unknown,
        }
    }

    /// Parse `bytes` as a `.desktop` file.
    ///
    /// Only the `[Desktop Entry]` group is read — keys in later groups
    /// (`[Desktop Action ...]`) belong to other objects and must not leak
    /// into this one. Unknown lines are ignored rather than rejected.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut entry = Self::empty();
        let mut in_entry_group = false;
        for raw_line in bytes.split(|&b| b == b'\n') {
            let line = trim(raw_line);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            if line[0] == b'[' {
                in_entry_group = line == b"[Desktop Entry]";
                continue;
            }
            if !in_entry_group {
                continue;
            }
            let Some(equals) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key = trim(&line[..equals]);
            let value = trim(&line[equals + 1..]);
            // An empty value carries no information; the freedesktop spec
            // treats it as "unset". Keeping the first non-empty value is
            // what makes `Name[fr]=` fall back to `Name`.
            if value.is_empty() {
                continue;
            }
            // Localised keys (`Name[de]=...`) take the unlocalised value.
            let key = match key.iter().position(|&b| b == b'[') {
                Some(end) => &key[..end],
                None => key,
            };
            match key {
                b"Exec" => {
                    set_field(&mut entry.exec, &mut entry.exec_len, value);
                    entry.exec_stripped_len = strip_field_codes(value, &mut entry.exec_stripped);
                    // Collapse the double spaces a removed code leaves.
                    let mut collapsed = [0u8; MAX_FIELD];
                    let collapsed_length = collapse_spaces(
                        &entry.exec_stripped[..entry.exec_stripped_len],
                        &mut collapsed,
                    );
                    entry.exec_stripped[..collapsed_length]
                        .copy_from_slice(&collapsed[..collapsed_length]);
                    entry.exec_stripped_len = collapsed_length;
                }
                b"Name" => set_field(&mut entry.name, &mut entry.name_len, value),
                b"GenericName" => {
                    set_field(&mut entry.generic_name, &mut entry.generic_name_len, value)
                }
                b"Icon" => set_field(&mut entry.icon, &mut entry.icon_len, value),
                b"Comment" => set_field(&mut entry.comment, &mut entry.comment_len, value),
                b"Path" => set_field(&mut entry.path, &mut entry.path_len, value),
                b"URL" => set_field(&mut entry.url, &mut entry.url_len, value),
                b"Categories" => set_field(&mut entry.categories, &mut entry.categories_len, value),
                b"OnlyShowIn" => {
                    set_field(&mut entry.only_show_in, &mut entry.only_show_in_len, value)
                }
                b"NotShowIn" => {
                    set_field(&mut entry.not_show_in, &mut entry.not_show_in_len, value)
                }
                b"Terminal" => entry.terminal = parse_bool(value),
                b"StartupNotify" => entry.startup_notify = parse_bool(value),
                b"Hidden" => entry.hidden = parse_bool(value),
                b"NoDisplay" => entry.no_display = parse_bool(value),
                b"Type" => {
                    entry.kind = match value {
                        b"Application" => EntryKind::Application,
                        b"Directory" => EntryKind::Directory,
                        b"Link" => EntryKind::Link,
                        _ => EntryKind::Unknown,
                    }
                }
                _ => {}
            }
        }
        entry
    }

    /// The `Exec=` command as bytes, with field codes stripped.
    pub fn exec_bytes(&self) -> &[u8] {
        &self.exec_stripped[..self.exec_stripped_len]
    }

    /// The `Exec=` command as text, with field codes stripped.
    pub fn exec(&self) -> Option<&str> {
        core::str::from_utf8(self.exec_stripped_bytes()).ok()
    }

    /// The raw `Exec=` line, field codes intact — the input to
    /// [`DesktopFile::expand_exec`].
    pub fn exec_raw(&self) -> Option<&str> {
        core::str::from_utf8(&self.exec[..self.exec_len]).ok()
    }

    /// The `Name=` label as text.
    pub fn name(&self) -> Option<&str> {
        core::str::from_utf8(&self.name[..self.name_len]).ok()
    }

    /// The `GenericName=` label as text, when present.
    pub fn generic_name(&self) -> Option<&str> {
        if self.generic_name_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.generic_name[..self.generic_name_len]).ok()
    }

    /// The `Icon=` theme name as text.
    pub fn icon(&self) -> Option<&str> {
        core::str::from_utf8(&self.icon[..self.icon_len]).ok()
    }

    /// The `Comment=` text, when present.
    pub fn comment(&self) -> Option<&str> {
        if self.comment_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.comment[..self.comment_len]).ok()
    }

    /// The `Path=` working directory, when present.
    pub fn path(&self) -> Option<&str> {
        if self.path_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.path[..self.path_len]).ok()
    }

    /// The `URL=` target, when present.
    pub fn url(&self) -> Option<&str> {
        if self.url_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.url[..self.url_len]).ok()
    }

    /// Whether `Hidden=true` was set.
    pub const fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Whether `NoDisplay=true` was set.
    pub const fn is_no_display(&self) -> bool {
        self.no_display
    }

    /// Whether `Terminal=true` was set.
    pub const fn wants_terminal(&self) -> bool {
        self.terminal
    }

    /// Whether `StartupNotify=true` was set.
    pub const fn uses_startup_notify(&self) -> bool {
        self.startup_notify
    }

    /// The semicolon-separated `Categories=` list.
    pub fn categories(&self) -> impl Iterator<Item = &str> + '_ {
        split_semicolons(&self.categories[..self.categories_len])
    }

    /// The semicolon-separated `OnlyShowIn=` list.
    pub fn only_show_in(&self) -> impl Iterator<Item = &str> + '_ {
        split_semicolons(&self.only_show_in[..self.only_show_in_len])
    }

    /// The semicolon-separated `NotShowIn=` list.
    pub fn not_show_in(&self) -> impl Iterator<Item = &str> + '_ {
        split_semicolons(&self.not_show_in[..self.not_show_in_len])
    }

    /// Whether the entry should be shown for a desktop named `desktop`.
    ///
    /// The freedesktop rule: `NotShowIn` excluding wins; otherwise
    /// `OnlyShowIn` must contain the desktop when it is set at all.
    pub fn shown_in(&self, desktop: &str) -> bool {
        for excluded in self.not_show_in() {
            if excluded == desktop {
                return false;
            }
        }
        let mut any = false;
        for included in self.only_show_in() {
            any = true;
            if included == desktop {
                return true;
            }
        }
        !any
    }

    /// Whether the entry is a launchable application.
    pub const fn is_application(&self) -> bool {
        matches!(self.kind, EntryKind::Application)
    }

    /// Whether the entry holds a parseable `Exec=` line.
    pub const fn is_launchable(&self) -> bool {
        self.exec_stripped_len > 0
    }

    /// Expand the `Exec=` line into an argument vector, with quoting,
    /// escapes, and field codes resolved.
    ///
    /// `desktop_path` is the path of the `.desktop` file itself (`%k`);
    /// `file` feeds `%f`/`%F`; `url` feeds `%u`/`%U`. Returns `None` when
    /// the command line is empty once field codes are removed — a bare
    /// `%f` with no file to feed it is not a command.
    pub fn expand_exec(&self, desktop_path: &str, file: &str, url: &str) -> Option<ExecArgs> {
        let raw = self.exec_raw()?;
        let mut args = ExecArgs::empty();

        for token in tokenize_exec(raw.as_bytes()) {
            let mut arg = [0u8; MAX_ARG];
            let mut len = 0usize;
            let mut index = 0usize;
            let mut pushed_any = false;
            while index < token.len {
                let byte = token.bytes[index];
                match byte {
                    b'%' if index + 1 < token.len => {
                        let code = token.bytes[index + 1];
                        index += 2;
                        match code {
                            b'%' => {
                                len = push_byte(&mut arg, len, b'%');
                                pushed_any = true;
                            }
                            b'f' | b'F' => {
                                let fed = push_text(&mut arg, len, file.as_bytes());
                                len = fed;
                                pushed_any = true;
                            }
                            b'u' | b'U' => {
                                let fed = push_text(&mut arg, len, url.as_bytes());
                                len = fed;
                                pushed_any = true;
                            }
                            b'c' => {
                                let fed = push_text(&mut arg, len, self.name_bytes());
                                len = fed;
                                pushed_any = true;
                            }
                            b'k' => {
                                let fed = push_text(&mut arg, len, desktop_path.as_bytes());
                                len = fed;
                                pushed_any = true;
                            }
                            b'i' => {
                                // `--icon <icon>` expands to two arguments.
                                if let Some(icon) = self.icon() {
                                    let mut icon_arg = [0u8; MAX_ARG];
                                    let icon_len = push_text(&mut icon_arg, 0, b"--icon");
                                    args.push(&icon_arg[..icon_len]);
                                    let icon_len = push_text(&mut icon_arg, 0, icon.as_bytes());
                                    args.push(&icon_arg[..icon_len]);
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        len = push_byte(&mut arg, len, byte);
                        pushed_any = true;
                        index += 1;
                    }
                }
            }
            if pushed_any {
                args.push(&arg[..len]);
            }
        }
        if args.count == 0 {
            return None;
        }
        Some(args)
    }
}

/// An expanded `Exec=` line: up to [`MAX_ARGV`] arguments, each up to
/// [`MAX_ARG`] bytes, all inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecArgs {
    args: [[u8; MAX_ARG]; MAX_ARGV],
    lens: [usize; MAX_ARGV],
    count: usize,
}

impl ExecArgs {
    /// An empty argument vector, ready for [`ExecArgs::push_arg`].
    pub const fn empty() -> Self {
        Self {
            args: [[0; MAX_ARG]; MAX_ARGV],
            lens: [0; MAX_ARGV],
            count: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.push_arg(bytes);
    }

    /// Append one argument. Overlong arguments are clipped; a full vector
    /// drops the rest.
    pub fn push_arg(&mut self, bytes: &[u8]) {
        if self.count >= MAX_ARGV {
            return;
        }
        let take = bytes.len().min(MAX_ARG);
        self.args[self.count][..take].copy_from_slice(&bytes[..take]);
        self.lens[self.count] = take;
        self.count += 1;
    }

    /// How many arguments the expansion produced.
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the expansion produced no arguments.
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The argument at `index`, or `None`.
    pub fn arg(&self, index: usize) -> Option<&str> {
        if index >= self.count {
            return None;
        }
        core::str::from_utf8(&self.args[index][..self.lens[index]]).ok()
    }

    /// The program name: the first argument.
    pub fn program(&self) -> Option<&str> {
        self.arg(0)
    }
}

/// The unlocalised `Name=` bytes.
impl DesktopFile {
    fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// The stripped `Exec=` bytes.
    fn exec_stripped_bytes(&self) -> &[u8] {
        &self.exec_stripped[..self.exec_stripped_len]
    }
}

/// One whitespace-free token produced by [`tokenize_exec`], with the
/// quotes and escapes already removed.
#[derive(Clone, Copy, Debug)]
struct ExecToken {
    bytes: [u8; MAX_ARG],
    len: usize,
}

/// Split an `Exec=` line into tokens, honouring double quotes (with
/// backslash escapes), single quotes (literal, except `\'`), and
/// backslash escapes outside quotes.
fn tokenize_exec(line: &[u8]) -> impl Iterator<Item = ExecToken> + '_ {
    let mut tokens = [ExecToken {
        bytes: [0; MAX_ARG],
        len: 0,
    }; MAX_ARGV];
    let mut count = 0usize;
    let mut token = ExecToken {
        bytes: [0; MAX_ARG],
        len: 0,
    };
    let mut in_double = false;
    let mut in_single = false;
    let mut index = 0usize;

    while index < line.len() {
        let byte = line[index];
        if (byte == b' ' || byte == b'\t') && !in_double && !in_single {
            if token.len > 0 && count < MAX_ARGV {
                tokens[count] = token;
                count += 1;
                token = ExecToken {
                    bytes: [0; MAX_ARG],
                    len: 0,
                };
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' if !in_single => {
                in_double = !in_double;
                index += 1;
            }
            b'\'' if !in_double => {
                in_single = !in_single;
                index += 1;
            }
            b'\\' if !in_single && index + 1 < line.len() => {
                token.bytes[token.len] = line[index + 1];
                token.len += 1;
                index += 2;
            }
            _ => {
                if token.len < MAX_ARG {
                    token.bytes[token.len] = byte;
                    token.len += 1;
                }
                index += 1;
            }
        }
    }
    if token.len > 0 && count < MAX_ARGV {
        tokens[count] = token;
        count += 1;
    }

    tokens.into_iter().take(count)
}

/// Append one byte to an argument being built.
fn push_byte(arg: &mut [u8; MAX_ARG], len: usize, byte: u8) -> usize {
    if len < MAX_ARG {
        arg[len] = byte;
        len + 1
    } else {
        len
    }
}

/// Append a byte string to an argument being built.
fn push_text(arg: &mut [u8; MAX_ARG], len: usize, text: &[u8]) -> usize {
    let mut length = len;
    for &byte in text {
        length = push_byte(arg, length, byte);
    }
    length
}

/// Iterate the semicolon-separated members of a list field.
fn split_semicolons(bytes: &[u8]) -> impl Iterator<Item = &str> + '_ {
    bytes
        .split(|&b| b == b';')
        .filter(|part| !part.is_empty())
        .map(|part| core::str::from_utf8(part).unwrap_or(""))
}

/// Parse a freedesktop boolean: `true`/`1` mean true; anything else —
/// including the spec's invalid spellings — is false.
fn parse_bool(value: &[u8]) -> bool {
    value.eq_ignore_ascii_case(b"true") || value == b"1"
}

/// Copy a value into a fixed field, clipping to the field's capacity.
fn set_field(slot: &mut [u8; MAX_FIELD], len: &mut usize, value: &[u8]) {
    let take = cmp::min(value.len(), MAX_FIELD);
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

/// Remove freedesktop field codes (`%f`, `%u`, `%%`, ...) from a command
/// line, writing the remainder into `out` and returning its length:
/// `my-app %f --fast %u` becomes `my-app --fast`. `%%` collapses to a
/// literal `%`.
fn strip_field_codes(bytes: &[u8], out: &mut [u8; MAX_FIELD]) -> usize {
    let mut length = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 1 < bytes.len() {
            // `%%` is a literal percent, not a field code; any other
            // `%x` pair is a code and disappears.
            if bytes[index + 1] == b'%' && length < MAX_FIELD {
                out[length] = b'%';
                length += 1;
            }
            index += 2;
            continue;
        }
        if length < MAX_FIELD {
            out[length] = bytes[index];
            length += 1;
        }
        index += 1;
    }
    trim(&out[..length]).len()
}

/// Collapse runs of spaces/tabs left behind by removed field codes.
fn collapse_spaces(bytes: &[u8], out: &mut [u8; MAX_FIELD]) -> usize {
    let mut length = 0usize;
    let mut previous_was_space = false;
    for &byte in bytes {
        let is_space = byte == b' ' || byte == b'\t';
        if is_space && previous_was_space {
            continue;
        }
        if length < MAX_FIELD {
            out[length] = byte;
            length += 1;
        }
        previous_was_space = is_space;
    }
    length
}

/// `trim` for a byte slice.
fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len()
        && (bytes[start] == b' ' || bytes[start] == b'\t' || bytes[start] == b'\r')
    {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start
        && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t' || bytes[end - 1] == b'\r')
    {
        end -= 1;
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Desktop Entry]
Version=1.0
Type=Application
Encoding=UTF-8
Exec=ede-launch --launch terminal %u
Icon=utilities-terminal
Name=Terminal
Comment=X11 terminal
Categories=System;
";

    #[test]
    fn parses_a_standard_entry() {
        let entry = DesktopFile::parse(SAMPLE.as_bytes());
        assert_eq!(entry.kind, EntryKind::Application);
        assert_eq!(entry.exec(), Some("ede-launch --launch terminal"));
        assert_eq!(entry.name(), Some("Terminal"));
        assert_eq!(entry.icon(), Some("utilities-terminal"));
        assert_eq!(entry.comment(), Some("X11 terminal"));
        assert!(entry.is_application());
        assert!(entry.is_launchable());
    }

    #[test]
    fn field_codes_are_stripped() {
        let entry = DesktopFile::parse(b"[Desktop Entry]\nExec=my-app %f --fast %u\n");
        assert_eq!(entry.exec(), Some("my-app --fast"));
        // The raw line keeps the codes for `expand_exec`.
        assert_eq!(entry.exec_raw(), Some("my-app %f --fast %u"));
    }

    #[test]
    fn localised_names_fall_back_to_the_unlocalised_one() {
        let entry = DesktopFile::parse(b"[Desktop Entry]\nName[de]=Terminal\nName[fr]=\n");
        // The localised-only file keeps the de value when no plain Name exists.
        assert_eq!(entry.name(), Some("Terminal"));
    }

    #[test]
    fn a_file_without_exec_is_not_launchable() {
        let entry = DesktopFile::parse(b"[Desktop Entry]\nType=Application\n");
        assert!(!entry.is_launchable());
    }

    #[test]
    fn keys_in_other_groups_do_not_leak() {
        let entry = DesktopFile::parse(
            b"[Desktop Entry]\nName=Terminal\n[Some Action]\nName=Not Terminal\n",
        );
        assert_eq!(entry.name(), Some("Terminal"));
    }

    #[test]
    fn quoting_and_escapes_survive_tokenization() {
        let entry = DesktopFile::parse(
            b"[Desktop Entry]\nType=Application\nExec=my-app \"two words\" \\'lit\\' %f\n",
        );
        let args = entry
            .expand_exec("/usr/share/app/thing.desktop", "photo.png", "")
            .unwrap();
        assert_eq!(args.program(), Some("my-app"));
        assert_eq!(args.arg(1), Some("two words"));
        assert_eq!(args.arg(2), Some("'lit'"));
        assert_eq!(args.arg(3), Some("photo.png"));
        assert_eq!(args.len(), 4);
    }

    #[test]
    fn percent_percent_is_a_literal_percent() {
        let entry = DesktopFile::parse(b"[Desktop Entry]\nExec=100%%-pure\n");
        assert_eq!(entry.exec(), Some("100%-pure"));
    }

    #[test]
    fn i_code_expands_to_the_icon_pair() {
        let entry = DesktopFile::parse(
            b"[Desktop Entry]\nType=Application\nName=Term\nIcon=utilities-terminal\nExec=term %i\n",
        );
        let args = entry.expand_exec("", "", "").unwrap();
        assert_eq!(args.arg(1), Some("--icon"));
        assert_eq!(args.arg(2), Some("utilities-terminal"));
    }

    #[test]
    fn c_code_expands_to_the_name() {
        let entry =
            DesktopFile::parse(b"[Desktop Entry]\nType=Application\nName=My Editor\nExec=run %c\n");
        let args = entry.expand_exec("", "", "").unwrap();
        assert_eq!(args.arg(1), Some("My Editor"));
    }

    #[test]
    fn path_terminal_and_categories_are_read() {
        let entry = DesktopFile::parse(
            b"[Desktop Entry]\nType=Application\nPath=/home/nova/work\nTerminal=true\nCategories=System;Utility;\nExec=x\n",
        );
        assert_eq!(entry.path(), Some("/home/nova/work"));
        assert!(entry.wants_terminal());
        let mut categories = ["", ""];
        for (index, item) in entry.categories().enumerate() {
            if index < 2 {
                categories[index] = item;
            }
        }
        assert_eq!(categories, ["System", "Utility"]);
    }

    #[test]
    fn show_in_rules_are_enforced() {
        let only = DesktopFile::parse(b"[Desktop Entry]\nOnlyShowIn=EDE;GNOME;\nExec=x\n");
        assert!(only.shown_in("EDE"));
        assert!(!only.shown_in("KDE"));
        let not = DesktopFile::parse(b"[Desktop Entry]\nNotShowIn=EDE;\nExec=x\n");
        assert!(!not.shown_in("EDE"));
        assert!(not.shown_in("GNOME"));
        let plain = DesktopFile::parse(b"[Desktop Entry]\nExec=x\n");
        assert!(plain.shown_in("anything"));
    }

    #[test]
    fn hidden_and_no_display_are_read() {
        let entry = DesktopFile::parse(
            b"[Desktop Entry]\nType=Application\nHidden=true\nNoDisplay=true\nExec=x\n",
        );
        assert!(entry.is_hidden());
        assert!(entry.is_no_display());
    }
}
