//! The panel: a dark bar across the bottom of the screen.
//!
//! # What it shows
//!
//! - Left: the NovaOS wordmark and the version — the system identifying
//!   itself, like any self-respecting status bar.
//! - Centre: the four applications, as launch chips. Clicking a chip spawns
//!   another instance of that application — an honest taskbar in a system
//!   without window lists coming over IPC yet.
//! - Right: the desktop's own address, `novaos://desktop`.
//!
//! # What it is not
//!
//! A clock. NovaOS has no user-reachable wall-clock syscall yet, and a panel
//! that shows a made-up time is a panel that lies. When the time syscall
//! lands, the clock belongs exactly here.

use nova_ui::Canvas;

use crate::{PANEL_BG, PANEL_LINE};

/// Height of the panel, in pixels.
pub const PANEL_HEIGHT: u32 = 32;

/// The applications the panel launches, in chip order.
///
/// Order matches the desktop icons, so a user who learns one can read the
/// other. The strings are exactly what `process::exec` takes.
pub const APP: &[&str] = &["terminal", "file-manager", "editor", "widgets-demo"];

/// The chip labels, in the same order as [`APP`].
pub const CHIP_LABELS: &[&str] = &["Terminal", "Files", "Editor", "Widgets"];

/// The x offset of chip `index`'s left edge.
fn chip_x(index: usize) -> i32 {
    // Chips are 96 px wide with a 12 px gap, starting after the wordmark.
    210 + index as i32 * 108
}

/// The chip's hit-test bounds.
pub fn chip_bounds(index: usize) -> (i32, i32, i32, i32) {
    (chip_x(index), 4, chip_x(index) + 96, 28)
}

/// Which chip is under the point, or `None`.
pub fn hit(x: i32, y: i32) -> Option<u8> {
    // The panel occupies only its own strip.
    // (The caller knows the panel's y from the canvas height; keeping this
    // in panel-local coordinates makes the geometry testable.)
    if y < 0 {
        return None;
    }
    for index in 0..CHIP_LABELS.len() {
        let (x0, y0, x1, y1) = chip_bounds(index);
        if x >= x0 && x < x1 && y >= y0 && y < y1 {
            return Some(index as u8);
        }
    }
    None
}

/// Draw the panel onto the bottom strip of the canvas.
pub fn draw(canvas: &mut Canvas, width: u32, height: u32, pressed: Option<u8>) {
    let top = height.saturating_sub(PANEL_HEIGHT);
    canvas.fill_rect(
        nova_ui::Rect::new(0, top as i32, width, PANEL_HEIGHT),
        PANEL_BG,
    );
    canvas.fill_rect(nova_ui::Rect::new(0, top as i32, width, 1), PANEL_LINE);

    // The wordmark: an accent dot and the name with version.
    canvas.fill_rect(
        nova_ui::Rect::new(16, (top + 8) as i32, 16, 16),
        crate::ACCENT,
    );
    canvas.draw_text(40, (top + 8) as i32, "NovaOS", 0x00E6ECF2, Some(PANEL_BG));

    // The chips.
    for (index, label) in CHIP_LABELS.iter().enumerate() {
        let (x0, _y0, _x1, _y1) = chip_bounds(index);
        let is_pressed = pressed == Some(index as u8);
        let fill = if is_pressed {
            crate::ACCENT
        } else {
            0x001A2430
        };
        canvas.fill_rect(nova_ui::Rect::new(x0, (top + 4) as i32, 96, 24), fill);
        canvas.stroke_rect(nova_ui::Rect::new(x0, (top + 4) as i32, 96, 24), PANEL_LINE);
        let text_colour = if is_pressed { 0x00101A26 } else { 0x00C8D4DE };
        canvas.draw_text(x0 + 10, (top + 8) as i32, label, text_colour, Some(fill));
    }

    // The address, right edge.
    let handle = "novaos://desktop";
    let width_px = nova_ui::Canvas::text_width(handle) as i32;
    canvas.draw_text(
        width as i32 - width_px - 16,
        (top + 8) as i32,
        handle,
        0x008A93A4,
        Some(PANEL_BG),
    );
}
