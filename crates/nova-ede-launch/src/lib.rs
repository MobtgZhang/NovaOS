//! Rust port of `ede-launch`, EDE's application launcher.
//!
//! The launcher shows a list of applications from the system's `.desktop`
//! files. Typing filters the list; Enter launches the selected entry;
//! Escape closes the dialog. This crate is the model — the filtering, the
//! selection, the keyboard handling — kept separate from the window it is
//! drawn in, so the logic is testable without a framebuffer.
//!
//! EDE launches whatever the `Exec` line says. NovaOS's service names
//! double as the executable names, so a launch is `process::exec(name)`.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::{DesktopFile, ExecArgs};

/// The maximum number of applications the launcher tracks.
pub const MAX_APPS: usize = 64;
/// The maximum length of a filtered list shown at once.
pub const MAX_SHOWN: usize = 8;
/// The longest name this launcher keeps.
pub const MAX_FIELD: usize = 48;
/// The longest command the history remembers.
pub const MAX_HISTORY: usize = 8;

/// One application in the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppEntry {
    name: [u8; MAX_FIELD],
    name_len: usize,
    exec: [u8; MAX_FIELD],
    exec_len: usize,
}

impl AppEntry {
    /// The display name.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }

    /// The `Exec` command.
    pub fn exec(&self) -> &str {
        core::str::from_utf8(&self.exec[..self.exec_len]).unwrap_or("")
    }
}

/// The four preferred-application slots EDE keeps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreferredKind {
    /// The web browser.
    Browser,
    /// The mail reader.
    Mail,
    /// The file manager.
    FileManager,
    /// The terminal emulator.
    Terminal,
}

/// One remembered Run Command line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HistoryEntry {
    command: [u8; MAX_FIELD],
    command_len: usize,
}

impl HistoryEntry {
    /// The remembered command text.
    pub fn command(&self) -> &str {
        core::str::from_utf8(&self.command[..self.command_len]).unwrap_or("")
    }
}

/// A resolved launch: what to run, its arguments, and how.
#[derive(Clone, Copy, Debug)]
pub struct LaunchRequest {
    /// The expanded argument vector, argument 0 being the program.
    args: ExecArgs,
    /// The `Path=` working directory, when the entry set one.
    working_dir: [u8; MAX_FIELD],
    working_dir_len: usize,
    /// Whether the program should run inside a terminal window.
    pub in_terminal: bool,
    /// Whether the program speaks the startup-notification protocol.
    pub startup_notify: bool,
}

impl LaunchRequest {
    /// The program name: argument zero.
    pub fn program(&self) -> Option<&str> {
        self.args.program()
    }

    /// The argument at `index`, or `None`.
    pub fn arg(&self, index: usize) -> Option<&str> {
        self.args.arg(index)
    }

    /// How many arguments the launch carries.
    pub fn argc(&self) -> usize {
        self.args.len()
    }

    /// The working directory from `Path=`, when set.
    pub fn working_dir(&self) -> Option<&str> {
        if self.working_dir_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.working_dir[..self.working_dir_len]).ok()
    }
}

/// The launcher state: all entries, the current filter, and the selection.
#[derive(Clone, Copy, Debug)]
pub struct Launchpad {
    entries: [Option<AppEntry>; MAX_APPS],
    count: usize,
    filter: [u8; MAX_FIELD],
    filter_len: usize,
    selected: usize,
    /// The preferred application for each [`PreferredKind`], by service
    /// name.
    preferred_browser: [u8; MAX_FIELD],
    preferred_browser_len: usize,
    preferred_mail: [u8; MAX_FIELD],
    preferred_mail_len: usize,
    preferred_files: [u8; MAX_FIELD],
    preferred_files_len: usize,
    preferred_terminal: [u8; MAX_FIELD],
    preferred_terminal_len: usize,
    /// The remembered Run Command lines, newest first.
    history: [HistoryEntry; MAX_HISTORY],
    history_count: usize,
}

