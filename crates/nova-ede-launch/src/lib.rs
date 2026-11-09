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

use nova_edelib::DesktopFile;

/// The maximum number of applications the launcher tracks.
pub const MAX_APPS: usize = 64;
/// The maximum length of a filtered list shown at once.
pub const MAX_SHOWN: usize = 8;
/// The longest name this launcher keeps.
pub const MAX_FIELD: usize = 48;

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

/// The launcher state: all entries, the current filter, and the selection.
#[derive(Clone, Copy, Debug)]
pub struct Launchpad {
    entries: [Option<AppEntry>; MAX_APPS],
    count: usize,
    filter: [u8; MAX_FIELD],
    filter_len: usize,
    selected: usize,
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
}

impl Default for Launchpad {
    fn default() -> Self {
        Self::new()
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
}
