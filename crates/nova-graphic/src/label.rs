//! A QLabel — simple text rendering with optional background fill.

use crate::Colour;
use crate::event::Event;
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A label: one line of static text.
#[derive(Clone, Copy, Debug)]
pub struct QLabel {
    pub rect: Rect,
    pub text: &'static str,
    pub fill: Colour,
}

impl QLabel {
    pub const fn new(rect: Rect, text: &'static str) -> Self {
        Self {
            rect,
            text,
            fill: Colour::PANEL,
        }
    }
}

impl Widget for QLabel {
    fn rect(&self) -> Rect {
        self.rect
    }
    fn set_rect(&mut self, r: Rect) {
        self.rect = r;
    }
    fn draw(&self, canvas: &mut Canvas) {
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            self.fill.0,
        );
        canvas.draw_text(
            self.rect.x + 2,
            self.rect.y + 2,
            self.text,
            Colour::TEXT.0,
            Some(self.fill.0),
        );
    }
    fn on_event(&mut self, _: &Event) -> bool {
        false
    }
}
