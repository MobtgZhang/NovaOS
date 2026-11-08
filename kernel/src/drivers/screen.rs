//! The on-screen kernel console: vector-rendered text in the framebuffer.
//!
//! # What this is
//!
//! The kernel's log lines go to the serial port, and — from here — to the
//! display the boot manager's GOP handed over. Every character is rasterized
//! from the embedded Iosevka outlines through the same [`nova_type`] engine
//! the UEFI menu and the user applications draw with: no bitmap fonts, any
//! pixel size, anti-aliased by 4×4 super-sampling.
//!
//! # Order of boot
//!
//! Logs start before the framebuffer exists (the serial console is up first)
//! and the display arrives later. Every line emitted while no display is
//! ready is buffered — bounded, truncating when full — and
//! [`on_display_ready`] replays the buffer onto the freshly mapped screen,
//! so the boot log's *beginning* is what a person sees, not its tail.
//!
//! # ASCII only
//!
//! The kernel console renders the printable ASCII range; bytes outside it
//! are skipped rather than drawn as boxes. Text in Chinese belongs to the
//! userland terminal, which embeds the Sarasa Gothic face and renders it
//! through the same crate.
//!
//! # Re-entrancy
//!
//! One [`SpinLock`] guards the whole console. The lock is re-entrant per
//! CPU, so a log call from inside a log call (a fault reporter re-entered
//! through the panic path) cannot deadlock; the inner call's bytes simply
//! interleave at a line boundary, which the logger's line-buffered output
//! already ensures.

use crate::drivers::display;
use crate::klog;
use crate::logger::Level;
use crate::sync::SpinLock;
use nova_type::parser::{Font, FontError, Outline};
use nova_type::raster::{GlyphBuffer, rasterize};
use nova_type::{GlyphBitmap, GlyphCache};

/// The embedded kernel console face: Iosevka, subset to ASCII, arrows, and
/// box drawing by `tools/fonts/build.py`.
const FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/iosevka-console.ttf");

/// The pixel size the console renders at. Iosevka is monospaced with a
/// half-em advance, so this yields an 8-pixel cell: 128 columns at 1024.
const FONT_SIZE_PX: u32 = 16;

/// Text colour, sRGB.
const FOREGROUND: (u8, u8, u8) = (230, 232, 236);

/// Background colour, sRGB: near-black, so alpha blending against the
/// cleared framebuffer is exactly "scale the glyph by its coverage".
const BACKGROUND: (u8, u8, u8) = (12, 12, 16);

/// Left margin of the text area, in pixels.
const MARGIN_X: u32 = 8;

/// The first line's baseline, in pixels from the top of the screen.
const FIRST_BASELINE: u32 = 16;

/// How many bytes of log may wait for a display that has not arrived.
///
/// The pre-display boot log is a few dozen short lines; 8 KiB holds it many
/// times over, and a headless boot (the test runner) discards the overflow
/// without any way for the log's writer to notice.
const PENDING_CAP: usize = 8 * 1024;

/// The whole on-screen console: font, cache, cursor, and the pending log
/// buffer for a display that has not arrived yet.
pub struct ScreenConsole {
    /// Parsed once on first use; `None` until then.
    font: Option<Font<'static>>,
    /// Rasterized-glyph memo, shared across every character drawn.
    cache: GlyphCache,
    /// Scratch one rasterization renders into.
    scratch: GlyphBuffer,
    /// Scratch one glyph's outline decodes into.
    outline: Outline,
    /// Pen x, in pixels.
    cursor_x: u32,
    /// Pen y — the current line's *baseline*, in pixels from the top.
    baseline_y: u32,
    /// Bytes logged before a display existed, replayed on arrival.
    pending: [u8; PENDING_CAP],
    /// How much of `pending` is real.
    pending_len: usize,
}

impl ScreenConsole {
    /// An empty console, with no font parsed yet.
    pub const fn new() -> Self {
        Self {
            font: None,
            cache: GlyphCache::new(),
            scratch: GlyphBuffer::new(),
            outline: Outline::empty(),
            cursor_x: MARGIN_X,
            baseline_y: FIRST_BASELINE,
            pending: [0; PENDING_CAP],
            pending_len: 0,
        }
    }

