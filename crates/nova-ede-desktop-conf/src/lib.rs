//! Rust port of `ede-desktop-conf`, EDE's background-and-icons
//! settings dialog.
//!
//! Upstream edits two groups of keys and writes them through edelib's
//! resource store, exactly as `desktop.conf` reads them back:
//!
//! ```text
//! [Desktop]
//! color = 33342a            ; background colour, hex
//! wallpaper_use = true
//! wallpaper_mode = 0        ; 0 scale, 1 tile
//! wallpaper = /path/to/img
//!
//! [Icons]
//! label_background = 000000
//! label_foreground = ffffff
//! label_font = 0
//! label_fontsize = 12
//! label_maxwidth = 120
//! label_transparent = true
//! label_visible = true
//! one_click_exec = false
//! ```
//!
//! The dialog itself is drawn by the shell; this crate is the settings
//! model with its defaults, its clamping, and its round trip.

#![no_std]
#![forbid(unsafe_code)]

use nova_edelib::EdeConfig;

/// The longest wallpaper path kept.
pub const PATH_MAX: usize = 128;

/// The desktop's background mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WallpaperMode {
    /// Scale the image to fill the screen.
    Scale,
    /// Tile the image at its natural size.
    Tile,
}

impl WallpaperMode {
    /// The numeric value upstream stores.
    pub const fn stored(self) -> u32 {
        match self {
            WallpaperMode::Scale => 0,
            WallpaperMode::Tile => 1,
        }
    }

    /// Read the stored value; anything but 1 is the default scale.
    pub fn from_stored(value: u32) -> Self {
        match value {
            1 => WallpaperMode::Tile,
            _ => WallpaperMode::Scale,
        }
    }
}

/// The desktop-and-icons settings, with upstream's defaults.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopConf {
    /// The background colour as a packed 0xRRGGBB value.
    pub color: u32,
    /// Whether a wallpaper image is shown at all.
    pub wallpaper_use: bool,
    /// How the wallpaper is drawn.
    pub wallpaper_mode: WallpaperMode,
    /// The wallpaper path; empty when none is set.
    pub wallpaper: [u8; PATH_MAX],
    wallpaper_len: usize,
    /// The icon label background, 0xRRGGBB.
    pub label_background: u32,
    /// The icon label foreground, 0xRRGGBB.
    pub label_foreground: u32,
    /// The label font index.
    pub label_font: u32,
    /// The label font size.
    pub label_fontsize: u32,
    /// The label's maximum width in pixels.
    pub label_maxwidth: u32,
    /// Whether the label background is transparent.
    pub label_transparent: bool,
    /// Whether labels are visible at all.
    pub label_visible: bool,
    /// Whether one click activates an icon (two clicks otherwise).
    pub one_click_exec: bool,
}

impl Default for DesktopConf {
    fn default() -> Self {
        Self {
            color: 0x33342a,
            wallpaper_use: false,
            wallpaper_mode: WallpaperMode::Scale,
            wallpaper: [0; PATH_MAX],
            wallpaper_len: 0,
            label_background: 0x000000,
            label_foreground: 0xffffff,
            label_font: 0,
            label_fontsize: 12,
            label_maxwidth: 120,
            label_transparent: true,
            label_visible: true,
            one_click_exec: false,
        }
    }
}

impl DesktopConf {
    /// Read every key from a config; missing keys keep the defaults.
    pub fn from_config(config: &EdeConfig) -> Self {
        let mut settings = Self::default();
        if let Some(color) =
            (config.get_u32("Desktop", "color")).filter(|&color| color <= 0xff_ffff)
        {
            settings.color = color;
        }
        settings.wallpaper_use = config
            .get_bool("Desktop", "wallpaper_use")
            .unwrap_or(settings.wallpaper_use);
        if let Some(mode) = config.get_u32("Desktop", "wallpaper_mode") {
            settings.wallpaper_mode = WallpaperMode::from_stored(mode);
        }
        if let Some(path) = config.get("Desktop", "wallpaper") {
            let bytes = path.as_bytes();
            let take = bytes.len().min(PATH_MAX);
            settings.wallpaper[..take].copy_from_slice(&bytes[..take]);
            settings.wallpaper_len = take;
        }
        if let Some(color) =
            (config.get_u32("Icons", "label_background")).filter(|&color| color <= 0xff_ffff)
        {
            settings.label_background = color;
        }
        if let Some(color) =
            (config.get_u32("Icons", "label_foreground")).filter(|&color| color <= 0xff_ffff)
        {
            settings.label_foreground = color;
        }
        if let Some(font) = config.get_u32("Icons", "label_font") {
            settings.label_font = font;
        }
        if let Some(size) =
            (config.get_u32("Icons", "label_fontsize")).filter(|&size| (8..=32).contains(&size))
        {
            settings.label_fontsize = size;
        }
        if let Some(width) =
            (config.get_u32("Icons", "label_maxwidth")).filter(|&width| (16..=512).contains(&width))
        {
            settings.label_maxwidth = width;
        }
        if let Some(flag) = config.get_bool("Icons", "label_transparent") {
            settings.label_transparent = flag;
        }
        if let Some(flag) = config.get_bool("Icons", "label_visible") {
            settings.label_visible = flag;
        }
        if let Some(flag) = config.get_bool("Icons", "one_click_exec") {
            settings.one_click_exec = flag;
        }
        settings
    }

