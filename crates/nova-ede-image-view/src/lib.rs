//! Rust port of `ede-image-view`, EDE's image viewer.
//!
//! Upstream opens an image, fits it to the window, and lets the user
//! zoom in and out and pan. The format support here is the subset the
//! no_std world can parse with no dependencies: PBM (P1/P4), PGM (P2/P5)
//! and PPM (P3/P6) — the Netpbm family, which is what the tests and the
//! shell's thumbnails use. Everything else is the viewer's state machine:
//! fit-to-window, zoom steps, and the pan bounds that keep some part of
//! the image on screen.

#![no_std]
#![forbid(unsafe_code)]

/// The maximum samples one decoded image may hold — 64 KiB keeps the
/// inline buffer stack-safe while still covering every thumbnail and
/// icon the desktop asks for.
pub const MAX_PIXELS: usize = 1 << 16;
/// The zoom steps the viewer offers, as per-mille (‰) of the fit size.
pub const ZOOM_STEPS: [u32; 8] = [250, 500, 750, 1000, 1500, 2000, 4000, 8000];
/// The zoom step the viewer opens at: fit-to-window.
pub const ZOOM_FIT: u32 = 1000;

/// How a decoded image's samples are laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleKind {
    /// One bit per pixel, expanded to 0/255.
    Bitmap,
    /// One grey sample per pixel.
    Grey,
    /// Three samples per pixel, red green blue.
    Rgb,
}

/// A decoded Netpbm image: header facts plus the sample buffer.
#[derive(Clone, Copy)]
pub struct NetpbmImage {
    /// The image width in pixels.
    pub width: u32,
    /// The image height in pixels.
    pub height: u32,
    /// How the samples are laid out.
    pub kind: SampleKind,
    /// The maximum sample value from the header (PGM/PPM); 255 default.
    pub maxval: u32,
    /// The samples, row-major, `samples_per_pixel() * width * height`.
    pub samples: [u8; MAX_PIXELS],
    sample_count: usize,
}

impl NetpbmImage {
    /// An empty image.
    pub const fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            kind: SampleKind::Grey,
            maxval: 255,
            samples: [0; MAX_PIXELS],
            sample_count: 0,
        }
    }

    /// Samples per pixel for this image's kind.
    pub const fn samples_per_pixel(&self) -> usize {
        match self.kind {
            SampleKind::Bitmap | SampleKind::Grey => 1,
            SampleKind::Rgb => 3,
        }
    }

    /// How many samples the decode produced.
    pub const fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// The grey value of the pixel at (`x`, `y`), averaging RGB.
    pub fn grey_at(&self, x: u32, y: u32) -> u8 {
        let per = self.samples_per_pixel();
        let index = (y as usize * self.width as usize + x as usize) * per;
        match self.kind {
            SampleKind::Rgb => {
                let r = self.samples.get(index).copied().unwrap_or(0) as u32;
                let g = self.samples.get(index + 1).copied().unwrap_or(0) as u32;
                let b = self.samples.get(index + 2).copied().unwrap_or(0) as u32;
                ((r * 299 + g * 587 + b * 114) / 1000) as u8
            }
            _ => self.samples.get(index).copied().unwrap_or(0),
        }
    }
}

impl Default for NetpbmImage {
    fn default() -> Self {
        Self::new()
    }
}