    /// Parse the embedded font if it has not been yet.
    ///
    /// The bytes are produced by the committed `tools/fonts/build.py`
    /// pipeline, so a parse failure means a broken build rather than
    /// hostile input; the console degrades to serial-only and says so once.
    fn ensure_font(&mut self) {
        if self.font.is_none() {
            match Font::parse(FONT_BYTES) {
                Ok(font) => self.font = Some(font),
                Err(error) => {
                    klog!(
                        Level::Error,
                        "screen: console font failed to parse: {}",
                        parse_error_code(error)
                    );
                }
            }
        }
    }

    /// One character cell's width: the monospace advance at the console
    /// size, or a half-em guess before the font has parsed.
    fn cell_advance(&self) -> u32 {
        let Some(font) = self.font.as_ref() else {
            return FONT_SIZE_PX / 2;
        };
        font.glyph_for_char('N')
            .map(|glyph| font.advance(glyph, FONT_SIZE_PX))
            .unwrap_or(FONT_SIZE_PX / 2)
    }

    /// Buffer bytes for a display that has not arrived, truncating when the
    /// bound is hit: a log nobody can see yet must never be able to stall
    /// the line that is replacing it.
    fn buffer_pending(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.pending_len < PENDING_CAP {
                self.pending[self.pending_len] = byte;
                self.pending_len += 1;
            }
        }
    }

    /// Feed one byte, drawing it or remembering it for the replay.
    fn feed(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.cursor_x = MARGIN_X,
            b'\t' => {
                // Tabs advance to the next multiple of four cells.
                let cell = self.cell_advance().max(1);
                let column = (self.cursor_x - MARGIN_X) / cell;
                let next = (column / 4 + 1) * 4;
                self.cursor_x = MARGIN_X + next * cell;
            }
            byte @ 0x20..=0x7E => self.draw_char(byte),
            // The kernel console is ASCII-only; everything else — including
            // the bytes of a multi-byte UTF-8 sequence — is skipped.
            _ => {}
        }
    }

    /// Move to the next line, scrolling the screen up one line when the
    /// bottom is reached.
    fn newline(&mut self) {
        self.cursor_x = MARGIN_X;
        let Some(font) = self.font.as_ref() else {
            return;
        };
        let metrics = font.line_metrics(FONT_SIZE_PX);
        let line_height = metrics.height();
        let Some(mode) = display::mode() else {
            return;
        };

        self.baseline_y += line_height;
        if self.baseline_y + metrics.descent > mode.height {
            // Scroll: every line moves up one line height, and the gap at
            // the bottom is repainted in the background colour.
            let _ = display::scroll_up(
                u64::from(line_height),
                BACKGROUND.0,
                BACKGROUND.1,
                BACKGROUND.2,
            );
            self.baseline_y = self.baseline_y.saturating_sub(line_height);
        }
    }

    /// Rasterize and draw one printable ASCII byte at the pen, then advance.
    fn draw_char(&mut self, byte: u8) {
        self.ensure_font();
        let Some(font) = self.font.as_ref() else {
            return;
        };
        let Some(glyph) = font.glyph_for_char(byte as char) else {
            return;
        };

        // Cache hit or fresh rasterization: either way the bitmap is owned
        // locally, so the draw below borrows nothing from the cache.
        let bitmap = match self.cache.lookup(glyph, FONT_SIZE_PX) {
            Some(cached) => *cached,
            None => {
                if font.outline(glyph, &mut self.outline).is_err() {
                    return;
                }
                rasterize(
                    &self.outline,
                    font.units_per_em(),
                    FONT_SIZE_PX,
                    &mut self.scratch,
                );
                let rendered = *self.scratch.bitmap();
                self.cache.remember(glyph, FONT_SIZE_PX, &rendered);
                rendered
            }
        };
        let advance = font.advance(glyph, FONT_SIZE_PX);

        self.blend_bitmap(&bitmap);
        self.cursor_x += advance;

        // Wrap when the next cell would run off the right edge.
        let right = display::mode().map(|mode| mode.width).unwrap_or(0);
        if right > 0 && self.cursor_x + advance > right {
            self.newline();
        }
    }

    /// Composite one glyph's coverage onto the framebuffer, alpha-blending
    /// against whatever is already on each pixel.
    fn blend_bitmap(&self, bitmap: &GlyphBitmap) {
        let Some(mode) = display::mode() else {
            return;
        };

        // The ink box relative to the pen: `left` offsets the columns,
        // `top` counts up from the baseline.
        let origin_x = i64::from(self.cursor_x) + i64::from(bitmap.left);
        let origin_y = i64::from(self.baseline_y) - i64::from(bitmap.top);

        for row in 0..bitmap.height {
            let y = origin_y + i64::from(row);
            if y < 0 || y >= i64::from(mode.height) {
                continue;
            }
            for column in 0..bitmap.width {
                let alpha = bitmap.coverage[row as usize * bitmap.width as usize + column as usize];
                if alpha == 0 {
                    continue;
                }
                let x = origin_x + i64::from(column);
                if x < 0 || x >= i64::from(mode.width) {
                    continue;
                }
                let Ok(existing) = display::get_pixel(x as u32, y as u32) else {
                    continue;
                };
                let (red, green, blue) = blend(
                    existing,
                    mode.red_shift,
                    mode.green_shift,
                    mode.blue_shift,
                    alpha,
                );
                let _ = display::put_pixel(x as u32, y as u32, red, green, blue);
            }
        }
    }

    /// Reset the canvas for a display that just arrived: clear to the
    /// background colour and home the cursor.
    fn reset(&mut self) {
        let _ = display::clear(BACKGROUND.0, BACKGROUND.1, BACKGROUND.2);
        self.cursor_x = MARGIN_X;
        self.baseline_y = FIRST_BASELINE;
    }
}

