//! Scanline rasterization of glyph outlines.
//!
//! # The algorithm
//!
//! A glyph outline is a set of closed contours in font units. Rendering it
//! runs in three passes, all in fixed point:
//!
//! 1. **Scale** the outline to the requested pixel size in *q6* coordinates
//!    — 64 sub-pixel steps per pixel, the classic 26.6 convention.
//! 2. **Flatten** every quadratic Bézier into straight segments, subdividing
//!    until each piece deviates by well under a sub-pixel. The segment count
//!    is capped, so a pathological outline degrades to chords instead of
//!    running away.
//! 3. **Scan**: for every pixel row, sample the winding number at 4×4
//!    sub-pixel positions per pixel. A sample inside the glyph (non-zero
//!    winding) lights one sample; coverage is the fraction of the 16 that
//!    lit, scaled to 0..255. The anti-aliasing *is* the sampling.
//!
//! The winding walk is the part worth naming: for each sample row the
//! segments crossing it are collected and sorted by their intersection x,
//! and the sample columns then walk left→right, subtracting each crossing's
//! direction from the running winding total as it is passed. One sort per
//! row replaces one contour test per sample, which is the difference
//! between a terminal that scrolls and one that crawls.
//!
//! # Bounds and ownership
//!
//! Every glyph is capped at [`MAX_GLYPH_WIDTH`]×[`MAX_GLYPH_HEIGHT`] pixels;
//! larger requests render their top-left crop. All scratch space lives in
//! the caller's [`GlyphBuffer`] — the renderer never allocates.

use crate::parser::Outline;

/// Horizontal sub-pixel samples per pixel.
pub const SUBPIXEL_STEPS: u32 = 4;

/// Vertical sub-pixel samples per pixel.
pub const SUBPIXEL_ROWS: u32 = 4;

/// The widest a rasterized glyph may be, in pixels.
pub const MAX_GLYPH_WIDTH: u32 = 64;

/// The tallest a rasterized glyph may be, in pixels.
pub const MAX_GLYPH_HEIGHT: u32 = 64;

/// Sub-pixel steps per pixel, as a q6 divisor: one pixel = 64 q6 units.
const PIXEL_SCALE: i32 = 64;

/// The most flattened segments one glyph may produce.
const MAX_SEGMENTS: usize = 1024;

/// The most row crossings remembered at once.
const MAX_CROSSINGS: usize = 512;

/// One flattened straight segment, in q6 sub-pixel coordinates (y up).
#[derive(Clone, Copy)]
struct Segment {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

/// One quadratic Bézier piece awaiting flattening.
#[derive(Clone, Copy)]
struct Curve {
    x0: i32,
    y0: i32,
    cx: i32,
    cy: i32,
    x1: i32,
    y1: i32,
}

/// One segment's crossing of a sample row: where, and which way.
#[derive(Clone, Copy)]
struct Crossing {
    x: i32,
    /// +1 for an upward segment, −1 for a downward one.
    dir: i32,
}

/// One rasterized glyph: ink box relative to the pen, plus coverage.
///
/// `left`/`top` place the bitmap against the current pen position:
/// `left` offsets horizontally from the pen (usually ≤ 0), `top` counts
/// pixels *up* from the baseline — so bitmap row `y` draws at
/// baseline − top + y.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GlyphBitmap {
    /// Pixels the ink starts left of the pen origin.
    pub left: i32,
    /// Pixels the ink extends above the baseline.
    pub top: i32,
    /// Ink width in pixels.
    pub width: u32,
    /// Ink height in pixels.
    pub height: u32,
    /// Coverage rows, `height` rows of `width` bytes, row-major.
    /// 0 = background, 255 = solid ink.
    pub coverage: [u8; Self::MAX_BYTES],
}

impl GlyphBitmap {
    /// The scratch capacity one rasterization needs.
    pub const MAX_BYTES: usize = (MAX_GLYPH_WIDTH * MAX_GLYPH_HEIGHT) as usize;

    /// An empty bitmap: the rasterized form of "no ink here".
    pub const EMPTY: Self = Self {
        left: 0,
        top: 0,
        width: 0,
        height: 0,
        coverage: [0; Self::MAX_BYTES],
    };

    /// The number of coverage bytes the ink box actually names.
    pub fn coverage_len(&self) -> usize {
        self.width as usize * self.height as usize
    }