/// The Netpbm decoder. `P1`/`P4` bitmaps, `P2`/`P5` greymaps, `P3`/`P6`
/// pixmaps; both the ASCII and the binary forms of each.
pub fn decode(bytes: &[u8]) -> Option<NetpbmImage> {
    let mut scanner = Scanner { bytes, pos: 0 };
    let magic = scanner.token()?;
    let kind = match magic {
        b"P1" => SampleKind::Bitmap,
        b"P2" => SampleKind::Grey,
        b"P3" => SampleKind::Rgb,
        b"P4" => SampleKind::Bitmap,
        b"P5" => SampleKind::Grey,
        b"P6" => SampleKind::Rgb,
        _ => return None,
    };
    let binary = matches!(magic, b"P4" | b"P5" | b"P6");
    let width = parse_u32(scanner.token()?)?;
    let height = parse_u32(scanner.token()?)?;
    if width == 0 || height == 0 {
        return None;
    }
    let pixels = width as usize * height as usize;
    if pixels > MAX_PIXELS {
        return None;
    }
    let per = match kind {
        SampleKind::Bitmap | SampleKind::Grey => 1,
        SampleKind::Rgb => 3,
    };
    if pixels * per > MAX_PIXELS {
        return None;
    }
    let mut image = NetpbmImage {
        width,
        height,
        kind,
        maxval: 255,
        samples: [0; MAX_PIXELS],
        sample_count: 0,
    };
    if kind != SampleKind::Bitmap {
        let maxval = parse_u32(scanner.token()?)?;
        if maxval == 0 || maxval > 255 {
            return None;
        }
        image.maxval = maxval;
    }
    let count = pixels * per;
    if kind == SampleKind::Bitmap && binary {
        // PBM binary packs 8 pixels per byte, MSB first, 1 = black;
        // expand row by row so the rest of the crate sees one grey
        // sample per pixel either way.
        let packed = (width as usize).div_ceil(8);
        scanner.skip_whitespace();
        for y in 0..height as usize {
            for chunk in 0..packed {
                let byte = scanner.raw_byte()?;
                for bit in 0..8 {
                    let x = chunk * 8 + bit;
                    if x >= width as usize {
                        break;
                    }
                    let black = byte & (0x80 >> bit) != 0;
                    image.samples[y * width as usize + x] = if black { 0 } else { 255 };
                }
            }
        }
        image.sample_count = pixels;
        return Some(image);
    }
    if binary {
        scanner.skip_whitespace();
        for slot in image.samples.iter_mut().take(count) {
            *slot = scanner.raw_byte()?;
        }
    } else {
        for slot in image.samples.iter_mut().take(count) {
            let value = parse_u32(scanner.token()?)?;
            // In PBM ASCII a `1` is black; expand to the grey scale
            // everything else in this crate speaks.
            if kind == SampleKind::Bitmap {
                *slot = if value != 0 { 0 } else { 255 };
            } else {
                *slot = value.min(255) as u8;
            }
        }
    }
    image.sample_count = count;
    Some(image)
}

/// Parse a decimal unsigned integer from bytes; overflow or junk is
/// `None` — no_std has no `str::parse` on byte slices.
fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

/// A byte scanner that skips `#` comments and whitespace between tokens.
struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    /// Skip whitespace and `#`-to-end-of-line comments.
    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            if byte == b'#' {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else if byte.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// The next whitespace-delimited token.
    fn token(&mut self) -> Option<&'a [u8]> {
        self.skip_whitespace();
        let start = self.pos;
        while self.pos < self.bytes.len() && !self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if start == self.pos {
            None
        } else {
            Some(&self.bytes[start..self.pos])
        }
    }

    /// One raw byte, for the binary formats.
    fn raw_byte(&mut self) -> Option<u8> {
        if self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            self.pos += 1;
            Some(byte)
        } else {
            None
        }
    }
}

/// The viewer's state: which image, which zoom, and the pan offset.
#[derive(Clone, Copy)]
pub struct Viewer {
    image_width: u32,
    image_height: u32,
    /// The zoom, in per-mille of the natural size.
    zoom_permille: u32,
    /// The pan offset, in screen pixels.
    pub pan_x: i32,
    pub pan_y: i32,
    /// The viewport the image is shown in.
    viewport_width: u32,
    viewport_height: u32,
}

impl Viewer {
    /// A viewer over an image, opened fit-to-window in `viewport`.
    pub fn open(
        image_width: u32,
        image_height: u32,
        viewport_width: u32,
        viewport_height: u32,
    ) -> Self {
        let mut viewer = Self {
            image_width,
            image_height,
            zoom_permille: ZOOM_FIT,
            pan_x: 0,
            pan_y: 0,
            viewport_width,
            viewport_height,
        };
        viewer.fit();
        viewer
    }

    /// The zoom in per-mille.
    pub const fn zoom_permille(&self) -> u32 {
        self.zoom_permille
    }