    /// Write every key, in upstream's order.
    pub fn to_config(&self, out: &mut [u8]) -> usize {
        let mut cursor = copy(out, 0, b"[Desktop]\n");
        cursor = write_u32(out, cursor, b"color", self.color);
        cursor = write_bool(out, cursor, b"wallpaper_use", self.wallpaper_use);
        cursor = write_u32(out, cursor, b"wallpaper_mode", self.wallpaper_mode.stored());
        cursor = copy(out, cursor, b"wallpaper = ");
        cursor = copy(out, cursor, &self.wallpaper[..self.wallpaper_len]);
        cursor = copy(out, cursor, b"\n\n[Icons]\n");
        cursor = write_u32(out, cursor, b"label_background", self.label_background);
        cursor = write_u32(out, cursor, b"label_foreground", self.label_foreground);
        cursor = write_u32(out, cursor, b"label_font", self.label_font);
        cursor = write_u32(out, cursor, b"label_fontsize", self.label_fontsize);
        cursor = write_u32(out, cursor, b"label_maxwidth", self.label_maxwidth);
        cursor = write_bool(out, cursor, b"label_transparent", self.label_transparent);
        cursor = write_bool(out, cursor, b"label_visible", self.label_visible);
        write_bool(out, cursor, b"one_click_exec", self.one_click_exec)
    }

    /// The wallpaper path as text, when one is set.
    pub fn wallpaper_path(&self) -> Option<&str> {
        if self.wallpaper_len == 0 {
            return None;
        }
        core::str::from_utf8(&self.wallpaper[..self.wallpaper_len]).ok()
    }
}

/// Append `key = value\n` with an unsigned number.
fn write_u32(out: &mut [u8], cursor: usize, key: &[u8], value: u32) -> usize {
    let mut buffer = [0u8; 10];
    let mut length = 0usize;
    let mut magnitude = value;
    if magnitude == 0 {
        buffer[0] = b'0';
        length = 1;
    }
    while magnitude > 0 {
        buffer[length] = b'0' + (magnitude % 10) as u8;
        length += 1;
        magnitude /= 10;
    }
    for index in 0..length / 2 {
        buffer.swap(index, length - index - 1);
    }
    let mut next = copy(out, cursor, key);
    next = copy(out, next, b" = ");
    next = copy(out, next, &buffer[..length]);
    copy(out, next, b"\n")
}

/// Append `key = true|false\n`.
fn write_bool(out: &mut [u8], cursor: usize, key: &[u8], value: bool) -> usize {
    let mut next = copy(out, cursor, key);
    next = copy(out, next, b" = ");
    next = copy(out, next, if value { b"true\n" } else { b"false\n" });
    next
}

/// Copy bytes into the buffer at `cursor`, returning the new cursor.
fn copy(out: &mut [u8], cursor: usize, value: &[u8]) -> usize {
    let take = value.len().min(out.len().saturating_sub(cursor));
    out[cursor..cursor + take].copy_from_slice(&value[..take]);
    cursor + take
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let settings = DesktopConf::default();
        assert_eq!(settings.color, 0x33342a);
        assert!(!settings.wallpaper_use);
        assert_eq!(settings.wallpaper_mode, WallpaperMode::Scale);
        assert_eq!(settings.label_fontsize, 12);
        assert!(settings.label_transparent);
        assert!(settings.label_visible);
        assert!(!settings.one_click_exec);
    }

    #[test]
    fn config_round_trips() {
        let settings = DesktopConf {
            color: 0x112233,
            wallpaper_use: true,
            wallpaper_mode: WallpaperMode::Tile,
            one_click_exec: true,
            ..DesktopConf::default()
        };
        let mut buffer = [0u8; 512];
        let length = settings.to_config(&mut buffer);
        let config = EdeConfig::parse(&buffer[..length]);
        let reloaded = DesktopConf::from_config(&config);
        assert_eq!(reloaded.color, settings.color);
        assert_eq!(reloaded.label_fontsize, 12);
        assert_eq!(reloaded.label_maxwidth, 120);
        assert_eq!(reloaded.wallpaper_mode, WallpaperMode::Tile);
        assert!(reloaded.one_click_exec);
    }

    #[test]
    fn a_missing_config_keeps_the_defaults() {
        let config = EdeConfig::parse(b"[Desktop]\n");
        assert_eq!(DesktopConf::from_config(&config), DesktopConf::default());
    }

    #[test]
    fn out_of_range_numbers_are_ignored() {
        let config = EdeConfig::parse(
            b"[Desktop]\ncolor = 99999999\n[Icons]\nlabel_fontsize = 900\nlabel_maxwidth = 1\n",
        );
        let settings = DesktopConf::from_config(&config);
        assert_eq!(settings.color, 0x33342a, "colour above 0xffffff ignored");
        assert_eq!(settings.label_fontsize, 12, "out of range ignored");
        assert_eq!(settings.label_maxwidth, 120, "out of range ignored");
    }

    #[test]
    fn the_wallpaper_path_reads_back() {
        let config = EdeConfig::parse(b"[Desktop]\nwallpaper = /home/nova/wall.ppm\n");
        let settings = DesktopConf::from_config(&config);
        assert_eq!(settings.wallpaper_path(), Some("/home/nova/wall.ppm"));
    }

    #[test]
    fn wallpaper_modes_store_the_upstream_numbers() {
        assert_eq!(WallpaperMode::Scale.stored(), 0);
        assert_eq!(WallpaperMode::Tile.stored(), 1);
        assert_eq!(WallpaperMode::from_stored(1), WallpaperMode::Tile);
        assert_eq!(WallpaperMode::from_stored(7), WallpaperMode::Scale);
    }
}
