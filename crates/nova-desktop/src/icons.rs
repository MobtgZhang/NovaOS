//! The desktop icon set: Terminal, Files, Editor, Widgets.
//!
//! Each icon is a tiny picture drawn with solid primitives, plus a label
//! under it. Nothing is a bitmap: every glyph a desktop needs is small
//! enough to compose from rectangles and arcs, and an asset pipeline for
//! four icons would be a cost with no payoff.
//!
//! # Layout
//!
//! Icons stack in one column down the left edge of the wallpaper, with the
//! first icon twelve pixels from the top. Each cell is 96 × 96; the glyph
//! occupies the top 48 and the label the rest.
//!
//! # Hit testing
//!
//! `hit` answers with the icon under a point, or `None`. The shell fires an
//! icon on release inside the same cell it pressed — the standard contract —
//! so a mis-drag that leaves the cell activates nothing.

use nova_ui::Canvas;

/// One desktop icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Icon {
    /// The label drawn under the glyph.
    pub label: &'static str,
    /// The accent colour the glyph body is drawn with.
    pub body: u32,
    /// The brighter accent used for details on the body.
    pub detail: u32,
}

/// The applications the desktop launches, in icon order.
///
/// The spelling matches the service name the init process knows how to
/// spawn, so `DesktopCommand::Launch(APP[i])` is exactly the string
/// `process::exec` takes.
pub const APP: &[&str] = &["terminal", "file-manager", "editor", "widgets-demo"];

/// The four desktop icons.
pub const ICONS: [Icon; 4] = [
    Icon {
        label: "Terminal",
        body: 0x001A2A3A,
        detail: 0x0068AAC4,
    },
    Icon {
        label: "Files",
        body: 0x00221A0E,
        detail: 0x00D8A24A,
    },
    Icon {
        label: "Editor",
        body: 0x001E2A22,
        detail: 0x007BC88B,
    },
    Icon {
        label: "Widgets",
        body: 0x002A1E2A,
        detail: 0x00C88BD0,
    },
];

/// The first icon's top edge, in pixels.
const FIRST_TOP: i32 = 16;
/// The height of one icon cell, in pixels.
const CELL: i32 = 96;
/// The width of the glyph box, in pixels.
const GLYPH: i32 = 56;

/// The cell bounds of icon `index`, or `None` if the index is out of range.
pub fn cell(index: usize) -> Option<(i32, i32, i32, i32)> {
    if index >= ICONS.len() {
        return None;
    }
    let top = FIRST_TOP + index as i32 * CELL;
    Some((16, top, 112, top + CELL))
}

/// The cell under `(x, y)`, as an icon index.
pub fn hit(x: i32, y: i32) -> Option<u8> {
    for index in 0..ICONS.len() {
        let (x0, y0, x1, y1) = cell(index)?;
        if x >= x0 && x < x1 && y >= y0 && y < y1 {
            return Some(index as u8);
        }
    }
    None
}

/// Draw every icon, with `pressed` slightly inset and brighter.
pub fn draw_all(canvas: &mut Canvas, pressed: Option<u8>) {
    for (index, icon) in ICONS.iter().enumerate() {
        let (x0, y0, _x1, _y1) = match cell(index) {
            Some(cell) => cell,
            None => continue,
        };
        draw_icon(canvas, x0 + 12, y0 + 6, icon, pressed == Some(index as u8));
    }
}

/// Draw one icon at `(x, y)`, which is its glyph's top-left.
fn draw_icon(canvas: &mut Canvas, x: i32, y: i32, icon: &Icon, pressed: bool) {
    let lift = if pressed { -1 } else { 0 };
    let y = y + lift;

    // A soft plate behind the glyph — makes the icon readable on any
    // wallpaper without a background asset.
    canvas.fill_rect(
        nova_ui::Rect::new(x - 4, y - 2, (GLYPH + 8) as u32, (GLYPH + 18) as u32),
        0x0017222E,
    );
    canvas.stroke_rect(
        nova_ui::Rect::new(x - 4, y - 2, (GLYPH + 8) as u32, (GLYPH + 18) as u32),
        0x001E2C3C,
    );

    match icon.label {
        "Terminal" => glyph_terminal(canvas, x, y, icon),
        "Files" => glyph_files(canvas, x, y, icon),
        "Editor" => glyph_editor(canvas, x, y, icon),
        "Widgets" => glyph_widgets(canvas, x, y, icon),
        _ => {}
    }

    // The label, drawn with a one-pixel shadow for legibility.
    canvas.draw_text(x - 2, y + GLYPH + 4, icon.label, 0x00000000, None);
    canvas.draw_text(x - 3, y + GLYPH + 3, icon.label, crate::LABEL, None);
}