/// Blend a background pixel's channels with the glyph's coverage as alpha,
/// returning the composited sRGB channels.
fn blend(
    background: u32,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
    alpha: u8,
) -> (u8, u8, u8) {
    let alpha = u32::from(alpha);
    let channel = |shift: u8| -> u32 { (background >> shift) & 0xFF };
    let mix =
        |fg: u32, bg: u32| -> u8 { ((fg * alpha + bg * (255 - alpha) + 127) / 255).min(255) as u8 };
    (
        mix(u32::from(FOREGROUND.0), channel(red_shift)),
        mix(u32::from(FOREGROUND.1), channel(green_shift)),
        mix(u32::from(FOREGROUND.2), channel(blue_shift)),
    )
}

/// Collapse a parse error into a number the log format can carry.
fn parse_error_code(error: FontError) -> i64 {
    match error {
        FontError::Truncated => -1,
        FontError::BadHeader => -2,
        FontError::MissingTable => -3,
        FontError::UnsupportedOutline => -4,
        FontError::NoUnicodeCmap => -5,
        FontError::BadMetrics => -6,
    }
}

/// The one console. Constructed empty; the font parses on first use.
static CONSOLE: SpinLock<ScreenConsole> = SpinLock::new(ScreenConsole::new());

/// Send bytes to the on-screen console.
///
/// Lines logged before the display is mapped are buffered and replayed by
/// [`on_display_ready`]; lines after that are drawn immediately. Errors are
/// swallowed by design — a console must never turn a log line into a fault.
pub fn push_bytes(bytes: &[u8]) {
    let mut console = CONSOLE.lock();
    if display::mode().is_none() {
        console.buffer_pending(bytes);
        return;
    }
    for &byte in bytes {
        console.feed(byte);
    }
}

/// Replay the pre-display log onto the screen that just arrived.
///
/// Called once, from the boot path, right after `display::init` maps the
/// framebuffer. A no-display boot never calls this, and the pending buffer
/// simply keeps absorbing (and truncating) the log.
pub fn on_display_ready() {
    let mut console = CONSOLE.lock();
    console.reset();

    let mut index = 0;
    while index < console.pending_len {
        let byte = console.pending[index];
        index += 1;
        console.feed(byte);
    }
    console.pending_len = 0;
}
