//! Rust port of `ede-autostart`, EDE's XDG autostart runner.
//!
//! Upstream scans `<user_config>/autostart/` and every
//! `<system_config>/autostart/`, deduplicates by basename (user entries
//! win), honours `Hidden`/`OnlyShowIn`/`NotShowIn`, optionally lists the
//! found programs in a "safe mode" chooser, and runs the selected
//! `Exec` lines. It also has a removable-media mode that looks for
//! `.autorun`/`autorun`/`autorun.sh` and asks before running anything.
//!
//! The scanning and the chooser are pure policy here: the caller walks
//! the directories and hands this crate the file bytes; this crate
//! decides what to run and what to show.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::DesktopFile;

/// How many autostart entries one session tracks.
pub const MAX_ENTRIES: usize = 16;
/// The longest basename kept.
pub const NAME_MAX: usize = 40;
/// The longest `Exec=` line kept.
pub const EXEC_MAX: usize = 64;

/// One autostart candidate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AutostartEntry {
    /// The file's basename — the deduplication key.
    pub basename: [u8; NAME_MAX],
    basename_len: usize,
    /// The `Exec=` line, ready to run.
    pub exec: [u8; EXEC_MAX],
    exec_len: usize,
    /// The `Name=` label for the chooser.
    pub label: [u8; NAME_MAX],
    label_len: usize,
    /// Whether the user chose to run this entry.
    pub chosen: bool,
}

impl AutostartEntry {
    /// The basename.
    pub fn basename(&self) -> &str {
        core::str::from_utf8(&self.basename[..self.basename_len]).unwrap_or("?")
    }

    /// The `Exec=` line.
    pub fn exec(&self) -> &str {
        core::str::from_utf8(&self.exec[..self.exec_len]).unwrap_or("")
    }

    /// The chooser label.
    pub fn label(&self) -> &str {
        core::str::from_utf8(&self.label[..self.label_len]).unwrap_or("?")
    }
}

/// The autostart set.
#[derive(Clone, Copy)]
pub struct Autostart {
    entries: [Option<AutostartEntry>; MAX_ENTRIES],
    count: usize,
}

impl Autostart {
    /// An empty set for an `EDE` session.
    pub fn new() -> Self {
        Self {
            entries: [None; MAX_ENTRIES],
            count: 0,
        }
    }

    /// Offer one autostart file. Later files with the same basename
    /// replace earlier ones — the user directory wins over the system's,
    /// upstream's deduplication rule.
    ///
    /// Entries that are hidden (`Hidden=true`, `NoDisplay=true`) or that
    /// do not show in this desktop are dropped, not stored.
    pub fn offer(&mut self, basename: &[u8], contents: &[u8]) -> bool {
        let file = DesktopFile::parse(contents);
        if file.is_hidden() || file.is_no_display() || !file.shown_in("EDE") {
            return false;
        }
        if !file.is_launchable() {
            return false;
        }
        let Some(exec) = file.exec() else {
            return false;
        };
        // Replace an existing entry with the same basename.
        let existing = (0..self.count).find(|&index| {
            self.entries[index]
                .is_some_and(|entry| slice_eq(&entry.basename[..entry.basename_len], basename))
        });
        let slot = match existing {
            Some(slot) => slot,
            None => {
                if self.count >= MAX_ENTRIES {
                    return false;
                }
                let slot = self.count;
                self.count += 1;
                slot
            }
        };
        let mut entry = AutostartEntry {
            basename: [0; NAME_MAX],
            basename_len: 0,
            exec: [0; EXEC_MAX],
            exec_len: 0,
            label: [0; NAME_MAX],
            label_len: 0,
            chosen: false,
        };
        entry.basename_len = copy(&mut entry.basename, basename);
        entry.exec_len = copy(&mut entry.exec, exec.as_bytes());
        entry.label_len = copy(&mut entry.label, file.name().unwrap_or("?").as_bytes());
        self.entries[slot] = Some(entry);
        true
    }

    /// How many entries survived the filters.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The entry at `index`.
    pub fn entry(&self, index: usize) -> Option<&AutostartEntry> {
        self.entries.get(index)?.as_ref()
    }

    /// Choose every entry — upstream's "Run all".
    pub fn choose_all(&mut self) {
        for entry in self.entries.iter_mut().take(self.count).flatten() {
            entry.chosen = true;
        }
    }

    /// Choose one entry — the chooser's "Run selected".
    pub fn choose(&mut self, index: usize) -> bool {
        match self.entries.get_mut(index) {
            Some(Some(entry)) => {
                entry.chosen = true;
                true
            }
            _ => false,
        }
    }