/// The terminal glyph: a dark screen with a cyan prompt.
fn glyph_terminal(canvas: &mut Canvas, x: i32, y: i32, icon: &Icon) {
    // Screen frame.
    canvas.fill_rect(nova_ui::Rect::new(x, y + 6, GLYPH as u32, 40), 0x000A1218);
    canvas.stroke_rect(nova_ui::Rect::new(x, y + 6, GLYPH as u32, 40), icon.detail);
    // Prompt lines.
    canvas.fill_rect(nova_ui::Rect::new(x + 6, y + 14, 10, 2), icon.detail);
    canvas.fill_rect(nova_ui::Rect::new(x + 6, y + 22, 18, 2), icon.detail);
    canvas.fill_rect(nova_ui::Rect::new(x + 6, y + 30, 14, 2), icon.detail);
    // The cursor block.
    canvas.fill_rect(nova_ui::Rect::new(x + 22, y + 30, 6, 8), icon.detail);
}

/// The files glyph: a manila folder.
fn glyph_files(canvas: &mut Canvas, x: i32, y: i32, icon: &Icon) {
    // The back tab.
    canvas.fill_rect(nova_ui::Rect::new(x + 2, y + 8, 22, 8), icon.detail);
    // The body.
    canvas.fill_rect(
        nova_ui::Rect::new(x + 2, y + 14, (GLYPH - 4) as u32, 32),
        icon.detail,
    );
    // A lighter front sheet, slightly lower, for depth.
    canvas.fill_rect(
        nova_ui::Rect::new(x + 6, y + 18, (GLYPH - 12) as u32, 26),
        0x00E8C47E,
    );
    // Two lines of "text" on the sheet.
    canvas.fill_rect(nova_ui::Rect::new(x + 12, y + 26, 16, 2), icon.body);
    canvas.fill_rect(nova_ui::Rect::new(x + 12, y + 34, 22, 2), icon.body);
}

/// The editor glyph: a pencil on a page.
fn glyph_editor(canvas: &mut Canvas, x: i32, y: i32, icon: &Icon) {
    // The page.
    canvas.fill_rect(nova_ui::Rect::new(x + 4, y + 4, 34, 44), 0x00F2F4F6);
    canvas.stroke_rect(nova_ui::Rect::new(x + 4, y + 4, 34, 44), icon.body);
    // The pencil — a diagonal bar built from steps.
    for step in 0..7 {
        let offset = step * 5;
        canvas.fill_rect(
            nova_ui::Rect::new(x + 18 + offset, y + 34 - offset, 6, 6),
            icon.detail,
        );
    }
    // The pencil tip.
    canvas.fill_rect(nova_ui::Rect::new(x + 52, y + 2, 8, 8), 0x00F2E4C8);
    // Text lines on the page.
    canvas.fill_rect(nova_ui::Rect::new(x + 10, y + 12, 20, 2), icon.body);
    canvas.fill_rect(nova_ui::Rect::new(x + 10, y + 20, 24, 2), icon.body);
    canvas.fill_rect(nova_ui::Rect::new(x + 10, y + 28, 16, 2), icon.body);
}

/// The widgets glyph: a 2×2 grid of rounded tiles.
fn glyph_widgets(canvas: &mut Canvas, x: i32, y: i32, icon: &Icon) {
    let tile = 22u32;
    let gap = 4u32;
    for (dx, dy) in [
        (0i32, 0i32),
        (tile as i32 + gap as i32, 0),
        (0, tile as i32 + gap as i32),
        (tile as i32 + gap as i32, tile as i32 + gap as i32),
    ] {
        canvas.fill_rect(
            nova_ui::Rect::new(x + dx, y + 6 + dy, tile, tile),
            icon.detail,
        );
    }
    // One tile gets the accent sparkle.
    canvas.fill_rect(
        nova_ui::Rect::new(x + tile as i32 + gap as i32 + 6, y + 12, 10, 10),
        crate::ACCENT,
    );
}
