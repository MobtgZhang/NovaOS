//! A QRadioButton — mutually exclusive circular control.

use crate::Colour;
use crate::event::{Event, EventKind, MouseButton, MouseState};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A radio button. `checked` toggles on click; exclusivity is enforced by the container.
#[derive(Clone, Copy, Debug)]
pub struct QRadioButton {
    pub rect: Rect,
    pub text: &'static str,
    pub checked: bool,
    pub enabled: bool,
}

impl QRadioButton {
    pub const fn new(rect: Rect, text: &'static str) -> Self {
        Self {
            rect,
            text,
            checked: false,
            enabled: true,
        }
    }
}

impl Widget for QRadioButton {
    fn rect(&self) -> Rect {
        self.rect
    }
    fn set_rect(&mut self, r: Rect) {
        self.rect = r;
    }
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn draw(&self, canvas: &mut Canvas) {
        let cr = Rect::new(self.rect.x, self.rect.y, self.rect.h, self.rect.h);
        // Draw the circle as a filled square approximated by a 10x10 block.
        canvas.fill_rect(
            nova_ui::Rect::new(cr.x + 1, cr.y + 1, cr.w - 2, cr.h - 2),
            Colour::PANEL.0,
        );
        canvas.stroke_rect(
            nova_ui::Rect::new(cr.x + 1, cr.y + 1, cr.w - 2, cr.h - 2),
            Colour::BORDER.0,
        );
        if self.checked {
            canvas.fill_rect(
                nova_ui::Rect::new(cr.x + 4, cr.y + 4, cr.w - 8, cr.h - 8),
                Colour::HIGHLIGHT.0,
            );
        }
        canvas.draw_text(
            cr.x + cr.w as i32 + 6,
            cr.y,
            self.text,
            Colour::TEXT.0,
            None,
        );
    }

    fn on_event(&mut self, e: &Event) -> bool {
        if let EventKind::MouseButton {
            x,
            y,
            button: MouseButton::Left,
            state: MouseState::Released,
        } = e.kind
            && self.rect.contains(x, y)
        {
            self.checked = !self.checked;
            return true;
        }
        false
    }
}
