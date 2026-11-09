//! A QCheckBox — labelled box that toggles.

use crate::Colour;
use crate::event::{Event, EventKind, MouseButton, MouseState};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A checkbox with label trailing the box.
#[derive(Clone, Copy, Debug)]
pub struct QCheckBox {
    pub rect: Rect,
    pub text: &'static str,
    pub checked: bool,
    pub enabled: bool,
}

impl QCheckBox {
    pub const fn new(rect: Rect, text: &'static str) -> Self {
        Self {
            rect,
            text,
            checked: false,
            enabled: true,
        }
    }

    fn box_rect(&self) -> nova_ui::Rect {
        nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.h, self.rect.h)
    }
}

impl Widget for QCheckBox {
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
        let br = self.box_rect();
        canvas.fill_rect(br, Colour::PANEL.0);
        canvas.stroke_rect(br, Colour::BORDER.0);
        if self.checked {
            canvas.draw_text(br.x + 2, br.y, "✓", Colour::HIGHLIGHT.0, None);
        }
        canvas.draw_text(
            br.x + br.width as i32 + 6,
            br.y,
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
