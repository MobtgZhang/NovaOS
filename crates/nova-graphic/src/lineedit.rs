//! A QLineEdit — single-line editable text field.
//!
//! # Why the text lives outside the widget
//!
//! A widget here is a value: fixed size, no allocator, no globals. A text
//! field that *owned* its string would need a heap it does not have. The
//! caller keeps the buffer — typically a stack array — and hands it to
//! [`QLineEdit::draw_with_text`] on every paint. The widget stores only
//! chrome state: focus, echo mode, and enabled.
//!
//! # What the widget deliberately does not do
//!
//! Cursor movement. The buffer is edited by the application, which decides
//! where the caret goes; this crate answers "is the frame drawn and where do
//! the glyphs land", it does not arbitrate editing policy.

use crate::Colour;
use crate::event::{Event, EventKind};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A single-line text input. The buffer is caller-owned.
#[derive(Clone, Copy, Debug)]
pub struct QLineEdit {
    pub rect: Rect,
    pub focused: bool,
    pub enabled: bool,
    pub echo_mode: EchoMode,
}

/// Whether typed characters print as-is or as bullet placeholders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EchoMode {
    /// Characters are drawn verbatim.
    Normal,
    /// Characters are drawn as `•` bullets; use for passwords.
    Password,
}

/// The maximum number of visible characters — the caller may have more, but
/// only this prefix is drawn.
pub const MAX_VISIBLE_CHARS: usize = 64;

impl QLineEdit {
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            focused: false,
            enabled: true,
            echo_mode: EchoMode::Normal,
        }
    }

    /// Draw the frame, then draw `buffer` inside it as text.
    ///
    /// When `echo_mode` is [`EchoMode::Password`], each visible character is
    /// replaced with one bullet byte so a glance at the screen leaks nothing
    /// beyond the length.
    pub fn draw_with_text(&self, canvas: &mut Canvas, buffer: &[u8]) {
        self.draw(canvas);

        let inset = 8;
        let x0 = self.rect.x + inset;
        let text_y = self.rect.y + 3;
        let baseline = Colour::TEXT.0;
        let face_background = Some(Colour::PANEL.0);

        match self.echo_mode {
            EchoMode::Normal => {
                let text = core::str::from_utf8(buffer).unwrap_or("?");
                canvas.draw_text(x0, text_y, text, baseline, face_background);
            }
            EchoMode::Password => {
                let count = buffer.len().min(MAX_VISIBLE_CHARS);
                let masked = &BULLETS[..count];
                let masked = core::str::from_utf8(masked).unwrap_or("");
                canvas.draw_text(x0, text_y, masked, baseline, face_background);
            }
        }

        if self.focused {
            // Caret after the visible text.
            let visible = visible_prefix(buffer, MAX_VISIBLE_CHARS);
            let width = Canvas::text_width(visible);
            let caret_x = x0 + width as i32;
            canvas.fill_rect(
                nova_ui::Rect::new(caret_x, self.rect.y + 3, 2, self.rect.h.saturating_sub(6)),
                Colour::TEXT.0,
            );
        }
    }
}

/// Return the longest prefix of `buffer` that is valid UTF-8, at most `max`
/// bytes, and never splits a code point.
fn visible_prefix(buffer: &[u8], max: usize) -> &str {
    let take = buffer.len().min(max);
    match core::str::from_utf8(&buffer[..take]) {
        Ok(text) => text,
        // Back off to a char boundary.
        Err(error) => {
            let valid = error.valid_up_to();
            core::str::from_utf8(&buffer[..valid]).unwrap_or("")
        }
    }
}

/// One `'*'` per visible password character. Every embedded face renders `*`,
/// so the mask never depends on a bullet glyph being present.
const BULLETS: &[u8] = b"****************************************************************";

impl Widget for QLineEdit {
    fn rect(&self) -> Rect {
        self.rect
    }
    fn set_rect(&mut self, r: Rect) {
        self.rect = r;
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn focused(&self) -> bool {
        self.focused
    }

    fn draw(&self, canvas: &mut Canvas) {
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::PANEL.0,
        );
        let border = if self.focused {
            crate::Colour::HIGHLIGHT
        } else {
            crate::Colour::BORDER
        };
        canvas.stroke_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            border.0,
        );
    }

    fn on_event(&mut self, e: &Event) -> bool {
        match e.kind {
            EventKind::FocusIn => {
                self.focused = true;
                true
            }
            EventKind::FocusOut => {
                self.focused = false;
                true
            }
            EventKind::KeyChar { .. } | EventKind::KeyBackspace => self.focused,
            EventKind::KeyEscape => {
                self.focused = false;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_prefix_keeps_code_points_whole() {
        // 0xE4 0xB8 0xAD is '中': truncating after the second byte must back off.
        let bytes = [0xE4, 0xB8, 0xAD, b'a'];
        assert_eq!(visible_prefix(&bytes, 2), "");
        assert_eq!(visible_prefix(&bytes, 4), "中a");
    }
}