    /// The alpha at one pixel, or 0 outside the ink box.
    pub fn at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.coverage[y as usize * self.width as usize + x as usize]
    }
}

/// A caller-owned scratch buffer the renderer works in.
///
/// The no-alloc contract: `rasterize` never reserves memory, so every
/// consumer (kernel console, UEFI menu, ring-3 app) hands over a `static`
/// or a stack slot. The buffer keeps the segment and crossing scratch next
/// to the output bitmap, so a rasterization costs nothing but the call.
pub struct GlyphBuffer {
    bitmap: GlyphBitmap,
    segments: [Segment; MAX_SEGMENTS],
    segment_count: usize,
    crossings: [Crossing; MAX_CROSSINGS],
}

impl GlyphBuffer {
    /// An empty buffer, ready for its first rasterization.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bitmap: GlyphBitmap::EMPTY,
            segments: [Segment {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            }; MAX_SEGMENTS],
            segment_count: 0,
            crossings: [Crossing { x: 0, dir: 0 }; MAX_CROSSINGS],
        }
    }
    /// Clear the buffer back to an empty bitmap.
    pub fn clear(&mut self) {
        self.bitmap = GlyphBitmap::EMPTY;
        self.segment_count = 0;
    }

    /// The last rasterization's result, or an empty bitmap before one.
    pub fn bitmap(&self) -> &GlyphBitmap {
        &self.bitmap
    }

    /// Append one straight segment, silently dropping past the cap.
    fn emit(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if self.segment_count < MAX_SEGMENTS {
            self.segments[self.segment_count] = Segment { x0, y0, x1, y1 };
            self.segment_count += 1;
        }
    }

    /// Subdivide one quadratic Bézier into straight segments.
    ///
    /// Flatness test: the control point's deviation from the chord's
    /// midpoint, doubled to stay in exact integers. At or under half a
    /// sub-pixel the piece is drawn as its chord — far below the 4×4
    /// sampling's resolution, so no visible corner is created.
    fn flatten_quad(&mut self, curve: Curve) {
        let deviation_x = (curve.cx * 2 - curve.x0 - curve.x1).abs();
        let deviation_y = (curve.cy * 2 - curve.y0 - curve.y1).abs();
        if deviation_x.max(deviation_y) <= 16 || self.segment_count >= MAX_SEGMENTS - 2 {
            self.emit(curve.x0, curve.y0, curve.x1, curve.y1);
            return;
        }
        // De Casteljau split at t = 1/2, in exact integers.
        let ax = (curve.x0 + curve.cx) / 2;
        let ay = (curve.y0 + curve.cy) / 2;
        let bx = (curve.cx + curve.x1) / 2;
        let by = (curve.cy + curve.y1) / 2;
        let mx = (ax + bx) / 2;
        let my = (ay + by) / 2;
        self.flatten_quad(Curve {
            x0: curve.x0,
            y0: curve.y0,
            cx: ax,
            cy: ay,
            x1: mx,
            y1: my,
        });
        self.flatten_quad(Curve {
            x0: mx,
            y0: my,
            cx: bx,
            cy: by,
            x1: curve.x1,
            y1: curve.y1,
        });
    }

    /// Flatten one closed contour's points `[start..=end]` into segments.
    ///
    /// Off-curve points are quadratic controls; consecutive controls imply
    /// an on-curve joint at their midpoint, the format's own rule, which
    /// the walk below splices in explicitly.
    fn flatten_contour(
        &mut self,
        outline: &Outline,
        start: usize,
        end: usize,
        scale: impl Fn(i32) -> i32,
    ) {
        let len = end - start + 1;
        if len < 2 {
            // A one-point contour draws nothing.
            return;
        }

        // Rotate the cycle to start on an on-curve point (the format
        // guarantees at least one exists).
        let mut rotation = 0usize;
        for offset in 0..len {
            if outline.on_curve[start + offset] {
                rotation = offset;
                break;
            }
        }

        let point = |offset: usize| -> (i32, i32) {
            // `offset` runs 0..=len, rotated so 0 is on-curve; len wraps
            // back to the contour start, closing the cycle.
            let index = start + (offset + rotation) % len;
            (scale(outline.x[index]), scale(outline.y[index]))
        };
        let on_at = |offset: usize| outline.on_curve[start + (offset + rotation) % len];

        // The all-controls case: implied on-curve points sit at the
        // midpoints of consecutive controls, giving one quad per control.
        if (0..len).all(on_at) {
            for offset in 0..len {
                let (ax, ay) = point(offset);
                let (cx, cy) = point(offset + 1);
                let (ex, ey) = point(offset + 2);
                self.flatten_quad(Curve {
                    x0: (ax + cx) / 2,
                    y0: (ay + cy) / 2,
                    cx,
                    cy,
                    x1: (cx + ex) / 2,
                    y1: (cy + ey) / 2,
                });
            }
            return;
        }

        let (mut pen_x, mut pen_y) = point(0);
        let mut offset = 1usize;
        loop {
            if on_at(offset) {
                // Straight piece to the next on-curve joint (`len` wraps to
                // the start point, closing the cycle).
                let (x, y) = point(offset);
                self.emit(pen_x, pen_y, x, y);
                pen_x = x;
                pen_y = y;
                offset += 1;
            } else {
                // A run of controls: each interior pair gets its implied
                // midpoint joint; the run ends at the next on-curve point.
                let mut run_end = offset;
                while run_end < len && !on_at(run_end) {
                    run_end += 1;
                }
                let mut control = offset;
                let mut next = offset + 1;
                while next < run_end {
                    let (cx, cy) = point(control);
                    let (ex, ey) = point(next);
                    self.flatten_quad(Curve {
                        x0: pen_x,
                        y0: pen_y,
                        cx,
                        cy,
                        x1: (cx + ex) / 2,
                        y1: (cy + ey) / 2,
                    });
                    pen_x = (cx + ex) / 2;
                    pen_y = (cy + ey) / 2;
                    control = next;
                    next += 1;
                }
                let (cx, cy) = point(control);
                let (ex, ey) = point(run_end);
                self.flatten_quad(Curve {
                    x0: pen_x,
                    y0: pen_y,
                    cx,
                    cy,
                    x1: ex,
                    y1: ey,
                });
                pen_x = ex;
                pen_y = ey;
                offset = run_end + 1;
            }
            if offset > len {
                break;
            }
        }
    }

    /// Scan one pixel row band into bitmap row `row`.
    ///
    /// The band is the y-up q6 strip `[band_low, band_low + 64)`; bitmap
    /// row 0 is the topmost ink row, so band 0 sits highest.
    fn scan_band(&mut self, band_low: i32, row: u32, left: i32, width: u32) {
        let mut hits = [0u32; MAX_GLYPH_WIDTH as usize];
        let sample_step = PIXEL_SCALE / SUBPIXEL_STEPS as i32;

        for sample_row in 0..SUBPIXEL_ROWS as i32 {
            let y = band_low + sample_row * sample_step + sample_step / 2;

            // Collect this sample row's crossings.
            let mut count = 0usize;
            for segment in &self.segments[..self.segment_count] {
                let dy = segment.y1 - segment.y0;
                if dy == 0 {
                    continue; // Horizontal: crosses no row.
                }
                let (low, high) = if dy > 0 {
                    (segment.y0, segment.y1)
                } else {
                    (segment.y1, segment.y0)
                };
                // Half-open span so shared vertices count exactly once.
                if y < low || y >= high {
                    continue;
                }
                if count >= self.crossings.len() {
                    break; // Degenerate crowd: drop the remainder.
                }
                let dx = segment.x1 - segment.x0;
                // Intersection x, in i64 so the product cannot overflow.
                let x = i64::from(segment.x0)
                    + i64::from(y - segment.y0) * i64::from(dx) / i64::from(dy);
                self.crossings[count] = Crossing {
                    x: x as i32,
                    dir: if dy > 0 { 1 } else { -1 },
                };
                count += 1;
            }
            if count == 0 {
                continue;
            }

            // Sort crossings left→right (insertion sort: `count` is small).
            for i in 1..count {
                let key = self.crossings[i];
                let mut slot = i;
                while slot > 0 && self.crossings[slot - 1].x > key.x {
                    self.crossings[slot] = self.crossings[slot - 1];
                    slot -= 1;
                }
                self.crossings[slot] = key;
            }

            // Walk the sample columns left→right, consuming crossings as
            // they pass: the winding at a sample is the sum of the signed
            // crossings still to its right.
            let total: i32 = self.crossings[..count].iter().map(|c| c.dir).sum();
            let mut winding = total;
            let mut index = 0usize;
            for (pixel, hit) in hits.iter_mut().enumerate().take(width as usize) {
                let base = i64::from(left + pixel as i32) * i64::from(PIXEL_SCALE);
                for column in 0..SUBPIXEL_STEPS as i32 {
                    let x = base + i64::from(column * sample_step + sample_step / 2);
                    while index < count && i64::from(self.crossings[index].x) <= x {
                        winding -= self.crossings[index].dir;
                        index += 1;
                    }
                    if winding != 0 {
                        *hit += 1;
                    }
                }
            }
        }

        // 16 samples → one coverage byte, scaled to 0..=255.
        let samples = SUBPIXEL_ROWS * SUBPIXEL_STEPS;
        let base = row as usize * width as usize;
        for pixel in 0..width as usize {
            let value = (hits[pixel] * 255 + samples / 2) / samples;
            self.bitmap.coverage[base + pixel] = value.min(255) as u8;
        }
    }
}

