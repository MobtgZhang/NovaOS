//! The sectioned key/value store behind `~/.ede/*.conf`.
//!
//! EDE stores its settings in INI-flavoured files:
//!
//! ```text
//! [Icons]
//! label_fontsize=12
//! one_click_exec=false
//!
//! [Panel]
//! applets=start-menu,quick-launch,taskbar,systray
//! ```
//!
//! This module parses and produces exactly that shape. It is small on
//! purpose: a configuration format that grew subsections, includes, and
//! quoting rules would stop being greppable, and greppability is the
//! feature.

use core::cmp;

/// The maximum number of sections a file may hold.
pub const MAX_SECTIONS: usize = 16;
/// The maximum number of keys per section.
pub const MAX_KEYS: usize = 64;
/// The longest section name this parser keeps.
pub const MAX_NAME: usize = 48;
/// The longest value this parser keeps.
pub const MAX_VALUE: usize = 256;

/// One stored key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    /// The key name; only `[..len]` bytes are meaningful.
    name: [u8; MAX_NAME],
    name_len: usize,
    /// The value; only `[..len]` bytes are meaningful.
    value: [u8; MAX_VALUE],
    value_len: usize,
}

impl Key {
    const EMPTY: Key = Key {
        name: [0; MAX_NAME],
        name_len: 0,
        value: [0; MAX_VALUE],
        value_len: 0,
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    fn value(&self) -> &[u8] {
        &self.value[..self.value_len]
    }
}

/// One section: a name plus its keys.
#[derive(Clone, Copy, Debug)]
struct Section {
    /// The section name; only `[..len]` bytes are meaningful.
    name: [u8; MAX_NAME],
    name_len: usize,
    keys: [Key; MAX_KEYS],
    key_count: usize,
}

impl Section {
    const EMPTY: Section = Section {
        name: [0; MAX_NAME],
        name_len: 0,
        keys: [Key::EMPTY; MAX_KEYS],
        key_count: 0,
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// The parsed configuration file.
///
/// A value, not a heap allocation: everything is inline, and everything is
/// copied out on read, so a long-lived handle is never needed.
#[derive(Clone, Copy)]
pub struct EdeConfig {
    sections: [Section; MAX_SECTIONS],
    section_count: usize,
}

impl EdeConfig {
    /// An empty configuration.
    pub const fn new() -> Self {
        Self {
            sections: [Section::EMPTY; MAX_SECTIONS],
            section_count: 0,
        }
    }

    /// Parse `bytes` as an INI document. Unknown lines are ignored rather
    /// than rejected — a config file with one bad line should degrade one
    /// entry, not the whole session.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut config = Self::new();
        let mut current: Option<usize> = None;

        for raw_line in bytes.split(|&b| b == b'\n') {
            let line = trim(raw_line);
            if line.is_empty() || line[0] == b'#' || line[0] == b';' {
                continue;
            }
            if line[0] == b'[' {
                let end = line.iter().position(|&b| b == b']').unwrap_or(line.len());
                let name = trim(&line[1..end]);
                if name.is_empty() {
                    continue;
                }
                let slot = config.section_slot(name);
                current = slot;
                continue;
            }
            let Some(equals) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key = trim(&line[..equals]);
            let value = trim(&line[equals + 1..]);
            let Some(section_index) = current else {
                continue;
            };
            config.set_key(section_index, key, value);
        }
        config
    }

    /// Find the section index for `name`, creating it if absent.
    fn section_slot(&mut self, name: &[u8]) -> Option<usize> {
        for (index, section) in self.sections.iter().enumerate().take(self.section_count) {
            if section.name() == name {
                return Some(index);
            }
        }
        if self.section_count >= MAX_SECTIONS {
            return None;
        }
        let slot = self.section_count;
        self.section_count += 1;
        let section = &mut self.sections[slot];
        let take = cmp::min(name.len(), MAX_NAME);
        section.name[..take].copy_from_slice(&name[..take]);
        section.name_len = take;
        Some(slot)
    }

    /// Store `value` under `key` in the section at `section_index`.
    fn set_key(&mut self, section_index: usize, key: &[u8], value: &[u8]) {
        let section = &mut self.sections[section_index];
        // Overwrite in place when the key already exists.
        for index in 0..section.key_count {
            if section.keys[index].name() == key {
                let take = cmp::min(value.len(), MAX_VALUE);
                section.keys[index].value[..take].copy_from_slice(&value[..take]);
                section.keys[index].value_len = take;
                return;
            }
        }
        if section.key_count >= MAX_KEYS {
            return;
        }
        let slot = section.key_count;
        section.key_count += 1;
        let take_key = cmp::min(key.len(), MAX_NAME);
        section.keys[slot].name[..take_key].copy_from_slice(&key[..take_key]);
        section.keys[slot].name_len = take_key;
        let take_value = cmp::min(value.len(), MAX_VALUE);
        section.keys[slot].value[..take_value].copy_from_slice(&value[..take_value]);
        section.keys[slot].value_len = take_value;
    }

    /// Read the value stored at `section`/`key`, or `None`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        let section_bytes = section.as_bytes();
        let key_bytes = key.as_bytes();
        for (_index, section) in self.sections.iter().enumerate().take(self.section_count) {
            if section.name() != section_bytes {
                continue;
            }
            for key in section.keys.iter().take(section.key_count) {
                if key.name() == key_bytes {
                    return core::str::from_utf8(key.value()).ok();
                }
            }
        }
        None
    }

