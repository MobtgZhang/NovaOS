//! The vector typeface a process draws with, behind the canvas API.
//!
//! # What this is
//!
//! A process that wants vector text — the terminal's Sarasa Gothic, the
//! GUI applications' Source Han Sans — embeds the face's bytes and calls
//! [`set_face`] once at startup. From then on every
//! [`Canvas::draw_text`](crate::Canvas::draw_text) renders through
//! [`nova_type`]: true outlines, any pixel size, anti-aliased by 4×4
//! super-sampling. A process that never calls [`set_face`] keeps the
//! built-in 8×16 bitmap font, which is why the host tests need no font
//! bytes at all.
//!
//! # One face per process
//!
//! A user program is a single cooperative thread, so there is exactly one
//! active face and no locking: [`set_face`] records the bytes, the first
//! draw parses them, and the parsed face plus its glyph cache live in one
//! static slot for the process's lifetime. The slot's interior mutability
//! is confined to this module; the public surface takes and returns only
//! plain data.
//!
//! # No allocation
//!
//! `nova-type` parses and rasterises without an allocator, and the glyph
//! cache is a fixed 32-slot ring: a process's text stack is ~160 KiB of
//! `.bss`, sized once, not a heap that can fragment.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use nova_type::parser::{Font, Outline};
use nova_type::raster::{GlyphBuffer, rasterize};
use nova_type::{GlyphBitmap, GlyphCache};

/// The pixel size UI text renders at.
///
/// The bitmap fallback is 16 pixels tall and the desktop's window chrome is
/// laid out on 16-pixel rows, so the vector face renders at the same size
/// and a face swap changes the letterforms, not the layout.
pub const TEXT_SIZE: u32 = 16;

/// A parsed face with its per-process rendering state.
pub struct Face {
    /// The parsed outlines, borrowing the embedded bytes.
    font: Font<'static>,
    /// Rasterized-glyph memo, shared across every character drawn.
    cache: GlyphCache,
    /// Scratch one rasterization renders into.
    scratch: GlyphBuffer,
    /// Scratch one glyph's outline decodes into.
    outline: Outline,
}

impl Face {
    /// The width one character advances the pen by, in pixels.
    ///
    /// A character the face does not cover advances by the base cell: the
    /// monospace advance, which is also what a full-width CJK glyph's
    /// double-width advance is measured against.
    pub fn char_advance(&mut self, ch: char) -> u32 {
        self.font
            .glyph_for_char(ch)
            .map(|glyph| self.font.advance(glyph, TEXT_SIZE))
            .unwrap_or_else(|| self.base_advance())
    }

    /// The monospace base-cell advance at [`TEXT_SIZE`].
    pub fn base_advance(&self) -> u32 {
        self.font
            .glyph_for_char('N')
            .map(|glyph| self.font.advance(glyph, TEXT_SIZE))
            .unwrap_or(TEXT_SIZE / 2)
    }

    /// The height of one text row, in pixels: the face's line metrics.
    pub fn row_height(&self) -> u32 {
        self.font.line_metrics(TEXT_SIZE).height()
    }

    /// The pixels a line of text occupies above its baseline.
    pub fn ascent(&self) -> u32 {
        self.font.line_metrics(TEXT_SIZE).ascent
    }

    /// Rasterize one character at [`TEXT_SIZE`], returning the copied
    /// bitmap and the character's advance.
    ///
    /// `None` for a character the face does not cover: the caller decides
    /// what a missing glyph looks like rather than this module drawing a
    /// guess.
    pub fn glyph(&mut self, ch: char) -> Option<(GlyphBitmap, u32)> {
        let glyph = self.font.glyph_for_char(ch)?;
        if let Some(cached) = self.cache.lookup(glyph, TEXT_SIZE) {
            return Some((*cached, self.font.advance(glyph, TEXT_SIZE)));
        }

        self.font.outline(glyph, &mut self.outline).ok()?;
        rasterize(
            &self.outline,
            self.font.units_per_em(),
            TEXT_SIZE,
            &mut self.scratch,
        );
        let rendered = *self.scratch.bitmap();
        self.cache.remember(glyph, TEXT_SIZE, &rendered);
        Some((rendered, self.font.advance(glyph, TEXT_SIZE)))
    }
}

/// The embedded bytes, and the slot the parsed face lands in.
///
/// The bytes live behind atomics because [`set_face`] may be called with a
/// different face than a previous boot of the same program image; the parsed
/// face is invalidated and re-parsed on the next draw.
static BYTES_PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static BYTES_LEN: AtomicUsize = AtomicUsize::new(0);
static PARSED: AtomicBool = AtomicBool::new(false);

/// The parse-once slot.
struct Slot {
    face: UnsafeCell<Option<Face>>,
}

static SLOT: Slot = Slot {
    face: UnsafeCell::new(None),
};

// SAFETY: `Slot`'s only field is an `UnsafeCell` reached exclusively from
// the process's one cooperative thread — see the invariant above — so the
// shareable guarantee `Sync` is asked for is single-threaded by construction.
unsafe impl Sync for Slot {}

/// Select the face every subsequent [`Canvas::draw_text`](crate::Canvas::draw_text)
/// renders with, from embedded font bytes.
///
/// Call this once, before the first draw. The bytes must live for the
/// program's lifetime — an `include_bytes!` static, which is what the
/// applications pass.
pub fn set_face(bytes: &'static [u8]) {
    BYTES_PTR.store(bytes.as_ptr().cast_mut(), Ordering::Relaxed);
    BYTES_LEN.store(bytes.len(), Ordering::Relaxed);
    PARSED.store(false, Ordering::Relaxed);
}

/// Run `body` with the active face, if one is set and parses.
pub(crate) fn with_face<R>(body: impl FnOnce(&mut Face) -> R) -> Option<R> {
    active_face().map(body)
}

/// The active face, parsing the selected bytes on first use.
///
/// A parse failure — possible only for bytes that were not produced by the
/// font pipeline — reports `None` forever, and every draw falls back to the
/// bitmap font rather than half-rendering the vector one.
pub(crate) fn active_face() -> Option<&'static mut Face> {
    if !PARSED.load(Ordering::Acquire) {
        let bytes = ptr_slice()?;
        // SAFETY: the slot is only reached from the process's one thread
        // (see the invariant on `SLOT`), and a draw uses the face only
        // within the call that fetched it.
        let slot = unsafe { &mut *SLOT.face.get() };
        match Font::parse(bytes) {
            Ok(font) => {
                *slot = Some(Face {
                    font,
                    cache: GlyphCache::new(),
                    scratch: GlyphBuffer::new(),
                    outline: Outline::empty(),
                });
                PARSED.store(true, Ordering::Release);
            }
            Err(_) => return None,
        }
    }

    // SAFETY: as above — single-threaded access, and the face was written
    // before `PARSED` flipped.
    let slot = unsafe { &mut *SLOT.face.get() };
    slot.as_mut()
}

/// The active bytes as a slice, or `None` before [`set_face`].
fn ptr_slice() -> Option<&'static [u8]> {
    let pointer = BYTES_PTR.load(Ordering::Relaxed);
    if pointer.is_null() {
        return None;
    }
    let length = BYTES_LEN.load(Ordering::Relaxed);
    // SAFETY: `set_face` stored a `&'static [u8]`'s pointer and length
    // together, so the pair reconstructs exactly the slice that came in.
    Some(unsafe { core::slice::from_raw_parts(pointer, length) })
}
