//! A QLineEdit — single-line editable text field.

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EchoMode {
    Normal,
    Password,
}

impl QLineEdit {
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            focused: false,
            enabled: true,
            echo_mode: EchoMode::Normal,
        }
    }
}

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
            Colour::HIGHLIGHT
        } else {
            Colour::BORDER
        };
        canvas.stroke_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            border.0,
        );
        // Cursor drawn when focused: a thin vertical bar after the text start.
        if self.focused {
            canvas.fill_rect(
                nova_ui::Rect::new(self.rect.x + 8, self.rect.y + 4, 2, self.rect.h - 8),
                Colour::TEXT.0,
            );
        }
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
            EventKind::KeyChar { .. } => self.focused,
            EventKind::KeyBackspace => self.focused,
            EventKind::KeyEscape => {
                if self.focused {
                    self.focused = false;
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}
