//! Rust port of `ede-tip`, EDE's tooltip component.
//!
//! A tooltip shows a short description of the control under the pointer
//! after a short delay, and hides again when the pointer moves away. This
//! crate is the model — the delay accounting and the text buffer — so the
//! behaviour is testable without a window.

#![no_std]
#![forbid(unsafe_code)]

/// The maximum length of a tooltip's text.
pub const MAX_TEXT: usize = 96;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
