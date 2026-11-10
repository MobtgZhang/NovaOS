//! Rust port of `ede-tip` — EDE's tip-of-the-day dialog, and the tooltip
//! model the desktop shell shares.
//!
//! Upstream `ede-tip` is two things: a fortune-format reader that shows
//! one tip at a time with Previous/Next browsing, and the "Show tips on
//! startup" switch that writes `~/.config/autostart/ede-tip.desktop`.
//! Both live here as models; the dialog itself is drawn by the shell.
//!
//! The `Tooltip` half is the hover delay model the desktop's icons and
//! panel use — the other half of upstream's tooltip machinery.

#![no_std]
#![forbid(unsafe_code)]

/// The maximum length of a tooltip's text.
pub const MAX_TEXT: usize = 96;
/// The maximum number of tips one book holds.
pub const MAX_TIPS: usize = 32;
/// The longest single tip the book keeps.
pub const MAX_TIP: usize = 192;
/// The longest basename the autostart model writes.
pub const AUTOSTART_NAME_MAX: usize = 48;

/// One tooltip: its text, and whether it is currently shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tooltip {
    text: [u8; MAX_TEXT],
    text_len: usize,
    /// Whether the tooltip is visible.
    pub visible: bool,
    /// How many ticks the pointer has been stationary on the control.
    pub hover_ticks: u32,
    /// The delay, in ticks, before the tooltip appears.
    pub delay: u32,
}

impl Tooltip {
    /// A tooltip with the given delay (in ticks).
    pub const fn new(delay: u32) -> Self {
        Self {
            text: [0; MAX_TEXT],
            text_len: 0,
            visible: false,
            hover_ticks: 0,
            delay,
        }
    }

    /// Set the tooltip text; long strings are clipped.
    pub fn set_text(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let take = bytes.len().min(MAX_TEXT);
        self.text[..take].copy_from_slice(&bytes[..take]);
        self.text_len = take;
    }

    /// The tooltip's text.
    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.text[..self.text_len]).unwrap_or("")
    }

    /// Report the pointer entered the control.
    pub fn enter(&mut self) {
        self.hover_ticks = 0;
        self.visible = false;
    }

    /// Report the pointer left the control.
    pub fn leave(&mut self) {
        self.hover_ticks = 0;
        self.visible = false;
    }

    /// Advance one tick; shows the tooltip once the delay elapses.
    pub fn tick(&mut self) {
        if self.text_len == 0 {
            return;
        }
        self.hover_ticks = self.hover_ticks.saturating_add(1);
        if self.hover_ticks >= self.delay {
            self.visible = true;
        }
    }
}

impl Default for Tooltip {
    fn default() -> Self {
        Self::new(20)
    }
}

/// The tip-of-the-day book: a fortune-format collection, a browsing
/// position, and the startup switch.
///
/// Fortune text is a series of tips separated by a line holding a single
/// `%` — exactly what upstream's `tips/ede` file uses, so the shell can
/// hand this crate the file's bytes and get browsing for free.
#[derive(Clone, Copy)]
pub struct TipsBook {
    tips: [[u8; MAX_TIP]; MAX_TIPS],
    tip_lens: [usize; MAX_TIPS],
    count: usize,
    /// The index of the tip currently shown.
    position: usize,
    /// Whether "Show tips on startup" is checked.
    pub show_on_startup: bool,
    /// Deterministic LCG state: `seed(0)` always shows the same first tip,
    /// which keeps tests honest and behaviour reproducible.
    random_state: u32,
}

impl TipsBook {
    /// An empty book.
    pub const fn new() -> Self {
        Self {
            tips: [[0; MAX_TIP]; MAX_TIPS],
            tip_lens: [0; MAX_TIPS],
            count: 0,
            position: 0,
            show_on_startup: true,
            random_state: 1,
        }
    }

    /// Parse a fortune-format collection: tips separated by a line that
    /// is a single `%`. Long tips are clipped, never split.
    pub fn parse(&mut self, bytes: &[u8]) {
        self.count = 0;
        self.position = 0;
        for chunk in split_on_separator(bytes) {
            let trimmed = trim(chunk);
            if trimmed.is_empty() {
                continue;
            }
            if self.count >= MAX_TIPS {
                break;
            }
            let take = trimmed.len().min(MAX_TIP);
            self.tips[self.count][..take].copy_from_slice(&trimmed[..take]);
            self.tip_lens[self.count] = take;
            self.count += 1;
        }
        // A single-`%` separator at the very end leaves an empty trailing
        // chunk; the loop above already ignores it.
    }

    /// How many tips the book holds.
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The tip at `index`, when present.
    pub fn tip(&self, index: usize) -> Option<&str> {
        if index >= self.count {
            return None;
        }
        core::str::from_utf8(&self.tips[index][..self.tip_lens[index]]).ok()
    }

    /// The tip currently shown.
    pub fn current(&self) -> Option<&str> {
        self.tip(self.position)
    }

    /// Move to the next tip, wrapping at the end.
    pub fn next(&mut self) {
        if self.count == 0 {
            return;
        }
        self.position = (self.position + 1) % self.count;
    }

