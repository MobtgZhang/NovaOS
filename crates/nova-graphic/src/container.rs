//! Layout containers — Qt6's `QVBoxLayout` analog.
//!
//! # Why a value, not a widget
//!
//! A layout is arithmetic: given a parent rectangle and a list of child
//! heights, it decides where each child sits. It owns nothing and paints
//! nothing; the caller keeps the widgets and pushes each computed rectangle
//! into `Widget::set_rect`. Encoding it as pure geometry makes the whole
//! thing host-testable and keeps `Widget` object-safe — an allocating child
//! list would smuggle in a dependency the rest of the crate does not have.
//!
//! # What a VBox is not
//!
//! A *size negotiation* protocol. There is no stretch factor, no
//! minimum/maximum hint, no alignment policy: children get the full width
//! of the parent and their requested height, stacked from the top with a
//! uniform gap. Those three rules are the entire contract.

use crate::widget::Rect;

/// A vertical box layout: fixed-width children stacked top-to-bottom with a
/// uniform gap between them.
///
/// ```
/// use nova_graphic::{VBox, Rect};
/// let layout = VBox::new(Rect::new(10, 20, 200, 0), 8);
/// let mut out = [Rect::new(0, 0, 0, 0); 3];
/// layout.layout(&[24, 24, 32], &mut out);
/// assert_eq!(out[0].y, 20);
/// assert_eq!(out[1].y, 20 + 24 + 8);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VBox {
    /// The parent rectangle the children are laid out inside. Its height is
    /// ignored — the stack's height is the sum of children plus gaps.
    pub rect: Rect,
    /// The vertical gap inserted between neighbouring children, in pixels.
    pub spacing: u32,
}

impl VBox {
    /// A layout packing `rect`'s width, stacked from `rect.y`, with `spacing`
    /// pixels between children.
    pub const fn new(rect: Rect, spacing: u32) -> Self {
        Self { rect, spacing }
    }

    /// Compute the rectangle for the child at `index`, whose height is
    /// `heights[index]`. Children are full-width of the parent rectangle.
    pub fn child_rect(&self, index: usize, heights: &[u32]) -> Rect {
        let mut y = self.rect.y;
        for (i, &height) in heights.iter().enumerate() {
            if i == index {
                return Rect::new(self.rect.x, y, self.rect.w, height);
            }
            y = y.saturating_add(height as i32 + self.spacing as i32);
        }
        // Past the end: an empty rect at the stack's tail.
        Rect::new(self.rect.x, y, self.rect.w, 0)
    }

    /// Lay out every child in `heights`, writing the computed rectangles to
    /// `out`. Returns the number of rectangles written, which is
    /// `min(heights.len(), out.len())`.
    pub fn layout(&self, heights: &[u32], out: &mut [Rect]) -> usize {
        let count = heights.len().min(out.len());
        let mut y = self.rect.y;
        for index in 0..count {
            out[index] = Rect::new(self.rect.x, y, self.rect.w, heights[index]);
            y = y.saturating_add(heights[index] as i32 + self.spacing as i32);
        }
        count
    }

    /// The total height the stack occupies given `heights`.
    pub fn natural_height(&self, heights: &[u32]) -> u32 {
        if heights.is_empty() {
            return 0;
        }
        let sum: u32 = heights.iter().sum();
        sum + self.spacing * (heights.len() as u32 - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_children_with_uniform_gap() {
        let layout = VBox::new(Rect::new(10, 20, 200, 0), 8);
        let mut out = [Rect::new(0, 0, 0, 0); 3];
        layout.layout(&[24, 24, 32], &mut out);
        assert_eq!(out[0], Rect::new(10, 20, 200, 24));
        assert_eq!(out[1], Rect::new(10, 20 + 32, 200, 24));
        assert_eq!(out[2], Rect::new(10, 20 + 64, 200, 32));
    }

    #[test]
    fn child_rect_matches_layout() {
        let layout = VBox::new(Rect::new(0, 100, 90, 0), 4);
        let heights = [10, 20, 30, 40];
        let mut out = [Rect::new(0, 0, 0, 0); 4];
        layout.layout(&heights, &mut out);
        for (index, rect) in out.iter().enumerate() {
            assert_eq!(layout.child_rect(index, &heights), *rect);
        }
    }

    #[test]
    fn natural_height_sums_children_and_gaps() {
        let layout = VBox::new(Rect::new(0, 0, 100, 0), 5);
        assert_eq!(layout.natural_height(&[]), 0);
        assert_eq!(layout.natural_height(&[10]), 10);
        assert_eq!(layout.natural_height(&[10, 20, 30]), 70);
    }

    #[test]
    fn layout_stops_at_the_shorter_slice() {
        let layout = VBox::new(Rect::new(0, 0, 100, 0), 2);
        let mut out = [Rect::new(0, 0, 0, 0); 2];
        assert_eq!(layout.layout(&[10, 20, 30, 40], &mut out), 2);
        assert_eq!(layout.layout(&[10], &mut out), 1);
    }

    #[test]
    fn child_rect_past_the_end_is_empty_at_the_tail() {
        let layout = VBox::new(Rect::new(5, 5, 50, 0), 2);
        let rect = layout.child_rect(3, &[10, 10]);
        assert_eq!(rect.h, 0);
        assert_eq!(rect.y, 5 + 10 + 2 + 10 + 2);
        assert_eq!(rect.x, 5);
    }
}
