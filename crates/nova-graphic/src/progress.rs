//! A QProgressBar — flat bar that fills from 0..=100.
//! Qt6 has the same notion: total/progress mapped onto a [0..=100] ratio.

use crate::Colour;
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A progress bar. Reads `value`, renders `value/max` fill.
#[derive(Clone, Copy, Debug)]
pub struct QProgressBar {
    pub rect: Rect,
    pub value: u32,
    pub maximum: u32,
    pub enabled: bool,
}

impl QProgressBar {
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            value: 0,
            maximum: 100,
            enabled: true,
        }
    }
}

impl Widget for QProgressBar {
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
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::PANEL.0,
        );
        let fill_w = self
            .rect
            .w
            .checked_mul(self.value)
            .and_then(|v| v.checked_div(self.maximum))
            .unwrap_or(0);
        if fill_w > 0 {
            canvas.fill_rect(
                nova_ui::Rect::new(self.rect.x, self.rect.y, fill_w, self.rect.h),
                Colour::HIGHLIGHT.0,
            );
        }
        canvas.stroke_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::BORDER.0,
        );
    }
    fn on_event(&mut self, _: &crate::event::Event) -> bool {
        false
    }
}