    /// Whether the value at `section`/`key` equals `expected` (case-insensitive).
    pub fn is(&self, section: &str, key: &str, expected: &str) -> bool {
        match self.get(section, key) {
            Some(value) => value.eq_ignore_ascii_case(expected),
            None => false,
        }
    }

    /// The value at `section`/`key` parsed as a boolean, if it is a known
    /// spelling of true or false.
    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        match self.get(section, key) {
            Some(value)
                if value.eq_ignore_ascii_case("true")
                    || value == "1"
                    || value.eq_ignore_ascii_case("yes") =>
            {
                Some(true)
            }
            Some(value)
                if value.eq_ignore_ascii_case("false")
                    || value == "0"
                    || value.eq_ignore_ascii_case("no") =>
            {
                Some(false)
            }
            _ => None,
        }
    }

    /// The value at `section`/`key` parsed as an unsigned integer.
    pub fn get_u32(&self, section: &str, key: &str) -> Option<u32> {
        let value = self.get(section, key)?;
        let mut result: u32 = 0;
        for byte in value.bytes() {
            if !byte.is_ascii_digit() {
                return None;
            }
            result = result.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
        }
        Some(result)
    }

    /// Write the configuration back out as an INI document.
    ///
    /// Returns the number of bytes written to `out`; extra room is left
    /// alone. The format round-trips through [`EdeConfig::parse`].
    pub fn serialize(&self, out: &mut [u8]) -> usize {
        let mut used = 0usize;
        for section in self.sections.iter().take(self.section_count) {
            used = write_str(out, used, b"[");
            used = write_bytes(out, used, section.name());
            used = write_str(out, used, b"]\n");
            for key in section.keys.iter().take(section.key_count) {
                used = write_bytes(out, used, key.name());
                used = write_str(out, used, b"=");
                used = write_bytes(out, used, key.value());
                used = write_str(out, used, b"\n");
            }
            used = write_str(out, used, b"\n");
        }
        used
    }
}

impl Default for EdeConfig {
    fn default() -> Self {
        Self::new()
    }
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

fn write_str(out: &mut [u8], used: usize, text: &[u8]) -> usize {
    write_bytes(out, used, text)
}

fn write_bytes(out: &mut [u8], used: usize, bytes: &[u8]) -> usize {
    let take = bytes.len().min(out.len().saturating_sub(used));
    out[used..used + take].copy_from_slice(&bytes[..take]);
    used + take
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Panel]
applets = start-menu,quick-launch,taskbar
height=32

[Icons]
label_fontsize=12
one_click_exec=false
# a comment
broken line\n";

    #[test]
    fn parses_sections_and_keys() {
        let config = EdeConfig::parse(SAMPLE.as_bytes());
        assert_eq!(
            config.get("Panel", "applets"),
            Some("start-menu,quick-launch,taskbar")
        );
        assert_eq!(config.get_u32("Panel", "height"), Some(32));
        assert_eq!(config.get_u32("Icons", "label_fontsize"), Some(12));
        assert_eq!(config.get_bool("Icons", "one_click_exec"), Some(false));
        assert_eq!(config.get("Missing", "key"), None);
    }

    #[test]
    fn unknown_lines_are_ignored() {
        let config = EdeConfig::parse(SAMPLE.as_bytes());
        assert_eq!(config.section_count, 2);
    }

    #[test]
    fn round_trips_through_serialize() {
        let config = EdeConfig::parse(SAMPLE.as_bytes());
        let mut buffer = [0u8; 1024];
        let length = config.serialize(&mut buffer);
        let reparsed = EdeConfig::parse(&buffer[..length]);
        assert_eq!(reparsed.get("Panel", "height"), Some("32"));
        assert_eq!(reparsed.get("Icons", "label_fontsize"), Some("12"));
    }

    #[test]
    fn keys_are_overridden_not_duplicated() {
        let config = EdeConfig::parse(b"[A]\nk=1\nk=2\n");
        assert_eq!(config.get("A", "k"), Some("2"));
    }
}
