//! A QPushButton — clickable rect with label and pressed state.

use crate::Colour;
use crate::event::{Event, EventKind, MouseButton, MouseState};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A push button: text inside a coloured frame.
#[derive(Clone, Copy, Debug)]
pub struct QPushButton {
    pub rect: Rect,
    pub text: &'static str,
    pub pressed: bool,
    pub enabled: bool,
    pub focused: bool,
    pub flat: bool,
}

impl QPushButton {
    pub const fn new(rect: Rect, text: &'static str) -> Self {
        Self {
            rect,
            text,
            pressed: false,
            enabled: true,
            focused: false,
            flat: false,
        }
    }
}

impl Widget for QPushButton {
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
        let face = if self.pressed {
            Colour::HIGHLIGHT_DARK
        } else {
            Colour::PANEL_LIGHT
        };
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            face.0,
        );
        canvas.stroke_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::BORDER.0,
        );
        let y = self.rect.y + 4;
        canvas.draw_text(self.rect.x + 8, y, self.text, Colour::TEXT.0, Some(face.0));
    }

    fn on_event(&mut self, e: &Event) -> bool {
        match e.kind {
            EventKind::MouseButton {
                x,
                y,
                button: MouseButton::Left,
                state: MouseState::Pressed,
            } => {
                if self.rect.contains(x, y) {
                    self.pressed = true;
                    return true;
                }
            }
            EventKind::MouseButton {
                x: _,
                y: _,
                button: MouseButton::Left,
                state: MouseState::Released,
            } => {
                let was = self.pressed;
                self.pressed = false;
                return was;
            }
            EventKind::FocusIn => {
                self.focused = true;
                return true;
            }
            EventKind::FocusOut => {
                self.focused = false;
                return true;
            }
            _ => {}
        }
        false
    }
}