impl Default for GlyphBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Rasterize one outline (font units) at `size_px` into `buffer`.
///
/// After the call, [`GlyphBuffer::bitmap`] names the glyph's ink: an empty
/// bitmap means the glyph draws nothing (a space). The caller moves the pen
/// by the advance width the font reports separately — the bitmap only ever
/// describes ink.
pub fn rasterize(outline: &Outline, units_per_em: u16, size_px: u32, buffer: &mut GlyphBuffer) {
    buffer.clear();
    if outline.count == 0 || size_px == 0 || units_per_em == 0 {
        return;
    }

    // Font units → q6 sub-pixels, rounding to nearest.
    let scale = |units: i32| -> i32 {
        ((i64::from(units) * i64::from(size_px) * i64::from(PIXEL_SCALE)
            + i64::from(units_per_em) / 2)
            / i64::from(units_per_em)) as i32
    };

    for contour in 0..outline.contours {
        let end = outline.contour_ends[contour] as usize;
        if end >= outline.count {
            continue; // Malformed contour end: skip, never read out of bounds.
        }
        let start = if contour == 0 {
            0
        } else {
            outline.contour_ends[contour - 1] as usize + 1
        };
        if start > end {
            continue;
        }
        buffer.flatten_contour(outline, start, end, scale);
    }

    if buffer.segment_count == 0 {
        return; // No ink: a space, or an outline too dense to hold.
    }

    // The ink box, in q6 sub-pixels (y up).
    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for segment in &buffer.segments[..buffer.segment_count] {
        min_x = min_x.min(segment.x0.min(segment.x1));
        max_x = max_x.max(segment.x0.max(segment.x1));
        min_y = min_y.min(segment.y0.min(segment.y1));
        max_y = max_y.max(segment.y0.max(segment.y1));
    }

    // q6 sub-pixels → pixel indices, flooring so partial coverage lands
    // inside the box. y up: the largest y is the topmost pixel row.
    let left = div_floor(min_x, PIXEL_SCALE);
    let right = div_floor(max_x, PIXEL_SCALE);
    let top_px = div_floor(max_y, PIXEL_SCALE);
    let bottom_px = div_floor(min_y, PIXEL_SCALE);

    let width = (right - left + 1).clamp(0, MAX_GLYPH_WIDTH as i32) as u32;
    let height = (top_px - bottom_px + 1).clamp(0, MAX_GLYPH_HEIGHT as i32) as u32;
    if width == 0 || height == 0 {
        return;
    }

    buffer.bitmap.left = left;
    buffer.bitmap.top = top_px;
    buffer.bitmap.width = width;
    buffer.bitmap.height = height;

    for row in 0..height {
        let band_low = (top_px - row as i32) * PIXEL_SCALE;
        buffer.scan_band(band_low, row, left, width);
    }
}

/// Floor division, rounding toward −∞ (sub-pixel → pixel mapping).
fn div_floor(value: i32, divisor: i32) -> i32 {
    let quotient = value / divisor;
    if value % divisor != 0 && value < 0 {
        quotient - 1
    } else {
        quotient
    }
}
