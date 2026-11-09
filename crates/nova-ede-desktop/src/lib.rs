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
}

/// The desktop's foreground set: the icon column.
///
/// EDE draws its own gradient background; NovaOS's desktop shell draws the
/// wallpaper and hands it the canvas. This struct only owns the icon layer.
#[derive(Clone, Copy)]
pub struct DesktopLayer {
    icons: [Option<DesktopIcon>; 16],
    count: usize,
}

impl DesktopLayer {
    /// An empty desktop layer, ready to be filled from a directory scan.
    pub const fn new() -> Self {
        Self {
            icons: [None; 16],
            count: 0,
        }
    }

    /// Parse a `.desktop` file's contents into an icon slot.
    ///
    /// `index` is the vertical slot; the icon is placed in a column down the
    /// left edge. Returns false when the file is not a launchable application.
    pub fn add_from_desktop(&mut self, contents: &[u8], index: usize) -> bool {
        let file = DesktopFile::parse(contents);
        if !file.is_application() || !file.is_launchable() {
            return false;
        }
        let Some(slot) = self.icons.get_mut(index) else {
            return false;
        };
        let mut icon = DesktopIcon {
            label: [0; FIELD_MAX],
            label_len: 0,
            exec: [0; FIELD_MAX],
            exec_len: 0,
            cell: Rect::new(12, 16 + index as i32 * 96, 96, 88),
            selected: false,
        };
        copy_field(
            &mut icon.label,
            &mut icon.label_len,
            file.name().unwrap_or("?").as_bytes(),
        );
        copy_field(&mut icon.exec, &mut icon.exec_len, file.exec_bytes());
        *slot = Some(icon);
        self.count += 1;
        true
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

    /// The currently selected icon, if any.
    pub fn selected(&self) -> Option<usize> {
        self.icons
            .iter()
            .position(|icon| icon.is_some_and(|icon| icon.selected))
    }
}

impl Default for DesktopLayer {
    fn default() -> Self {
        Self::new()
    }
}

fn copy_field(slot: &mut [u8; FIELD_MAX], len: &mut usize, value: &[u8]) {
    let take = value.len().min(FIELD_MAX);
    slot[..take].copy_from_slice(&value[..take]);
    *len = take;
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Desktop Entry]
Type=Application
Name=Terminal
Exec=terminal
";

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
    fn a_non_application_entry_is_dropped() {
        let mut layer = DesktopLayer::new();
        let directory = b"[Desktop Entry]\nType=Directory\nName=Foo\n";
        assert!(!layer.add_from_desktop(directory, 0));
        assert_eq!(layer.len(), 0);
    }

    #[test]
    fn selection_clears_previous() {
        let mut layer = DesktopLayer::new();
        layer.add_from_desktop(SAMPLE.as_bytes(), 0);
        layer.add_from_desktop(SAMPLE.as_bytes(), 1);
        assert_eq!(layer.select(Some(0)), Some(0));
        assert_eq!(layer.selected(), Some(0));
        assert_eq!(layer.select(Some(1)), Some(1));
        assert_eq!(layer.selected(), Some(1));
        assert_eq!(layer.select(None), None);
        assert_eq!(layer.selected(), None);
    }
}
