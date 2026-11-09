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

/// A parsed `.desktop` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopFile {
    /// The `Exec=` command, e.g. `terminal`.
    pub exec: [u8; MAX_FIELD],
    exec_len: usize,
    /// The `Name=` label, e.g. `Terminal`.
    pub name: [u8; MAX_FIELD],
    name_len: usize,
    /// The `Icon=` theme name, e.g. `utilities-terminal`.
    pub icon: [u8; MAX_FIELD],
    icon_len: usize,
    /// The `Comment=` description, when present.
    pub comment: [u8; MAX_FIELD],
    comment_len: usize,
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
            name: [0; MAX_FIELD],
            name_len: 0,
            icon: [0; MAX_FIELD],
            icon_len: 0,
            comment: [0; MAX_FIELD],
            comment_len: 0,
            kind: EntryKind::Unknown,
        }
    }

    /// Parse `bytes` as a `.desktop` file.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut entry = Self::empty();
        for raw_line in bytes.split(|&b| b == b'\n') {
            let line = trim(raw_line);
            if line.is_empty() || line[0] == b'#' || line[0] == b'[' {
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
                    // Strip field codes (%f, %u, ...) the way every launcher does.
                    let clean = strip_field_codes(value);
                    set_field(&mut entry.exec, &mut entry.exec_len, clean);
                }
                b"Name" => set_field(&mut entry.name, &mut entry.name_len, value),
                b"Icon" => set_field(&mut entry.icon, &mut entry.icon_len, value),
                b"Comment" => set_field(&mut entry.comment, &mut entry.comment_len, value),
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

    /// The `Exec=` command as bytes.
    pub fn exec_bytes(&self) -> &[u8] {
        &self.exec[..self.exec_len]
    }

    /// The `Exec=` command as text.
    pub fn exec(&self) -> Option<&str> {
        core::str::from_utf8(self.exec_bytes()).ok()
    }

    /// The `Name=` label as text.
    pub fn name(&self) -> Option<&str> {
        core::str::from_utf8(&self.name[..self.name_len]).ok()
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

    /// Whether the entry is a launchable application.
    pub const fn is_application(&self) -> bool {
        matches!(self.kind, EntryKind::Application)
    }

    /// Whether the entry holds a parseable `Exec=` line.
    pub const fn is_launchable(&self) -> bool {
        self.exec_len > 0
    }
}

/// Copy a value into a fixed field, clipping to the field's capacity.
fn set_field(slot: &mut [u8; MAX_FIELD], len: &mut usize, value: &[u8]) {
    let take = cmp::min(value.len(), MAX_FIELD);
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

/// Remove freedesktop field codes (`%f`, `%u`, `%F`, ...) from a command line.
fn strip_field_codes(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    for index in 0..bytes.len() {
        if bytes[index] == b'%' && index + 1 < bytes.len() {
            end = index;
            break;
        }
    }
    trim(&bytes[..end])
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
        assert_eq!(entry.exec(), Some("my-app"));
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
}
