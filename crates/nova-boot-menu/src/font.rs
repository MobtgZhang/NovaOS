//! The boot menu's typeface: embedded Iosevka, rendered through `nova-type`.
//!
//! # Why vector here
//!
//! The boot manager is the first screen a person sees; it draws its text from
//! the same embedded Iosevka outlines the kernel console and the userland
//! render with — any pixel size, anti-aliased by 4×4 super-sampling. There is
//! no bitmap font anywhere in the system to fork at boot time.
//!
//! # Ownership shape
//!
//! The renderer is a plain struct, not a static: the UEFI application has an
//! allocator, but the menu draws from one owned [`Typeface`] passed by
//! `&mut` through the draw calls, which keeps the borrow graph obvious and
//! costs no global state.

use nova_type::parser::{Font, Outline};
use nova_type::raster::{GlyphBuffer, rasterize};
use nova_type::{GlyphBitmap, GlyphCache};

/// The embedded menu face: Iosevka, subset to ASCII, arrows, and box drawing
/// by `tools/fonts/build.py` — the same bytes the kernel console embeds.
pub const FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/iosevka-console.ttf");

/// One menu text renderer: parsed font, glyph cache, and scratch buffers.
pub struct Typeface {
    /// Parsed once on first use; `None` until then.
    font: Option<Font<'static>>,
    /// Rasterized-glyph memo shared across every character drawn.
    cache: GlyphCache,
    /// Scratch one rasterization renders into.
    scratch: GlyphBuffer,
    /// Scratch one glyph's outline decodes into.
    outline: Outline,
}

impl Default for Typeface {
    fn default() -> Self {
        Self::new()
    }
}

impl Typeface {
    /// An empty renderer; the font parses on first draw.
    pub const fn new() -> Self {
        Self {
            font: None,
            cache: GlyphCache::new(),
            scratch: GlyphBuffer::new(),
            outline: Outline::empty(),
        }
    }

    /// The width of `text` at `size`, in pixels: Iosevka is monospaced, so
    /// the advance of one glyph times the character count.
    pub fn text_width(&mut self, text: &str, size: u32) -> usize {
        self.ensure_font();
        let Some(font) = self.font.as_ref() else {
            return 0;
        };
        let advance = font
            .glyph_for_char('N')
            .map(|glyph| font.advance(glyph, size))
            .unwrap_or(size / 2);
        text.chars().count() * advance as usize
    }

    /// Rasterize one character at `size`, returning the copied bitmap and the
    /// character's horizontal advance in pixels.
    ///
    /// `None` for a character the subset does not cover: the caller skips it.
    pub fn glyph(&mut self, ch: char, size: u32) -> Option<(GlyphBitmap, u32)> {
        self.ensure_font();
        let font = self.font.as_ref()?;
        let glyph = font.glyph_for_char(ch)?;
        if let Some(cached) = self.cache.lookup(glyph, size) {
            return Some((*cached, font.advance(glyph, size)));
        }
        font.outline(glyph, &mut self.outline).ok()?;
        rasterize(&self.outline, font.units_per_em(), size, &mut self.scratch);
        let rendered = *self.scratch.bitmap();
        self.cache.remember(glyph, size, &rendered);
        Some((rendered, font.advance(glyph, size)))
    }

    /// Parse the embedded font if it has not been yet.
    ///
    /// The embedded bytes come from the committed build pipeline, so a parse
    /// failure is a broken build; the menu falls back to drawing nothing
    /// rather than panicking in firmware.
    fn ensure_font(&mut self) {
        if self.font.is_none() {
            self.font = Font::parse(FONT_BYTES).ok();
        }
    }

    /// The pixels a line of text occupies above the baseline at `size`.
    pub fn ascent(&mut self, size: u32) -> u32 {
        self.ensure_font();
        self.font
            .as_ref()
            .map(|font| font.line_metrics(size).ascent)
            .unwrap_or(size)
    }
}