impl Launchpad {
    /// An empty launcher.
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_APPS],
            count: 0,
            filter: [0; MAX_FIELD],
            filter_len: 0,
            selected: 0,
            preferred_browser: [0; MAX_FIELD],
            preferred_browser_len: 0,
            preferred_mail: [0; MAX_FIELD],
            preferred_mail_len: 0,
            preferred_files: [0; MAX_FIELD],
            preferred_files_len: 0,
            preferred_terminal: [0; MAX_FIELD],
            preferred_terminal_len: 0,
            history: [HistoryEntry {
                command: [0; MAX_FIELD],
                command_len: 0,
            }; MAX_HISTORY],
            history_count: 0,
        }
    }

    /// Add one application from a `.desktop` file's contents.
    /// Returns false for non-launchable entries.
    pub fn add_desktop(&mut self, contents: &[u8]) -> bool {
        let file = DesktopFile::parse(contents);
        if !file.is_application() || !file.is_launchable() {
            return false;
        }
        let slot = self.count;
        if slot >= MAX_APPS {
            return false;
        }
        let mut entry = AppEntry {
            name: [0; MAX_FIELD],
            name_len: 0,
            exec: [0; MAX_FIELD],
            exec_len: 0,
        };
        copy_field(
            &mut entry.name,
            &mut entry.name_len,
            file.name().unwrap_or("?").as_bytes(),
        );
        copy_field(&mut entry.exec, &mut entry.exec_len, file.exec_bytes());
        self.entries[slot] = Some(entry);
        self.count += 1;
        true
    }

    /// The number of applications registered.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Set one preferred-application slot by service name.
    pub fn set_preferred(&mut self, kind: PreferredKind, service: &str) -> bool {
        let (slot, length) = match kind {
            PreferredKind::Browser => {
                (&mut self.preferred_browser, &mut self.preferred_browser_len)
            }
            PreferredKind::Mail => (&mut self.preferred_mail, &mut self.preferred_mail_len),
            PreferredKind::FileManager => {
                (&mut self.preferred_files, &mut self.preferred_files_len)
            }
            PreferredKind::Terminal => (
                &mut self.preferred_terminal,
                &mut self.preferred_terminal_len,
            ),
        };
        let bytes = service.as_bytes();
        if bytes.len() > MAX_FIELD {
            return false;
        }
        slot[..bytes.len()].copy_from_slice(bytes);
        *length = bytes.len();
        true
    }

    /// The preferred application for `kind`, if one is set.
    pub fn preferred(&self, kind: PreferredKind) -> Option<&str> {
        let (slot, length) = match kind {
            PreferredKind::Browser => (&self.preferred_browser, self.preferred_browser_len),
            PreferredKind::Mail => (&self.preferred_mail, self.preferred_mail_len),
            PreferredKind::FileManager => (&self.preferred_files, self.preferred_files_len),
            PreferredKind::Terminal => (&self.preferred_terminal, self.preferred_terminal_len),
        };
        if length == 0 {
            return None;
        }
        core::str::from_utf8(&slot[..length]).ok()
    }

    /// Resolve free text into a launch: a `.desktop` path runs its entry;
    /// a URL or mail address routes to the preferred browser or reader;
    /// anything else is a command line.
    ///
    /// This is `ede-launch`'s argument triage, kept as a pure function so
    /// the shell can drive it and the tests can pin it.
    pub fn resolve(&self, text: &str) -> Option<LaunchRequest> {
        if text.ends_with(".desktop") {
            return None; // the caller reads the file; see `resolve_entry`
        }
        if looks_like_mail(text) {
            let mail = self.preferred(PreferredKind::Mail)?;
            return Some(simple_request(mail));
        }
        if looks_like_url(text) {
            let browser = self.preferred(PreferredKind::Browser)?;
            return Some(simple_request(browser));
        }
        None
    }

    /// Build the launch for a parsed `.desktop` entry.
    ///
    /// The entry's `Exec` line is expanded with no file and no URL —
    /// desktop-driven launches carry neither; `Path` becomes the working
    /// directory and `Terminal`/`StartupNotify` ride along.
    pub fn resolve_entry(&self, contents: &[u8]) -> Option<LaunchRequest> {
        let file = DesktopFile::parse(contents);
        if !file.is_application() || !file.is_launchable() {
            return None;
        }
        let args = file.expand_exec("", "", "")?;
        let mut working_dir = [0u8; MAX_FIELD];
        let mut working_dir_len = 0usize;
        if let Some(path) = file.path() {
            let bytes = path.as_bytes();
            let take = bytes.len().min(MAX_FIELD);
            working_dir[..take].copy_from_slice(&bytes[..take]);
            working_dir_len = take;
        }
        Some(LaunchRequest {
            args,
            working_dir,
            working_dir_len,
            in_terminal: file.wants_terminal(),
            startup_notify: file.uses_startup_notify(),
        })
    }

    /// Remember one Run Command line, newest first, capped at
    /// [`MAX_HISTORY`]. Duplicates move to the front rather than
    /// duplicating.
    pub fn remember(&mut self, command: &str) -> bool {
        let bytes = command.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_FIELD {
            return false;
        }
        // Move an existing duplicate to the front.
        for index in 0..self.history_count {
            if self.history[index].command() == command {
                let found = self.history[index];
                for slot in (0..index).rev() {
                    self.history[slot + 1] = self.history[slot];
                }
                self.history[0] = found;
                return true;
            }
        }
        if self.history_count >= MAX_HISTORY {
            self.history_count -= 1;
        }
        for slot in (0..self.history_count).rev() {
            self.history[slot + 1] = self.history[slot];
        }
        self.history_count += 1;
        self.history[0].command[..bytes.len()].copy_from_slice(bytes);
        self.history[0].command_len = bytes.len();
        true
    }

    /// The remembered command at `index` (0 is the newest).
    pub fn history(&self, index: usize) -> Option<&str> {
        if index >= self.history_count {
            return None;
        }
        Some(self.history[index].command())
    }

    /// How many commands are remembered.
    pub fn history_count(&self) -> usize {
        self.history_count
    }

    /// The filter text as typed.
    pub fn filter(&self) -> &str {
        core::str::from_utf8(&self.filter[..self.filter_len]).unwrap_or("")
    }

    /// Type one character. Returns true when the list changed.
    pub fn type_char(&mut self, byte: u8) -> bool {
        if !(0x20..0x7f).contains(&byte) || self.filter_len >= MAX_FIELD {
            return false;
        }
        self.filter[self.filter_len] = byte;
        self.filter_len += 1;
        self.selected = 0;
        true
    }

    /// Backspace: remove the last filter character.
    pub fn backspace(&mut self) -> bool {
        if self.filter_len == 0 {
            return false;
        }
        self.filter_len -= 1;
        self.selected = 0;
        true
    }

    /// Clear the filter.
    pub fn clear(&mut self) {
        self.filter_len = 0;
        self.selected = 0;
    }

    /// The number of entries matching the current filter.
    pub fn matches(&self) -> usize {
        self.matching_indices().count()
    }

    /// Iterate the indices of entries matching the filter.
    pub fn matching_indices(&self) -> impl Iterator<Item = usize> + '_ {
        let needle = self.filter();
        (0..self.count).filter(move |&index| {
            let Some(entry) = &self.entries[index] else {
                return false;
            };
            if needle.is_empty() {
                return true;
            }
            contains_ignore_case(entry.name().as_bytes(), needle.as_bytes())
        })
    }

    /// The currently selected matching entry, if any.
    pub fn selected_entry(&self) -> Option<AppEntry> {
        self.matching_indices()
            .nth(self.selected)
            .and_then(|index| self.entries[index])
    }

    /// The application at `index` — the start menu's row lookup.
    pub fn entry(&self, index: usize) -> Option<AppEntry> {
        self.entries.get(index).copied().flatten()
    }

    /// Move the selection by `delta` (wrapping).
    pub fn move_selection(&mut self, delta: i32) {
        let matches = self.matches();
        if matches == 0 {
            self.selected = 0;
            return;
        }
        let mut next = self.selected as i32 + delta;
        while next < 0 {
            next += matches as i32;
        }
        self.selected = next as usize % matches;
    }

    /// The selected application, if any.
    pub fn chosen(&self) -> Option<AppEntry> {
        self.selected_entry()
    }

    /// The selection's position among the matching entries — what the
    /// start menu highlights while the user types.
    pub const fn selected_index(&self) -> usize {
        self.selected
    }
}

