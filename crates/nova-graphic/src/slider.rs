//! A QSlider — horizontal track with draggable handle.

use crate::Colour;
use crate::event::{Event, EventKind, MouseButton, MouseState};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A horizontal slider: handle position is stored as `value` (0..=100).
#[derive(Clone, Copy, Debug)]
pub struct QSlider {
    pub rect: Rect,
    pub value: u32,
    pub enabled: bool,
}

impl QSlider {
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            value: 50,
            enabled: true,
        }
    }
}

impl Widget for QSlider {
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
        let cy = self.rect.y + self.rect.h as i32 / 2;
        let handle_x = self.rect.x + (self.rect.w as i32 * self.value as i32 / 100);
        // Track
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, cy - 2, self.rect.w, 4),
            Colour::BORDER.0,
        );
        // Handle (circle approximated by 12x12 square)
        canvas.fill_rect(
            nova_ui::Rect::new(handle_x - 6, cy - 6, 12, 12),
            Colour::HIGHLIGHT.0,
        );
        canvas.stroke_rect(
            nova_ui::Rect::new(handle_x - 6, cy - 6, 12, 12),
            Colour::HIGHLIGHT_DARK.0,
        );
    }

    fn on_event(&mut self, e: &Event) -> bool {
        if let EventKind::MouseButton {
            x,
            y,
            button: MouseButton::Left,
            state: MouseState::Pressed,
        } = e.kind
            && self.rect.contains(x, y)
        {
            let rel = (x - self.rect.x).max(0) as u32;
            self.value = rel.saturating_mul(100).min(self.rect.w) / self.rect.w.max(1);
            return true;
        }
        false
    }
}
