//! Scalable vector typography for NovaOS.
//!
//! # What this is
//!
//! A TrueType font parser and a scalable, anti-aliased glyph rasterizer,
//! written for environments with no allocator, no standard library, and no
//! floating point: the kernel's boot console, the UEFI boot manager, and
//! ring-3 applications all render their text through this one crate. Fonts
//! are *outline* fonts — a glyph is a set of quadratic Bézier contours in
//! font units, scaled to whatever pixel size the caller asks for — which is
//! what makes every size a first-class size, unlike a bitmap font where one
//! size is the only size that was ever drawn by hand.
//!
//! # The three parts
//!
//! - [`Font`]: a borrowed view over the font's bytes. Parsing touches no
//!   allocation — the tables are located, not copied — so a font mapped
//!   read-only into an address space is rendered in place.
//! - [`GlyphBitmap`]: one rasterized glyph at one pixel size: its ink
//!   bounding box and a row-major coverage map, 0 = transparent,
//!   255 = solid. The caller decides what a coverage value means: blend it
//!   over a background, threshold it, tint it.
//! - [`GlyphCache`]: a fixed-slot memo for rasterized glyphs. Rasterizing is
//!   the expensive part (hundreds of point-in-contour tests per glyph), and
//!   a terminal redraws the same glyphs thousands of times, so the cache is
//!   what makes text scrolling cheap instead of merely correct.
//!
//! # Why integers everywhere
//!
//! Every coordinate is handled in 26.6 fixed point — 64 subpixel steps per
//! pixel, the same convention the classic font engines use — and every
//! accumulation in wider integers. The kernel does not enable the SIMD
//! units a `float` would lower to, and a text engine that renders
//! differently depending on which CPU's rounding mode is current is not a
//! text engine anyone can pin tests onto. Fixed point renders identically
//! everywhere, which is the property the tests rely on.
//!
//! # What is deliberately not here
//!
//! Hinting (the glyph outlines are rendered free), kerning pairs (the fonts
//! NovaOS ships are monospaced or UI faces where it is not missed), and
//! variable-font axis interpolation. None of it changes what a caller can
//! do; all of it is listed so its absence is a decision, not a gap.

#![no_std]
#![forbid(unsafe_code)]

pub mod cache;
pub mod parser;
pub mod raster;

pub use cache::GlyphCache;
pub use parser::{Font, FontError, GlyphId, LineMetrics, Outline};
pub use raster::{GlyphBitmap, GlyphBuffer, MAX_GLYPH_HEIGHT, MAX_GLYPH_WIDTH, SUBPIXEL_STEPS};

/// A [`GlyphBitmap`] whose coverage lives in a buffer the caller can see.
///
/// [`GlyphBitmap`] owns its rows in a fixed array; a view names wherever a
/// copy of those rows currently lives — a cache slot or a scratch buffer —
/// so drawing code never cares which.
#[derive(Clone, Copy)]
pub struct GlyphBitmapView<'a> {
    /// The glyph's layout: ink box relative to the pen, coverage rows.
    pub left: i32,
    /// Pixels the ink extends above the baseline (positive) or below (negative).
    pub top: i32,
    /// Coverage rows, `height` rows of `width` bytes each, row-major.
    pub coverage: &'a [u8],
    pub width: u32,
    pub height: u32,
}

impl GlyphBitmapView<'_> {
    /// The alpha at one pixel of the bitmap, or 0 outside it.
    pub fn at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        let index = (y as usize) * (self.width as usize) + x as usize;
        self.coverage.get(index).copied().unwrap_or(0)
    }
}