impl Default for Launchpad {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether free text looks like a URL — scheme-prefixed or dotted host.
fn looks_like_url(text: &str) -> bool {
    if text.contains("://") {
        return true;
    }
    // A bare host with a dot and no spaces: `example.com`.
    !text.contains(' ') && text.contains('.') && !text.starts_with('.')
}

/// Whether free text looks like a mail address: one `@`, no spaces.
fn looks_like_mail(text: &str) -> bool {
    let mut at = 0;
    for byte in text.bytes() {
        if byte == b'@' {
            at += 1;
        }
    }
    at == 1 && !text.contains(' ')
}

/// A launch with a plain command and no arguments.
fn simple_request(command: &str) -> LaunchRequest {
    let mut args = ExecArgs::empty();
    args.push_arg(command.as_bytes());
    LaunchRequest {
        args,
        working_dir: [0; MAX_FIELD],
        working_dir_len: 0,
        in_terminal: false,
        startup_notify: false,
    }
}

/// Copy a byte string into a fixed field.
fn copy_field(slot: &mut [u8; MAX_FIELD], len: &mut usize, value: &[u8]) {
    let take = value.len().min(MAX_FIELD);
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

/// Case-insensitive substring match.
fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERM: &str = "[Desktop Entry]\nType=Application\nName=Terminal\nExec=terminal\n";
    const FILES: &str = "[Desktop Entry]\nType=Application\nName=File Manager\nExec=file-manager\n";
    const EDITOR: &str = "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor\n";

    fn pad() -> Launchpad {
        let mut pad = Launchpad::new();
        assert!(pad.add_desktop(TERM.as_bytes()));
        assert!(pad.add_desktop(FILES.as_bytes()));
        assert!(pad.add_desktop(EDITOR.as_bytes()));
        pad
    }

    #[test]
    fn empty_filter_matches_everything() {
        let pad = pad();
        assert_eq!(pad.matches(), 3);
    }

    #[test]
    fn typing_narrows_the_list() {
        let mut pad = pad();
        pad.type_char(b't');
        pad.type_char(b'e');
        assert_eq!(pad.matches(), 1);
        pad.clear();
        pad.type_char(b'e');
        assert_eq!(pad.matches(), 3);
    }

    #[test]
    fn selection_wraps() {
        let mut pad = pad();
        pad.move_selection(1);
        assert_eq!(pad.selected, 1);
        pad.move_selection(-1);
        assert_eq!(pad.selected, 0);
        pad.move_selection(-1);
        assert_eq!(pad.selected, 2);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let mut pad = pad();
        pad.type_char(b'T');
        pad.type_char(b'E');
        pad.type_char(b'R');
        assert_eq!(pad.matches(), 1);
    }

    #[test]
    fn preferred_slots_hold_service_names() {
        let mut pad = pad();
        assert!(pad.set_preferred(PreferredKind::Browser, "nova-web"));
        assert!(pad.set_preferred(PreferredKind::Terminal, "terminal"));
        assert_eq!(pad.preferred(PreferredKind::Browser), Some("nova-web"));
        assert_eq!(pad.preferred(PreferredKind::Terminal), Some("terminal"));
        assert_eq!(pad.preferred(PreferredKind::Mail), None);
    }

    #[test]
    fn a_url_routes_to_the_browser() {
        let mut pad = pad();
        pad.set_preferred(PreferredKind::Browser, "nova-web");
        let request = pad.resolve("https://example.com").unwrap();
        assert_eq!(request.program(), Some("nova-web"));
        assert_eq!(request.argc(), 1);
    }

    #[test]
    fn a_mail_address_routes_to_the_reader() {
        let mut pad = pad();
        pad.set_preferred(PreferredKind::Mail, "nova-mail");
        let request = pad.resolve("user@example.com").unwrap();
        assert_eq!(request.program(), Some("nova-mail"));
    }

    #[test]
    fn a_desktop_path_is_not_resolved_without_the_file() {
        let pad = pad();
        assert!(pad.resolve("apps/terminal.desktop").is_none());
    }

    #[test]
    fn an_entry_resolves_to_an_argument_vector() {
        let mut pad = pad();
        let with_args = "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor /home/nova/notes.txt\nTerminal=true\nStartupNotify=true\n";
        assert!(pad.add_desktop(with_args.as_bytes()));
        let request = pad.resolve_entry(with_args.as_bytes()).unwrap();
        assert_eq!(request.program(), Some("editor"));
        assert_eq!(request.arg(1), Some("/home/nova/notes.txt"));
        assert!(request.in_terminal);
        assert!(request.startup_notify);
    }

    #[test]
    fn a_non_application_entry_does_not_resolve() {
        let pad = Launchpad::new();
        let link = b"[Desktop Entry]\nType=Link\nName=Docs\nURL=https://example.com\n";
        assert!(pad.resolve_entry(link).is_none());
    }

    #[test]
    fn history_is_newest_first_and_deduplicates() {
        let mut pad = pad();
        assert!(pad.remember("editor notes.txt"));
        assert!(pad.remember("ls -l"));
        assert_eq!(pad.history(0), Some("ls -l"));
        assert_eq!(pad.history(1), Some("editor notes.txt"));
        assert!(pad.remember("editor notes.txt"));
        assert_eq!(pad.history(0), Some("editor notes.txt"));
        assert_eq!(pad.history_count(), 2);
    }

    #[test]
    fn history_is_capped() {
        let mut pad = pad();
        for index in 0..(MAX_HISTORY + 4) {
            let mut command = [0u8; 8];
            let label = b"cmd";
            command[..label.len()].copy_from_slice(label);
            command[label.len()] = b'0' + (index % 10) as u8;
            let text = core::str::from_utf8(&command[..label.len() + 1]).unwrap();
            assert!(pad.remember(text));
        }
        assert_eq!(pad.history_count(), MAX_HISTORY);
    }
}
