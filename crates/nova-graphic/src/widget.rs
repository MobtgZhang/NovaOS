//! The common widget interface shared by every Qt6-flavoured widget in this crate.

use crate::event::Event;
use nova_ui::Canvas;

/// A pixel rectangle every widget owns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w as i32 && y < self.y + self.h as i32
    }
}

/// Everything a widget must know and do to participate in layout/paint/event.
pub trait Widget {
    /// The bounding rectangle (relative to parent / scene coordinates).
    fn rect(&self) -> Rect;

    /// Set the bounding rectangle.
    fn set_rect(&mut self, r: Rect);

    /// Whether id widget is enabled and visible.
    fn enabled(&self) -> bool {
        true
    }

    /// Whether id widget is currently focused (keyboard events go to it).
    fn focused(&self) -> bool {
        false
    }

    /// Draw the widget onto `canvas`.
    fn draw(&self, canvas: &mut Canvas);

    /// Receive one input event; returns `true` when the event was consumed.
    fn on_event(&mut self, event: &Event) -> bool;

    /// Common one-line text the widget exposes for debugging.
    fn label(&self) -> &str {
        "?"
    }
}
