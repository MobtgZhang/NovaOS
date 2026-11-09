//! A QListWidget — a scrollable list of items with selection.

use crate::Colour;
use crate::event::{Event, EventKind, MouseButton, MouseState};
use crate::widget::{Rect, Widget};
use nova_ui::Canvas;

/// A list of items. Selection is one index at a time.
#[derive(Clone, Copy, Debug)]
pub struct QListWidget {
    pub rect: Rect,
    pub items: [&'static str; 8],
    pub item_count: usize,
    pub selected: Option<usize>,
    pub focused: bool,
    pub item_h: u32,
}

impl QListWidget {
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            items: [""; 8],
            item_count: 0,
            selected: None,
            focused: false,
            item_h: 16,
        }
    }
}

impl Widget for QListWidget {
    fn rect(&self) -> Rect {
        self.rect
    }
    fn set_rect(&mut self, r: Rect) {
        self.rect = r;
    }
    fn focused(&self) -> bool {
        self.focused
    }
    fn draw(&self, canvas: &mut Canvas) {
        canvas.fill_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::PANEL.0,
        );
        canvas.stroke_rect(
            nova_ui::Rect::new(self.rect.x, self.rect.y, self.rect.w, self.rect.h),
            Colour::BORDER.0,
        );
        for i in 0..self.item_count {
            let row = self.rect.y + (i as i32) * self.item_h as i32;
            if Some(i) == self.selected {
                canvas.fill_rect(
                    nova_ui::Rect::new(self.rect.x + 1, row, self.rect.w - 2, self.item_h),
                    Colour::HIGHLIGHT.0,
                );
            }
            canvas.draw_text(
                self.rect.x + 4,
                row + 1,
                self.items[i],
                Colour::TEXT.0,
                None,
            );
        }
    }
    fn on_event(&mut self, e: &Event) -> bool {
        match e.kind {
            EventKind::FocusIn => {
                self.focused = true;
                return true;
            }
            EventKind::FocusOut => {
                self.focused = false;
                return true;
            }
            EventKind::MouseButton {
                x,
                y,
                button: MouseButton::Left,
                state: MouseState::Released,
            } if self.rect.contains(x, y) => {
                let rel = (y - self.rect.y).max(0) as u32;
                let i = (rel / self.item_h) as usize;
                if i < self.item_count {
                    self.selected = Some(i);
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}
