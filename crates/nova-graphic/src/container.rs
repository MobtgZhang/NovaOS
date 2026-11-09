//! Layout containers — Qt6's `QVBoxLayout` analog.

use crate::widget::{Rect, Widget};

/// A simple vertical box layout: stacks widgets top-to-bottom with fixed spacing.
/// The implementation is explicit and non-allocating, as the rest of the crate.
pub struct VBox {
    pub rect: Rect,
    pub spacing: i32,
    pub children: [Option<&'static mut dyn Widget>; 8],
}

impl VBox {
    pub const fn new(rect: Rect, spacing: i32) -> Self {
        Self {
            rect,
            spacing,
            children: [None, None, None, None, None, None, None, None],
        }
    }
}