    /// Move to the previous tip, wrapping at the start.
    pub fn previous(&mut self) {
        if self.count == 0 {
            return;
        }
        self.position = if self.position == 0 {
            self.count - 1
        } else {
            self.position - 1
        };
    }

    /// Jump to a deterministic "random" tip. Upstream picked with
    /// `rand() % count`, which never reaches the last tip; this LCG
    /// covers the whole range, including the last.
    pub fn random(&mut self) {
        if self.count == 0 {
            return;
        }
        self.random_state = self
            .random_state
            .wrapping_mul(1_103_515_245)
            .wrapping_add(12_345);
        self.position = (self.random_state >> 16) as usize % self.count;
    }

    /// The index currently shown.
    pub const fn position(&self) -> usize {
        self.position
    }

    /// The bytes of the autostart entry this book's switch writes —
    /// `Hidden=true` when the user turned tips off, absent otherwise.
    /// The caller writes this to `~/.config/autostart/ede-tip.desktop`.
    pub fn autostart_desktop(&self, out: &mut [u8]) -> usize {
        let header = if self.show_on_startup {
            &b"[Desktop Entry]\nType=Application\nName=EDE Tips\nExec=ede-tip\n"[..]
        } else {
            &b"[Desktop Entry]\nType=Application\nName=EDE Tips\nExec=ede-tip\nHidden=true\n"[..]
        };
        let take = header.len().min(out.len());
        out[..take].copy_from_slice(&header[..take]);
        take
    }
}

impl Default for TipsBook {
    fn default() -> Self {
        Self::new()
    }
}

/// Split fortune text on the `%`-line separator, yielding tip chunks.
/// `core` has no multi-byte split, so the scan is by hand.
fn split_on_separator(bytes: &[u8]) -> ChunkIter<'_> {
    ChunkIter { bytes, pos: 0 }
}

/// The iterator behind [`split_on_separator`].
struct ChunkIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.bytes.len() {
            return None;
        }
        let mut index = self.pos;
        while index + 1 < self.bytes.len() {
            if self.bytes[index] == b'%' && self.bytes[index + 1] == b'\n' {
                let chunk = &self.bytes[self.pos..index];
                self.pos = index + 2;
                return Some(chunk);
            }
            index += 1;
        }
        // A trailing `%` with no newline still terminates the last tip.
        if self.bytes[self.bytes.len() - 1] == b'%' {
            let chunk = &self.bytes[self.pos..self.bytes.len() - 1];
            self.pos = self.bytes.len();
            return Some(chunk);
        }
        let chunk = &self.bytes[self.pos..];
        self.pos = self.bytes.len();
        Some(chunk)
    }
}

/// `trim` for a byte slice.
fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &[u8] = b"Welcome to NovaOS EDE.\n%\n\
Did you know? The comet is drawn, not an image.\n%\n\
Press the Nova button for applications.\n%\n";

    #[test]
    fn shows_after_the_delay_and_hides_on_leave() {
        let mut tip = Tooltip::new(3);
        tip.set_text("hello");
        tip.enter();
        assert!(!tip.visible);
        tip.tick();
        tip.tick();
        assert!(!tip.visible);
        tip.tick();
        assert!(tip.visible);
        tip.leave();
        assert!(!tip.visible);
    }

    #[test]
    fn empty_text_never_shows() {
        let mut tip = Tooltip::new(0);
        tip.enter();
        tip.tick();
        assert!(!tip.visible);
    }

    #[test]
    fn parses_fortune_format() {
        let mut book = TipsBook::new();
        book.parse(BOOK);
        assert_eq!(book.count(), 3);
        assert_eq!(book.current(), Some("Welcome to NovaOS EDE."));
    }

    #[test]
    fn browsing_wraps_both_ways() {
        let mut book = TipsBook::new();
        book.parse(BOOK);
        book.next();
        assert_eq!(
            book.current(),
            Some("Did you know? The comet is drawn, not an image.")
        );
        book.next();
        book.next();
        assert_eq!(book.current(), Some("Welcome to NovaOS EDE."), "next wraps");
        book.previous();
        assert_eq!(book.position(), 2, "previous wraps from 0 to the last");
    }

    #[test]
    fn random_covers_the_whole_range() {
        let mut book = TipsBook::new();
        book.parse(BOOK);
        let mut reached = [false; 3];
        for _ in 0..64 {
            book.random();
            reached[book.position()] = true;
        }
        assert!(reached.iter().all(|seen| *seen), "every tip is reachable");
    }

    #[test]
    fn an_empty_book_browses_safely() {
        let mut book = TipsBook::new();
        book.next();
        book.previous();
        book.random();
        assert_eq!(book.current(), None);
    }

    #[test]
    fn autostart_flag_controls_the_hidden_key() {
        let mut book = TipsBook::new();
        let mut buffer = [0u8; 128];
        let length = book.autostart_desktop(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap();
        assert!(!text.contains("Hidden"), "default is on");

        book.show_on_startup = false;
        let length = book.autostart_desktop(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap();
        assert!(text.contains("Hidden=true"));
        assert!(text.contains("Exec=ede-tip"));
    }
}
