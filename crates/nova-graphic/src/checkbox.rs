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
            // A checkmark cannot rely on the font: the embedded GUI faces
            // carry ASCII and GB2312, not U+2713. Plotted as 1-px cells
            // along the two diagonals of the box.
            const PATTERN: [(u32, u32); 8] = [
                (2, 5),
                (3, 6),
                (4, 7),
                (5, 6),
                (6, 5),
                (7, 4),
                (8, 3),
                (9, 2),
            ];
            let ox = br.x as u32;
            let oy = br.y as u32;
            for &(dx, dy) in &PATTERN {
                let px = ox + dx;
                let py = oy + dy;
                if px < ox + br.width && py < oy + br.height {
                    canvas.fill_rect(
                        nova_ui::Rect::new(px as i32, py as i32, 1, 1),
                        Colour::HIGHLIGHT.0,
                    );
                }
            }
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