    /// Cancel the chooser: nothing runs.
    pub fn choose_none(&mut self) {
        for entry in self.entries.iter_mut().take(self.count).flatten() {
            entry.chosen = false;
        }
    }

    /// The chosen entries, in list order — what the session runs.
    pub fn chosen(&self) -> impl Iterator<Item = AutostartEntry> + '_ {
        self.entries
            .iter()
            .take(self.count)
            .flatten()
            .filter(|entry| entry.chosen)
            .copied()
    }
}

impl Default for Autostart {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the removable medium at the mount point carries an autorun
/// instruction, and which one wins: `.autorun`, then `autorun`, then
/// `autorun.sh` — upstream's search order.
pub fn autorun_file(names: &[&[u8]]) -> Option<&'static str> {
    const CANDIDATES: [&str; 3] = [".autorun", "autorun", "autorun.sh"];
    for candidate in CANDIDATES {
        let candidate_bytes = candidate.as_bytes();
        if names.contains(&candidate_bytes) {
            return Some(candidate);
        }
    }
    None
}

/// Copy `value` into `slot`, returning the copied length.
fn copy(slot: &mut [u8], value: &[u8]) -> usize {
    let take = value.len().min(slot.len());
    slot[..take].copy_from_slice(&value[..take]);
    take
}

fn slice_eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL: &[u8] = b"[Desktop Entry]\nType=Application\nName=Terminal\nExec=terminal\n";
    const HIDDEN: &[u8] =
        b"[Desktop Entry]\nType=Application\nName=Updater\nExec=updater\nHidden=true\n";
    const GNOME_ONLY: &[u8] =
        b"[Desktop Entry]\nType=Application\nName=GNOME Thing\nExec=gthing\nOnlyShowIn=GNOME;\n";

    #[test]
    fn valid_entries_are_kept() {
        let mut set = Autostart::new();
        assert!(set.offer(b"terminal.desktop", TERMINAL));
        assert_eq!(set.count(), 1);
        assert_eq!(set.entry(0).unwrap().exec(), "terminal");
        assert_eq!(set.entry(0).unwrap().label(), "Terminal");
    }

    #[test]
    fn hidden_entries_are_dropped() {
        let mut set = Autostart::new();
        assert!(!set.offer(b"updater.desktop", HIDDEN));
        assert_eq!(set.count(), 0);
    }

    #[test]
    fn other_desktops_entries_are_dropped() {
        let mut set = Autostart::new();
        assert!(!set.offer(b"gthing.desktop", GNOME_ONLY));
        assert_eq!(set.count(), 0);
    }

    #[test]
    fn later_files_with_the_same_basename_win() {
        let mut set = Autostart::new();
        assert!(set.offer(b"terminal.desktop", TERMINAL));
        let override_file =
            b"[Desktop Entry]\nType=Application\nName=User Terminal\nExec=my-terminal\n";
        assert!(set.offer(b"terminal.desktop", override_file));
        assert_eq!(set.count(), 1, "deduplicated, not duplicated");
        assert_eq!(set.entry(0).unwrap().exec(), "my-terminal");
        assert_eq!(set.entry(0).unwrap().label(), "User Terminal");
    }

    #[test]
    fn run_all_selects_everything() {
        let mut set = Autostart::new();
        set.offer(b"terminal.desktop", TERMINAL);
        set.offer(
            b"other.desktop",
            b"[Desktop Entry]\nType=Application\nName=Other\nExec=other\n",
        );
        set.choose_all();
        assert_eq!(set.chosen().count(), 2);
    }

    #[test]
    fn the_chooser_can_run_one_and_cancel() {
        let mut set = Autostart::new();
        set.offer(b"terminal.desktop", TERMINAL);
        set.offer(
            b"other.desktop",
            b"[Desktop Entry]\nType=Application\nName=Other\nExec=other\n",
        );
        assert!(set.choose(0));
        assert_eq!(set.chosen().count(), 1);
        set.choose_none();
        assert_eq!(set.chosen().count(), 0, "cancel runs nothing");
    }

    #[test]
    fn an_entry_without_exec_is_dropped() {
        let mut set = Autostart::new();
        assert!(!set.offer(
            b"empty.desktop",
            b"[Desktop Entry]\nType=Application\nName=E\n"
        ));
    }

    #[test]
    fn autorun_search_order_follows_upstream() {
        assert_eq!(autorun_file(&[b"autorun", b".autorun"]), Some(".autorun"));
        assert_eq!(autorun_file(&[b"autorun", b"autorun.sh"]), Some("autorun"));
        assert_eq!(autorun_file(&[b"autorun.sh"]), Some("autorun.sh"));
        assert_eq!(autorun_file(&[b"readme.txt"]), None);
    }
}