    /// The image's size on screen at the current zoom.
    pub const fn shown_size(&self) -> (u32, u32) {
        (
            self.image_width * self.zoom_permille / 1000,
            self.image_height * self.zoom_permille / 1000,
        )
    }

    /// Fit the image to the viewport — the opening state, and the
    /// "fit" button. Never upscales past 8000‰.
    pub fn fit(&mut self) {
        if self.image_width == 0 || self.image_height == 0 {
            self.zoom_permille = ZOOM_FIT;
            return;
        }
        let width_zoom = self.viewport_width as u64 * 1000 / self.image_width as u64;
        let height_zoom = self.viewport_height as u64 * 1000 / self.image_height as u64;
        let fit = width_zoom.min(height_zoom).min(8000) as u32;
        self.zoom_permille = fit.max(1);
        self.center();
    }

    /// Step to the next zoom in [`ZOOM_STEPS`], clamped at the top.
    pub fn zoom_in(&mut self) {
        self.zoom_permille = ZOOM_STEPS
            .iter()
            .copied()
            .find(|step| *step > self.zoom_permille)
            .unwrap_or(self.zoom_permille);
        self.clamp_pan();
    }

    /// Step to the next zoom out, clamped at the bottom.
    pub fn zoom_out(&mut self) {
        self.zoom_permille = ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|step| *step < self.zoom_permille)
            .unwrap_or(self.zoom_permille);
        self.clamp_pan();
    }

    /// Pan by a delta, keeping at least a pixel of the image visible.
    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.pan_x += dx;
        self.pan_y += dy;
        self.clamp_pan();
    }

    /// Keep the image on screen: never pan past an edge.
    fn clamp_pan(&mut self) {
        let (shown_width, shown_height) = self.shown_size();
        let max_x = shown_width as i32 - 1;
        let max_y = shown_height as i32 - 1;
        if self.pan_x > max_x {
            self.pan_x = max_x;
        }
        if self.pan_y > max_y {
            self.pan_y = max_y;
        }
        // And never show only empty space on the right/bottom.
        let min_x = -(shown_width as i32 - self.viewport_width as i32).max(0) - 1;
        let min_y = -(shown_height as i32 - self.viewport_height as i32).max(0) - 1;
        if self.pan_x < min_x {
            self.pan_x = min_x;
        }
        if self.pan_y < min_y {
            self.pan_y = min_y;
        }
    }

    /// Center the image in the viewport.
    fn center(&mut self) {
        let (shown_width, shown_height) = self.shown_size();
        self.pan_x = ((self.viewport_width as i64 - shown_width as i64) / 2) as i32;
        self.pan_y = ((self.viewport_height as i64 - shown_height as i64) / 2) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ascii_pgm_decodes() {
        // 2×2 grey map: corners dark, middle light.
        let bytes = b"P2\n# a comment\n2 2\n255\n0 128\n255 0\n";
        let image = decode(bytes).unwrap();
        assert_eq!(image.width, 2);
        assert_eq!(image.height, 2);
        assert_eq!(image.kind, SampleKind::Grey);
        assert_eq!(image.sample_count(), 4);
        assert_eq!(image.grey_at(0, 0), 0);
        assert_eq!(image.grey_at(1, 0), 128);
    }

    #[test]
    fn an_ascii_ppm_decodes_and_greyscales() {
        let bytes = b"P3\n2 1\n255\n255 0 0  0 255 0\n";
        let image = decode(bytes).unwrap();
        assert_eq!(image.kind, SampleKind::Rgb);
        assert_eq!(image.sample_count(), 6);
        // Pure red greyscales to ~76.
        let grey = image.grey_at(0, 0);
        assert_eq!(grey, 76);
    }

    #[test]
    fn a_binary_pgm_decodes() {
        let mut bytes = b"P5\n2 2\n255 ".to_vec();
        bytes.extend_from_slice(&[0, 64, 128, 255]);
        let image = decode(&bytes).unwrap();
        assert_eq!(image.sample_count(), 4);
        assert_eq!(image.grey_at(1, 1), 255);
    }

    #[test]
    fn a_binary_pbm_expands_bits() {
        // 4×2 bitmap: first byte 0b10110000, second 0b00100000.
        let mut bytes = b"P4\n4 2 ".to_vec();
        bytes.extend_from_slice(&[0b1011_0000, 0b0010_0000]);
        let image = decode(&bytes).unwrap();
        assert_eq!(image.kind, SampleKind::Bitmap);
        assert_eq!(image.sample_count(), 8);
        // 1 is black (0), 0 is white (255): row 0 = 1011 → dark, light,
        // dark, dark; row 1 byte 0b0010_0000 → bit 2 set → dark at x=2.
        assert_eq!(image.grey_at(0, 0), 0);
        assert_eq!(image.grey_at(1, 0), 255);
        assert_eq!(image.grey_at(2, 0), 0);
        assert_eq!(image.grey_at(3, 0), 0);
        assert_eq!(image.grey_at(2, 1), 0, "the second byte's third bit");
        assert_eq!(image.grey_at(0, 1), 255);
        assert_eq!(image.grey_at(1, 1), 255);
    }

    #[test]
    fn an_ascii_pbm_reads_digits_as_samples() {
        let bytes = b"P1\n3 1\n1 0 1\n";
        let image = decode(bytes).unwrap();
        assert_eq!(image.sample_count(), 3);
        assert_eq!(image.grey_at(0, 0), 0, "1 is black in PBM");
        assert_eq!(image.grey_at(1, 0), 255);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"JPEG stuff").is_none());
        assert!(decode(b"P2\n0 0\n255\n").is_none(), "zero dimensions");
        assert!(decode(b"P2\n2 2\n999\n0 0 0 0\n").is_none(), "maxval > 255");
        assert!(decode(b"P2\n2 2\n255\n0 0\n").is_none(), "truncated data");
    }

    #[test]
    fn an_oversized_image_is_rejected() {
        // 2000×2000×3 samples exceed MAX_PIXELS.
        let bytes = b"P3\n2000 2000\n255\n";
        assert!(decode(bytes).is_none());
    }

    #[test]
    fn the_viewer_opens_fitted() {
        let viewer = Viewer::open(100, 50, 200, 200);
        assert!(viewer.zoom_permille() >= 1000, "upscales small images");
        let (width, height) = viewer.shown_size();
        assert!(width <= 200 && height <= 200);
        // Centered.
        assert_eq!(viewer.pan_x, (200 - width as i32) / 2);
    }

    #[test]
    fn a_large_image_fits_down() {
        let viewer = Viewer::open(400, 300, 200, 200);
        let fit = viewer.zoom_permille();
        assert_eq!(fit, 500, "200/400 = 500‰");
        let (width, height) = viewer.shown_size();
        assert_eq!(width, 200);
        assert_eq!(height, 150);
    }

    #[test]
    fn zoom_steps_are_discrete() {
        let mut viewer = Viewer::open(400, 300, 200, 200);
        assert_eq!(viewer.zoom_permille(), 500);
        viewer.zoom_in();
        assert_eq!(viewer.zoom_permille(), 750);
        viewer.zoom_out();
        assert_eq!(viewer.zoom_permille(), 500);
        for _ in 0..10 {
            viewer.zoom_out();
        }
        assert_eq!(viewer.zoom_permille(), 250, "clamped at the bottom");
        for _ in 0..10 {
            viewer.zoom_in();
        }
        assert_eq!(viewer.zoom_permille(), 8000, "clamped at the top");
    }

    #[test]
    fn panning_never_loses_the_image() {
        let mut viewer = Viewer::open(800, 600, 200, 200);
        // Zoomed out to fit, panning does nothing useful but stays sane.
        viewer.pan(10, 10);
        let (width, height) = viewer.shown_size();
        assert!(viewer.pan_x <= width as i32);
        assert!(viewer.pan_y <= height as i32);
        // Zoomed in, big pans are clamped both ways.
        for _ in 0..5 {
            viewer.zoom_in();
        }
        viewer.pan(-100000, -100000);
        assert!(viewer.pan_x >= -100000, "clamped, not wrapped");
        let (width, _height) = viewer.shown_size();
        assert!(viewer.pan_x > -(width as i32), "some image stays on screen");
    }
}
