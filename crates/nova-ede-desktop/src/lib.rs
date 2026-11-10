//! Rust port of `ede-desktop`, the desktop manager of EDE.
//!
//! ede-desktop owns the desktop background and the icon column. It reads
//! `.desktop` files from the desktop directory (`/home/nova/Desktop`) and
//! renders one icon per file with its `Name` label. A single click selects
//! an icon; a double click activates it by launching its `Exec` command.
//!
//! In NovaOS this crate draws the background window through the compositor.
//! Like every other EDE component it is a library of drawing and input
//! policy, not a window: the `ede-shell` application wires it to a surface.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::DesktopFile;

/// The desktop directory, relative to the user's home.
pub const DESKTOP_DIR: &str = "/home/nova/Desktop";

/// The longest field the desktop keeps from a `.desktop` file.
pub const FIELD_MAX: usize = 48;

/// How many icons one desktop holds.
pub const MAX_ICONS: usize = 16;

/// Columns per row of the icon grid.
pub const GRID_COLUMNS: usize = 4;

/// One cell of the icon grid, in pixels.
pub const CELL_WIDTH: i32 = 96;
pub const CELL_HEIGHT: i32 = 88;

/// A pixel rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
}

impl Rect {
    /// Construct a rectangle.
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether the point is inside.
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    /// The rectangle spanning both points, corners included.
    pub const fn spanning(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self {
            x: if x0 < x1 { x0 } else { x1 },
            y: if y0 < y1 { y0 } else { y1 },
            width: (x1 - x0).abs(),
            height: (y1 - y0).abs(),
        }
    }

    /// Whether the two rectangles overlap at all.
    pub const fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// One icon on the desktop: its `.desktop` file, its position, and its
/// selection state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopIcon {
    /// The icon's label (from `Name=`).
    pub label: [u8; FIELD_MAX],
    label_len: usize,
    /// The `Exec=` command (from `Exec=`), with field codes stripped.
    pub exec: [u8; FIELD_MAX],
    exec_len: usize,
    /// The `Icon=` theme name, when one was given.
    pub icon_name: [u8; FIELD_MAX],
    icon_name_len: usize,
    /// The `Comment=` description, when one was given — the tooltip text.
    pub comment: [u8; FIELD_MAX],
    comment_len: usize,
    /// The icon's cell rectangle on the desktop.
    pub cell: Rect,
    /// Whether the icon is currently selected.
    pub selected: bool,
}

impl DesktopIcon {
    /// The label as a string slice.
    pub fn label(&self) -> &str {
        core::str::from_utf8(&self.label[..self.label_len]).unwrap_or("?")
    }

    /// The `Exec` command as a string slice.
    pub fn exec(&self) -> &str {
        core::str::from_utf8(&self.exec[..self.exec_len]).unwrap_or("")
    }

    /// The `Icon=` theme name, when one was given.
    pub fn icon_name(&self) -> Option<&str> {
        if self.icon_name_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.icon_name[..self.icon_name_len]).ok()
    }

    /// The `Comment=` description, when one was given.
    pub fn comment(&self) -> Option<&str> {
        if self.comment_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.comment[..self.comment_len]).ok()
    }
}

/// The desktop's foreground set: the icon grid.
///
/// EDE draws its own gradient background; NovaOS's desktop shell draws the
/// wallpaper and hands it the canvas. This struct only owns the icon layer.
#[derive(Clone, Copy)]
pub struct DesktopLayer {
    icons: [Option<DesktopIcon>; MAX_ICONS],
    count: usize,
    /// The grid's left margin.
    margin_x: i32,
    margin_y: i32,
    /// Each slot's source basename, for refresh and position files.
    names: [[u8; BASENAME_MAX]; MAX_ICONS],
    name_lens: [usize; MAX_ICONS],
}

impl DesktopLayer {
    /// An empty desktop layer, ready to be filled from a directory scan.
    pub const fn new() -> Self {
        Self {
            icons: [None; MAX_ICONS],
            count: 0,
            margin_x: 12,
            margin_y: 16,
            names: [[0; BASENAME_MAX]; MAX_ICONS],
            name_lens: [0; MAX_ICONS],
        }
    }

