//! Fixed-slot glyph caching.
//!
//! Rasterizing is the expensive part of text: hundreds of intersection
//! tests per glyph. A terminal redraws the same glyphs thousands of times,
//! so the results are remembered. The cache is a fixed ring of slots — no
//! allocation, no eviction policy more clever than round-robin, and a key
//! of (glyph, size) because a cache that forgets the size would happily
//! return a 12-px glyph where 16 px was asked for.

use crate::parser::GlyphId;
use crate::raster::GlyphBitmap;

/// How many rasterized glyphs are remembered between draws.
pub const SLOT_COUNT: usize = 32;

/// One cache slot: the key plus the glyph it produced.
#[derive(Clone, Copy)]
struct Slot {
    filled: bool,
    glyph: u16,
    size: u16,
    bitmap: GlyphBitmap,
}

/// A fixed-slot memo for rasterized glyphs.
///
/// Owned by whoever owns the text stack (a `static` in the kernel console,
/// a field of the UEFI menu, a field of a user application) and used
/// behind a `&mut` — one cache per renderer, never shared.
pub struct GlyphCache {
    slots: [Slot; SLOT_COUNT],
    /// Round-robin victim pointer: the next slot to overwrite.
    next: usize,
}

impl GlyphCache {
    /// An empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [Slot {
                filled: false,
                glyph: 0,
                size: 0,
                bitmap: GlyphBitmap::EMPTY,
            }; SLOT_COUNT],
            next: 0,
        }
    }

    /// Forget every cached glyph.
    ///
    /// Called when the font changes — the keys name glyphs *within one
    /// font*, so a new face invalidates the whole cache, not one slot.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.filled = false;
        }
        self.next = 0;
    }

    /// The cached bitmap for `glyph` at `size_px`, if one is remembered.
    pub fn lookup(&self, glyph: GlyphId, size_px: u32) -> Option<&GlyphBitmap> {
        let size = Self::size_key(size_px);
        let slot = self
            .slots
            .iter()
            .find(|slot| slot.filled && slot.glyph == glyph.0 && slot.size == size)?;
        Some(&slot.bitmap)
    }

    /// Remember a rasterization, overwriting the round-robin victim slot.
    ///
    /// The bitmap is copied by value — a cache hit must not depend on the
    /// caller's scratch buffer outliving the call.
    pub fn remember(&mut self, glyph: GlyphId, size_px: u32, bitmap: &GlyphBitmap) {
        let slot = &mut self.slots[self.next];
        slot.filled = true;
        slot.glyph = glyph.0;
        slot.size = Self::size_key(size_px);
        slot.bitmap = *bitmap;
        self.next = (self.next + 1) % SLOT_COUNT;
    }

    /// Sizes clamp into the key's `u16` — no real font renders at 65 536 px.
    fn size_key(size_px: u32) -> u16 {
        size_px.min(u16::MAX as u32) as u16
    }
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new()
    }
}