    /// The first free slot: the caller's hint when it is free, otherwise
    /// the first free slot at all, otherwise `MAX_ICONS` (full).
    fn free_slot(&self, hint: usize) -> usize {
        if hint < MAX_ICONS && self.icons[hint].is_none() {
            return hint;
        }
        self.icons
            .iter()
            .position(|icon| icon.is_none())
            .unwrap_or(MAX_ICONS)
    }

    /// Parse a `.desktop` file's contents into an icon slot.
    ///
    /// `index` is the grid slot; icons flow down the first column and wrap
    /// into the next. Returns false when the file is not a launchable
    /// application.
    pub fn add_from_desktop(&mut self, contents: &[u8], index: usize) -> bool {
        self.add_named(contents, index, &[])
    }

    /// Parse a `.desktop` file into an icon slot, keyed by the file's
    /// basename so a re-scan can find the existing entry and keep its
    /// position.
    pub fn add_named(&mut self, contents: &[u8], index: usize, basename: &[u8]) -> bool {
        let file = DesktopFile::parse(contents);
        if !file.is_application() || !file.is_launchable() {
            return false;
        }
        // A re-scan of an existing entry replaces it in place, keeping its
        // cell and selection — EDE's desktop never shuffles icons the user
        // has arranged.
        let mut existing_position = None;
        if !basename.is_empty() {
            for (slot, entry) in self.names.iter().enumerate().take(MAX_ICONS) {
                if self.name_lens[slot] > 0 && &entry[..self.name_lens[slot]] == basename {
                    existing_position = Some(slot);
                    break;
                }
            }
        }
        let slot = existing_position.unwrap_or_else(|| self.free_slot(index));
        if slot >= MAX_ICONS {
            return false;
        }
        let (cell, selected) = match existing_position {
            Some(found) => {
                let icon = self.icons[found].unwrap();
                (icon.cell, icon.selected)
            }
            None => (self.cell_for(slot), false),
        };
        let mut icon = DesktopIcon {
            label: [0; FIELD_MAX],
            label_len: 0,
            exec: [0; FIELD_MAX],
            exec_len: 0,
            icon_name: [0; FIELD_MAX],
            icon_name_len: 0,
            comment: [0; FIELD_MAX],
            comment_len: 0,
            cell,
            selected,
        };
        copy_field(
            &mut icon.label,
            &mut icon.label_len,
            file.name().unwrap_or("?").as_bytes(),
        );
        copy_field(&mut icon.exec, &mut icon.exec_len, file.exec_bytes());
        copy_field(
            &mut icon.icon_name,
            &mut icon.icon_name_len,
            file.icon().unwrap_or("").as_bytes(),
        );
        copy_field(
            &mut icon.comment,
            &mut icon.comment_len,
            file.comment().unwrap_or("").as_bytes(),
        );
        self.icons[slot] = Some(icon);
        self.names[slot].fill(0);
        let take = basename.len().min(BASENAME_MAX);
        self.names[slot][..take].copy_from_slice(&basename[..take]);
        self.name_lens[slot] = take;
        if existing_position.is_none() {
            self.count += 1;
        }
        true
    }

    /// The grid cell for slot `slot`: down the first column, wrapping to
    /// the next column every [`GRID_COLUMNS`] entries.
    pub const fn cell_for(&self, slot: usize) -> Rect {
        let column = (slot / GRID_COLUMNS) as i32;
        let row = (slot % GRID_COLUMNS) as i32;
        Rect::new(
            self.margin_x + column * CELL_WIDTH,
            self.margin_y + row * CELL_HEIGHT,
            CELL_WIDTH,
            CELL_HEIGHT,
        )
    }

    /// Remove every icon not present in `keep`, a list of basenames the
    /// caller just re-scanned. Icons the user deleted disappear; everything
    /// else keeps its position. Returns how many icons were dropped.
    pub fn refresh(&mut self, keep: &[&[u8]]) -> usize {
        let mut dropped = 0;
        for slot in 0..MAX_ICONS {
            let name = self.names[slot];
            let length = self.name_lens[slot];
            if length == 0 {
                continue;
            }
            let survives = keep.iter().any(|kept| *kept == &name[..length]);
            if !survives {
                self.icons[slot] = None;
                self.name_lens[slot] = 0;
                dropped += 1;
            }
        }
        self.count = self.icons.iter().filter(|icon| icon.is_some()).count();
        dropped
    }

    /// The number of icons currently present.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the layer has no icons.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The icon at `index`, when present.
    pub fn icon(&self, index: usize) -> Option<&DesktopIcon> {
        self.icons.get(index)?.as_ref()
    }

    /// The icon at `index`, mutably.
    pub fn icon_mut(&mut self, index: usize) -> Option<&mut DesktopIcon> {
        self.icons.get_mut(index)?.as_mut()
    }

    /// Hit-test a point; the slot index of the icon under it, if any.
    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        self.icons
            .iter()
            .position(|icon| icon.is_some_and(|icon| icon.cell.contains(x, y)))
    }

    /// Select the icon at `index`, clearing the previous selection.
    /// Returns the index that ended up selected.
    pub fn select(&mut self, index: Option<usize>) -> Option<usize> {
        for icon in self.icons.iter_mut().flatten() {
            icon.selected = false;
        }
        let target = index?;
        let icon = self.icons.get_mut(target)?.as_mut()?;
        icon.selected = true;
        Some(target)
    }

    /// Add `index` to the selection without clearing the rest — the
    /// Shift-click behaviour.
    pub fn select_also(&mut self, index: usize) -> bool {
        match self.icons.get_mut(index) {
            Some(Some(icon)) => {
                icon.selected = true;
                true
            }
            _ => false,
        }
    }

    /// Select every icon whose cell intersects `band` — the rubber-band
    /// rectangle the user dragged across the desktop.
    pub fn select_band(&mut self, band: Rect) -> usize {
        let mut selected = 0;
        for icon in self.icons.iter_mut().flatten() {
            icon.selected = band.overlaps(&icon.cell);
            if icon.selected {
                selected += 1;
            }
        }
        selected
    }

    /// The currently selected icon, if any.
    pub fn selected(&self) -> Option<usize> {
        self.icons
            .iter()
            .position(|icon| icon.is_some_and(|icon| icon.selected))
    }

    /// How many icons are selected right now.
    pub fn selected_count(&self) -> usize {
        self.icons
            .iter()
            .filter(|icon| icon.is_some_and(|icon| icon.selected))
            .count()
    }

    /// Move the icon at `index` so its cell's top-left sits at `(x, y)`.
    /// The cell keeps its size, so dragging by a corner works.
    pub fn move_to(&mut self, index: usize, x: i32, y: i32) -> bool {
        match self.icons.get_mut(index) {
            Some(Some(icon)) => {
                icon.cell = Rect::new(x, y, icon.cell.width, icon.cell.height);
                true
            }
            _ => false,
        }
    }

    /// Snap every icon back to its grid slot — EDE's "auto-arrange".
    /// Icons keep their relative order.
    pub fn auto_arrange(&mut self) {
        let mut order = [0usize; MAX_ICONS];
        let mut filled = 0usize;
        for (slot, icon) in self.icons.iter().enumerate() {
            if icon.is_some() {
                order[filled] = slot;
                filled += 1;
            }
        }
        for (new_slot, old_slot) in order.iter().take(filled).enumerate() {
            let cell = self.cell_for(new_slot);
            let icon = self.icons[*old_slot].as_mut().unwrap();
            icon.cell = cell;
            let name = self.names[*old_slot];
            let length = self.name_lens[*old_slot];
            if new_slot != *old_slot {
                self.icons[new_slot] = self.icons[*old_slot];
                self.icons[*old_slot] = None;
                self.names[new_slot] = name;
                self.name_lens[new_slot] = length;
                self.names[*old_slot].fill(0);
                self.name_lens[*old_slot] = 0;
            }
        }
    }

    /// Serialize every icon's position as an `icons.conf` document — the
    /// file EDE writes under `~/.ede` so a session's arrangement survives.
    /// Returns the number of bytes written.
    pub fn save_positions(&self, out: &mut [u8]) -> usize {
        let mut used = 0usize;
        for slot in 0..MAX_ICONS {
            let Some(icon) = self.icons[slot] else {
                continue;
            };
            let name = &self.names[slot][..self.name_lens[slot]];
            let section = write(out, used, b"[");
            used = section;
            used = write(out, used, name);
            used = write(out, used, b"]\nX=");
            used = write_number(out, used, icon.cell.x);
            used = write(out, used, b"\nY=");
            used = write_number(out, used, icon.cell.y);
            used = write(out, used, b"\n\n");
        }
        used
    }

    /// Load one section of an `icons.conf` document: move the icon whose
    /// basename is `name` to `(x, y)`. Unknown names are ignored — a
    /// layout file for an icon that no longer exists is stale, not fatal.
    pub fn load_position(&mut self, name: &[u8], x: i32, y: i32) -> bool {
        for slot in 0..MAX_ICONS {
            if self.name_lens[slot] == 0 || &self.names[slot][..self.name_lens[slot]] != name {
                continue;
            }
            let Some(icon) = self.icons[slot].as_mut() else {
                return false;
            };
            icon.cell = Rect::new(x, y, icon.cell.width, icon.cell.height);
            return true;
        }
        false
    }
}

impl Default for DesktopLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// The longest basename a slot keeps — 31 bytes, the initramfs name limit.
const BASENAME_MAX: usize = 31;

fn copy_field(slot: &mut [u8; FIELD_MAX], len: &mut usize, value: &[u8]) {
    let take = value.len().min(FIELD_MAX);
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

fn write(out: &mut [u8], used: usize, bytes: &[u8]) -> usize {
    let take = bytes.len().min(out.len().saturating_sub(used));
    out[used..used + take].copy_from_slice(&bytes[..take]);
    used + take
}

fn write_number(out: &mut [u8], used: usize, value: i32) -> usize {
    let mut buffer = [0u8; 12];
    let mut length = 0usize;
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    if magnitude == 0 {
        buffer[0] = b'0';
        length = 1;
    }
    while magnitude > 0 {
        buffer[length] = b'0' + (magnitude % 10) as u8;
        length += 1;
        magnitude /= 10;
    }
    if negative {
        buffer[length] = b'-';
        length += 1;
    }
    for index in 0..length / 2 {
        buffer.swap(index, length - index - 1);
    }
    write(out, used, &buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Desktop Entry]
Type=Application
Name=Terminal
Exec=terminal
Icon=utilities-terminal
Comment=A terminal emulator
";

    fn layer_with(count: usize) -> DesktopLayer {
        let mut layer = DesktopLayer::new();
        for index in 0..count {
            let mut name = [0u8; 31];
            name[0] = b'i';
            name[1] = b'0' + index as u8;
            name[2..10].copy_from_slice(b".desktop");
            assert!(layer.add_named(SAMPLE.as_bytes(), index, &name[..10]));
        }
        layer
    }

    #[test]
    fn a_valid_desktop_file_becomes_an_icon() {
        let mut layer = DesktopLayer::new();
        assert!(layer.add_from_desktop(SAMPLE.as_bytes(), 0));
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.icon(0).unwrap().label(), "Terminal");
        assert_eq!(layer.icon(0).unwrap().exec(), "terminal");
        assert_eq!(layer.hit(30, 40), Some(0));
        assert_eq!(layer.hit(300, 40), None);
    }

    #[test]
    fn icon_fields_carry_through() {
        let layer = layer_with(1);
        let icon = layer.icon(0).unwrap();
        assert_eq!(icon.icon_name(), Some("utilities-terminal"));
        assert_eq!(icon.comment(), Some("A terminal emulator"));
    }

    #[test]
    fn a_non_application_entry_is_dropped() {
        let mut layer = DesktopLayer::new();
        let directory = b"[Desktop Entry]\nType=Directory\nName=Foo\n";
        assert!(!layer.add_from_desktop(directory, 0));
        assert_eq!(layer.len(), 0);
    }

    #[test]
    fn selection_clears_previous() {
        let mut layer = layer_with(2);
        assert_eq!(layer.select(Some(0)), Some(0));
        assert_eq!(layer.selected(), Some(0));
        assert_eq!(layer.select(Some(1)), Some(1));
        assert_eq!(layer.selected(), Some(1));
        assert_eq!(layer.select(None), None);
        assert_eq!(layer.selected(), None);
    }

    #[test]
    fn the_grid_wraps_after_four_columns() {
        let layer = layer_with(5);
        let first = layer.cell_for(0);
        let fifth = layer.cell_for(4);
        assert_eq!(first.y, fifth.y);
        assert!(fifth.x > first.x);
    }

    #[test]
    fn shift_click_extends_the_selection() {
        let mut layer = layer_with(3);
        layer.select(Some(0));
        assert!(layer.select_also(2));
        assert_eq!(layer.selected_count(), 2);
    }

    #[test]
    fn a_band_selects_what_it_touches() {
        let mut layer = layer_with(4);
        let first = layer.cell_for(0);
        let band = Rect::spanning(first.x - 4, first.y - 4, first.x + 8, first.y + 8);
        assert_eq!(layer.select_band(band), 1);
        assert_eq!(layer.selected_count(), 1);
    }

    #[test]
    fn icons_can_be_dragged() {
        let mut layer = layer_with(1);
        assert!(layer.move_to(0, 500, 300));
        assert_eq!(layer.hit(510, 310), Some(0));
        assert_eq!(layer.hit(30, 40), None);
    }

    #[test]
    fn auto_arrange_snaps_back_to_the_grid() {
        let mut layer = layer_with(3);
        layer.move_to(0, 700, 600);
        layer.auto_arrange();
        assert_eq!(layer.icon(0).unwrap().cell, layer.cell_for(0));
        assert_eq!(layer.icon(1).unwrap().cell, layer.cell_for(1));
    }

    #[test]
    fn positions_round_trip_through_the_config_file() {
        let mut layer = layer_with(2);
        layer.move_to(0, 400, 200);
        let mut buffer = [0u8; 512];
        let length = layer.save_positions(&mut buffer);
        assert!(length > 0);
        assert!(layer.load_position(b"i0.desktop", 400, 200));
        let icon = layer.icon(0).unwrap();
        assert_eq!(icon.cell.x, 400);
        assert_eq!(icon.cell.y, 200);
    }

    #[test]
    fn refresh_drops_removed_and_keeps_kept_entries() {
        let mut layer = DesktopLayer::new();
        assert!(layer.add_named(SAMPLE.as_bytes(), 0, b"terminal.desktop"));
        assert!(layer.add_named(SAMPLE.as_bytes(), 1, b"editor.desktop"));
        assert_eq!(layer.len(), 2);
        assert_eq!(layer.refresh(&[b"terminal.desktop"]), 1);
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.icon(0).unwrap().label(), "Terminal");
    }

    #[test]
    fn a_rescan_keeps_the_existing_cell() {
        let mut layer = DesktopLayer::new();
        assert!(layer.add_named(SAMPLE.as_bytes(), 0, b"terminal.desktop"));
        layer.move_to(0, 640, 480);
        layer.select(Some(0));
        assert!(layer.add_named(SAMPLE.as_bytes(), 0, b"terminal.desktop"));
        assert_eq!(layer.len(), 1, "a re-scan must not duplicate");
        assert_eq!(layer.icon(0).unwrap().cell.x, 640);
        assert_eq!(layer.selected(), Some(0));
    }
}
