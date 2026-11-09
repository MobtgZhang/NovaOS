//! SFNT/TrueType table parsing.
//!
//! # What is parsed
//!
//! Exactly the tables a renderer needs and nothing more: `head` (the
//! em-square and the `loca` encoding), `maxp` (the glyph count), `cmap`
//! (codepoint → glyph, formats 4 and 12), `hhea`/`hmtx` (line metrics and
//! advances), and `glyf`/`loca` (the outlines, simple and composite). Every
//! table lookup is bounds-checked against the font blob's real length: a
//! truncated or hostile font file produces an error, never a panic, because
//! fonts arrive from disks and firmware the kernel does not trust.
//!
//! # What is not parsed
//!
//! `CFF` outlines (the OpenType flavour — the fonts NovaOS ships are
//! TrueType-flavoured), hinting programs, and OpenType layout features. The
//! error [`FontError::UnsupportedOutline`] names the first one explicitly so
//! a wrong-flavour font is diagnosed in one log line instead of as a
//! mystery of empty glyphs. A `ttc` collection is not a single sfnt and is
//! rejected the same way; ship the face, not the collection.

/// Everything that can go wrong, named precisely enough to log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FontError {
    /// The blob is shorter than any table it claims to have, or a record
    /// points outside it.
    Truncated,
    /// The SFNT header or a required table's magic does not match.
    BadHeader,
    /// A required table is missing.
    MissingTable,
    /// The `glyf` outlines are CFF-flavoured, which this parser does not do.
    UnsupportedOutline,
    /// The `cmap` has no Unicode subtable this parser understands.
    NoUnicodeCmap,
    /// The metrics tables are inconsistent (`numGlyphs` zero, no advances).
    BadMetrics,
}

/// A glyph index inside the font, as the `cmap` and `glyf` spell it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GlyphId(pub u16);

/// One parsed outline in font units, ready for [`raster`](crate::raster).
///
/// Contours are given as *last point indices*: contour `i` is the closed
/// cycle `points[last[i-1]+1..=last[i]]`, the convention `glyf` itself
/// uses. Points sit on the curve or off it (quadratic controls); the
/// rasterizer owns the business of mid-point synthesis for consecutive
/// off-curve points.
pub struct Outline {
    /// Outline points in font units.
    pub x: [i32; MAX_OUTLINE_POINTS],
    pub y: [i32; MAX_OUTLINE_POINTS],
    /// Whether each point sits on the curve (`false` = quadratic control).
    pub on_curve: [bool; MAX_OUTLINE_POINTS],
    /// How many points are populated.
    pub count: usize,
    /// The last point index of each contour.
    pub contour_ends: [u16; MAX_CONTOURS],
    /// How many contours are populated.
    pub contours: usize,
    /// Decode scratch: per-point flags, carried between the flag and the
    /// coordinate passes of one `simple_outline` call.
    ///
    /// The scratch lives *here* rather than on the parser's stack on
    /// purpose: the arrays total ~4.6 KiB, and the kernel renders glyphs
    /// from deep call chains (a log line inside a syscall path) on 16 KiB
    /// kernel stacks. Stack scratch there overflowed the stack into
    /// whatever frame lay below it — a user page, once a page table.
    pub flags: [u8; MAX_OUTLINE_POINTS],
    /// Decode scratch: the x pass's accumulated coordinates.
    pub xs: [i32; MAX_OUTLINE_POINTS],
    /// Decode scratch: the y pass's accumulated coordinates.
    pub ys: [i32; MAX_OUTLINE_POINTS],
    /// Decode scratch: the contour-end list, re-based at commit time.
    pub ends: [u16; MAX_CONTOURS],
}

impl Outline {
    /// An empty outline, safe to hand to the rasterizer.
    pub const fn empty() -> Self {
        Self {
            x: [0; MAX_OUTLINE_POINTS],
            y: [0; MAX_OUTLINE_POINTS],
            on_curve: [false; MAX_OUTLINE_POINTS],
            count: 0,
            contour_ends: [0; MAX_CONTOURS],
            contours: 0,
            flags: [0; MAX_OUTLINE_POINTS],
            xs: [0; MAX_OUTLINE_POINTS],
            ys: [0; MAX_OUTLINE_POINTS],
            ends: [0; MAX_CONTOURS],
        }
    }
}

/// The most outline points one glyph may have.
///
/// A dense CJK glyph is a few hundred points; composites add their
/// components'. A glyph past this bound renders as nothing rather than
/// overflowing — bounded, so a hostile font cannot ask for unbounded work.
pub const MAX_OUTLINE_POINTS: usize = 512;

/// The most contours one glyph may have.
pub const MAX_CONTOURS: usize = 16;

/// A parsed, borrowed TrueType font.
///
/// The font's bytes are *not* copied: every field is either a scalar read
/// from `data` at construction or a byte range resolved then, and all later
/// lookups re-read `data` through those ranges. A font the kernel maps
/// read-only is therefore rendered in place, with zero heap traffic.
pub struct Font<'a> {
    data: &'a [u8],
    units_per_em: u16,
    num_glyphs: u16,
    /// `loca` entry width: `false` = u16 units of two, `true` = u32 bytes.
    long_loca: bool,
    loca: Table,
    glyf: Table,
    cmap: Table,
    hmtx_start: usize,
    num_h_metrics: u16,
    /// `hhea` ascender, in font units (positive = above baseline).
    ascender: i16,
    /// `hhea` descender, in font units (negative = below baseline).
    descender: i16,
    line_gap: i16,
}

/// A half-open byte range inside the font blob, kept absolute.
#[derive(Clone, Copy)]
struct Table {
    start: usize,
    len: usize,
}

impl Table {
    const EMPTY: Self = Self { start: 0, len: 0 };

    /// The table's bytes, or an empty slice when absent or out of bounds.
    fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        data.get(self.start..self.start.saturating_add(self.len))
            .unwrap_or(&[])
    }

    /// The table's bytes from `offset` on, for sub-table walks.
    fn from<'a>(&self, data: &'a [u8], offset: usize) -> &'a [u8] {
        let start = self.start.saturating_add(offset);
        data.get(start..self.start.saturating_add(self.len))
            .unwrap_or(&[])
    }
}

impl<'a> Font<'a> {
    /// Parse the font's directory and cache the table locations.
    ///
    /// No allocation, no copying: the work is a table-directory walk plus a
    /// handful of scalar reads, all bounds-checked against `data`.
    pub fn parse(data: &'a [u8]) -> Result<Self, FontError> {
        // A single sfnt: version, numTables, then 16-byte records.
        if data.len() < 12 {
            return Err(FontError::Truncated);
        }
        let num_tables = read_u16(data, 4).ok_or(FontError::Truncated)? as usize;
        let directory_end = 12usize.saturating_add(num_tables.saturating_mul(16));
        if directory_end > data.len() {
            return Err(FontError::Truncated);
        }

        let mut head = Table::EMPTY;
        let mut maxp = Table::EMPTY;
        let mut cmap = Table::EMPTY;
        let mut hhea = Table::EMPTY;
        let mut hmtx = Table::EMPTY;
        let mut loca = Table::EMPTY;
        let mut glyf = Table::EMPTY;
        let mut have_head = false;
        let mut have_maxp = false;
        let mut have_cmap = false;
        let mut have_hhea = false;
        let mut have_hmtx = false;
        let mut have_loca = false;
        let mut have_glyf = false;

        for record in 0..num_tables {
            let base = 12 + record * 16;
            let mut tag = [0u8; 4];
            tag.copy_from_slice(data.get(base..base + 4).ok_or(FontError::Truncated)?);
            let offset = read_u32(data, base + 8).ok_or(FontError::Truncated)? as usize;
            let length = read_u32(data, base + 12).ok_or(FontError::Truncated)? as usize;
            // The record must name bytes the blob actually holds.
            if offset.saturating_add(length) > data.len() {
                return Err(FontError::Truncated);
            }
            let table = Table {
                start: offset,
                len: length,
            };
            match &tag {
                b"head" => {
                    head = table;
                    have_head = true;
                }
                b"maxp" => {
                    maxp = table;
                    have_maxp = true;
                }
                b"cmap" => {
                    cmap = table;
                    have_cmap = true;
                }
                b"hhea" => {
                    hhea = table;
                    have_hhea = true;
                }
                b"hmtx" => {
                    hmtx = table;
                    have_hmtx = true;
                }
                b"loca" => {
                    loca = table;
                    have_loca = true;
                }
                b"glyf" => {
                    glyf = table;
                    have_glyf = true;
                }
                _ => {}
            }
        }

        // A `glyf`-less font is CFF-flavoured: the outlines this renderer
        // draws do not exist in it, and saying so beats empty boxes.
        if !(have_head && have_maxp && have_cmap && have_hhea && have_hmtx && have_loca) {
            return Err(if have_glyf {
                FontError::Truncated
            } else {
                FontError::UnsupportedOutline
            });
        }

        let units_per_em = read_u16(head.bytes(data), 18).ok_or(FontError::BadHeader)?;
        if units_per_em == 0 {
            return Err(FontError::BadMetrics);
        }
        let long_loca = read_u16(head.bytes(data), 50).ok_or(FontError::BadHeader)? != 0;

        let num_glyphs = read_u16(maxp.bytes(data), 4).ok_or(FontError::BadHeader)?;
        if num_glyphs == 0 {
            return Err(FontError::BadMetrics);
        }

        let hhea_bytes = hhea.bytes(data);
        if hhea_bytes.len() < 36 {
            return Err(FontError::Truncated);
        }
        let ascender = read_i16(hhea_bytes, 4).ok_or(FontError::Truncated)?;
        let descender = read_i16(hhea_bytes, 6).ok_or(FontError::Truncated)?;
        let line_gap = read_i16(hhea_bytes, 8).ok_or(FontError::Truncated)?;
        let num_h_metrics = read_u16(hhea_bytes, 34).ok_or(FontError::Truncated)?;
        if num_h_metrics == 0 {
            return Err(FontError::BadMetrics);
        }

        Ok(Self {
            data,
            units_per_em,
            num_glyphs,
            long_loca,
            loca,
            glyf,
            cmap,
            hmtx_start: hmtx.start,
            num_h_metrics,
            ascender,
            descender,
            line_gap,
        })
    }

    /// The em-square the outlines are drawn in (typically 1000 or 2048).
    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// `(ascent, descent, line gap)` scaled to `size_px`, in pixels.
    ///
    /// Ascent is positive above the baseline; descent is returned positive
    /// too (it is a *depth*), so a line's total height is the sum of the
    /// three.
    pub fn line_metrics(&self, size_px: u32) -> LineMetrics {
        let scale = |units: i32| {
            (i64::from(units) * i64::from(size_px) + i64::from(self.units_per_em) / 2)
                / i64::from(self.units_per_em)
        };
        LineMetrics {
            ascent: scale(i32::from(self.ascender)).max(0) as u32,
            descent: scale(-i32::from(self.descender)).max(0) as u32,
            line_gap: scale(i32::from(self.line_gap)).max(0) as u32,
        }
    }

    /// The advance width of one glyph at `size_px`, in whole pixels.
    ///
    /// Rounded to nearest; a monospaced font returns the same value for
    /// every glyph, which is what makes a terminal column arithmetic
    /// trivial.
    pub fn advance(&self, glyph: GlyphId, size_px: u32) -> u32 {
        let index = glyph.0 as usize;
        // The metrics array covers `num_h_metrics` glyphs; every glyph past
        // it shares the last entry's advance (the format's own rule).
        let entry = index.min(self.num_h_metrics as usize - 1);
        let raw = read_u16(self.data, self.hmtx_start + entry * 4).unwrap_or(self.units_per_em);
        ((i64::from(raw) * i64::from(size_px) + i64::from(self.units_per_em) / 2)
            / i64::from(self.units_per_em)) as u32
    }

    /// The glyph that draws `ch`, or `None` when the font has no mapping.
    pub fn glyph_for_char(&self, ch: char) -> Option<GlyphId> {
        self.cmap_lookup(ch as u32)
    }

    /// Walk the `cmap` encoding records and look `codepoint` up in the
    /// first subtable that understands it.
    fn cmap_lookup(&self, codepoint: u32) -> Option<GlyphId> {
        let count = usize::from(read_u16(self.cmap.bytes(self.data), 2)?);
        for record in 0..count {
            let base = 4 + record * 8;
            let platform = read_u16(self.cmap.bytes(self.data), base)?;
            let encoding = read_u16(self.cmap.bytes(self.data), base + 2)?;
            let offset = read_u32(self.cmap.bytes(self.data), base + 4)? as usize;
            // (3,1) Windows BMP, (3,10) Windows full repertoire, and the
            // platform-0 Unicode records all name codepoints the same way.
            let unicode = (platform == 3 && (encoding == 1 || encoding == 10)) || platform == 0;
            if !unicode {
                continue;
            }
            let subtable = self.cmap.from(self.data, offset);
            let format = read_u16(subtable, 0)?;
            let glyph = match format {
                4 => Self::cmap4(subtable, codepoint)?,
                12 => Self::cmap12(subtable, codepoint)?,
                _ => continue,
            };
            // A mapping to glyph 0 is the font saying "I draw nothing
            // here"; a real miss must stay a miss so callers can fall back.
            if glyph.0 == 0 {
                return None;
            }
            return Some(glyph);
        }
        None
    }

    /// `cmap` format 4: segmented mapping for the BMP.
    fn cmap4(subtable: &[u8], codepoint: u32) -> Option<GlyphId> {
        if codepoint > 0xFFFF {
            return None;
        }
        let seg_count = usize::from(read_u16(subtable, 6)? / 2);
        let end_base = 14usize;
        let start_base = end_base + seg_count * 2 + 2;
        let delta_base = start_base + seg_count * 2;
        let range_base = delta_base + seg_count * 2;

        let needle = codepoint as u16;
        for segment in 0..seg_count {
            let end = read_u16(subtable, end_base + segment * 2)?;
            if needle > end {
                continue;
            }
            let start = read_u16(subtable, start_base + segment * 2)?;
            if needle < start {
                return None;
            }
            let range_offset = read_u16(subtable, range_base + segment * 2)?;
            let glyph = if range_offset == 0 {
                // The id-delta arithmetic, mod 2^16 as the format demands.
                let delta = read_u16(subtable, delta_base + segment * 2)?;
                needle.wrapping_add(delta)
            } else {
                let address = range_base
                    + segment * 2
                    + usize::from(range_offset)
                    + (usize::from(needle) - usize::from(start)) * 2;
                read_u16(subtable, address)?
            };
            return Some(GlyphId(glyph));
        }
        None
    }

    /// `cmap` format 12: segmented mapping for the full repertoire.
    fn cmap12(subtable: &[u8], codepoint: u32) -> Option<GlyphId> {
        let groups = read_u32(subtable, 12)? as usize;
        // Binary search over the sorted group list.
        let mut low = 0usize;
        let mut high = groups;
        while low < high {
            let mid = (low + high) / 2;
            let base = 16 + mid * 12;
            let start = read_u32(subtable, base)?;
            let end = read_u32(subtable, base + 4)?;
            if codepoint < start {
                high = mid;
            } else if codepoint > end {
                low = mid + 1;
            } else {
                let first_glyph = read_u32(subtable, base + 8)?;
                return Some(GlyphId((first_glyph + codepoint - start) as u16));
            }
        }
        None
    }

    /// Assemble one glyph's outline in font units.
    ///
    /// Composite glyphs are followed into their components, each
    /// transformed by its own offset and 2×2, up to a small depth bound —
    /// enough for the accented-Latin nesting real fonts use.
    pub fn outline(&self, glyph: GlyphId, out: &mut Outline) -> Result<(), FontError> {
        out.count = 0;
        out.contours = 0;
        let index = usize::from(glyph.0);
        if index >= usize::from(self.num_glyphs) {
            return Err(FontError::BadHeader);
        }
        let (start, end) = self.loca_range(index)?;
        if start == end {
            // An empty glyph (usually space) is a valid outline of nothing.
            return Ok(());
        }
        let body = self.glyf.bytes(self.data).get(start..end).unwrap_or(&[]);
        Self::glyph_outline(self, body, Transform::IDENTITY, 0, out)
    }

    /// The `loca` byte range of glyph `index` inside `glyf`.
    fn loca_range(&self, index: usize) -> Result<(usize, usize), FontError> {
        let entry_size = if self.long_loca { 4 } else { 2 };
        let base = self
            .loca
            .start
            .saturating_add(index.saturating_mul(entry_size));
        let read = |at: usize| -> Option<usize> {
            if self.long_loca {
                read_u32(self.data, at).map(|v| v as usize)
            } else {
                read_u16(self.data, at).map(|v| usize::from(v) * 2)
            }
        };
        let start = read(base).ok_or(FontError::Truncated)?;
        let end = read(base.saturating_add(entry_size)).ok_or(FontError::Truncated)?;
        if start > end {
            return Err(FontError::Truncated);
        }
        Ok((start, end))
    }

    /// Parse one (possibly component) glyph body into `out`, applying the
    /// affine placement the composite named.
    ///
    /// # Placement layout
    ///
    /// The matrix maps a font-unit point to its placed position:
    /// `(a·x + c·y + dx, b·x + d·y + dy)`, with the scale words in F2Dot14
    /// widened to 16.16 and the offsets in font units.
    fn glyph_outline(
        font: &Self,
        slice: &[u8],
        placed: Transform,
        depth: usize,
        out: &mut Outline,
    ) -> Result<(), FontError> {
        if depth > 4 {
            // Nesting past four deep is decoration the renderer skips.
            return Ok(());
        }
        let num_contours = read_i16(slice, 0).ok_or(FontError::Truncated)?;
        if num_contours >= 0 {
            Self::simple_outline(slice, usize::from(num_contours as u16), placed, out)
        } else {
            Self::composite_outline(font, slice, placed, depth, out)
        }
    }

    /// Assemble a simple (non-composite) glyph's points, appending to
    /// whatever a composite has already accumulated in `out`.
    fn simple_outline(
        slice: &[u8],
        num_contours: usize,
        placed: Transform,
        out: &mut Outline,
    ) -> Result<(), FontError> {
        if num_contours == 0 || num_contours > MAX_CONTOURS {
            return Ok(());
        }
        const ENDS_BASE: usize = 10;

        // Contour ends first: they name the byte layout that follows.
        let mut last_end = 0usize;
        for (contour, slot) in out.ends.iter_mut().enumerate().take(num_contours) {
            let end =
                read_u16(slice, ENDS_BASE + contour * 2).ok_or(FontError::Truncated)? as usize;
            if end < last_end {
                return Err(FontError::Truncated);
            }
            last_end = end;
            *slot = end as u16;
        }

        let point_count = last_end + 1;
        if point_count > MAX_OUTLINE_POINTS {
            return Ok(()); // Too dense to hold: render nothing rather than lie.
        }

        // Instructions follow the contour-end list; they are skipped, but
        // their length must be honoured to find the point data.
        let instructions =
            read_u16(slice, ENDS_BASE + num_contours * 2).ok_or(FontError::Truncated)? as usize;
        let flags_base = ENDS_BASE + num_contours * 2 + 2 + instructions;

        // Flags: one byte per point, with a repeat-count compression the
        // format defines. Decode into the outline's scratch first.
        let mut produced = 0usize;
        let mut cursor = flags_base;
        while produced < point_count {
            let flag = *slice.get(cursor).ok_or(FontError::Truncated)?;
            cursor += 1;
            out.flags[produced] = flag;
            produced += 1;
            if flag & 0x08 != 0 {
                // Repeat: the next byte counts how many more get this flag.
                let repeat = *slice.get(cursor).ok_or(FontError::Truncated)?;
                cursor += 1;
                for _ in 0..repeat {
                    if produced < point_count {
                        out.flags[produced] = flag;
                        produced += 1;
                    }
                }
            }
        }

        // The deltas are the clever part of `glyf`: each coordinate is
        // either 0 (same as the previous point), one *unsigned* byte whose
        // sign the flag carries (bit set = positive), or two bytes signed —
        // chosen per point by flag bits 1/4 and 2/5.
        let mut x = 0i32;
        for point in 0..point_count {
            let flag = out.flags[point];
            x += match (flag & 0x02, flag & 0x10) {
                (0x02, 0x10) => {
                    let byte = *slice.get(cursor).ok_or(FontError::Truncated)?;
                    cursor += 1;
                    i32::from(byte) // short and positive
                }
                (0x02, 0) => {
                    let byte = *slice.get(cursor).ok_or(FontError::Truncated)?;
                    cursor += 1;
                    -i32::from(byte) // short and negative: the byte is a magnitude
                }
                (0, 0x10) => 0, // same as previous; no bytes at all
                _ => {
                    cursor += 2;
                    i32::from(read_i16(slice, cursor - 2).ok_or(FontError::Truncated)?)
                }
            };
            out.xs[point] = x;
        }
        let mut y = 0i32;
        for point in 0..point_count {
            let flag = out.flags[point];
            y += match (flag & 0x04, flag & 0x20) {
                (0x04, 0x20) => {
                    let byte = *slice.get(cursor).ok_or(FontError::Truncated)?;
                    cursor += 1;
                    i32::from(byte)
                }
                (0x04, 0) => {
                    let byte = *slice.get(cursor).ok_or(FontError::Truncated)?;
                    cursor += 1;
                    -i32::from(byte)
                }
                (0, 0x20) => 0,
                _ => {
                    cursor += 2;
                    i32::from(read_i16(slice, cursor - 2).ok_or(FontError::Truncated)?)
                }
            };
            out.ys[point] = y;
        }

        // Commit: the component's points append after whatever the
        // composite has accumulated so far, contour ends re-based to match.
        if out.count + point_count > MAX_OUTLINE_POINTS
            || out.contours + num_contours > MAX_CONTOURS
        {
            return Ok(()); // A composite this rich loses a component, bounded.
        }
        let base = out.count;
        for point in 0..point_count {
            let (x, y) = placed.place(out.xs[point], out.ys[point]);
            out.x[base + point] = x;
            out.y[base + point] = y;
            out.on_curve[base + point] = out.flags[point] & 0x01 != 0;
        }
        for contour in 0..num_contours {
            out.contour_ends[out.contours + contour] = base as u16 + out.ends[contour];
        }
        out.count = base + point_count;
        out.contours += num_contours;
        Ok(())
    }

    /// Follow a composite glyph into its components.
    fn composite_outline(
        font: &Self,
        slice: &[u8],
        placed: Transform,
        depth: usize,
        out: &mut Outline,
    ) -> Result<(), FontError> {
        // After the contour count (negative for composites) sit the
        // components: u16 flags, u16 glyph id, args, then an optional
        // transform. The walk ends at MORE_COMPONENTS clear.
        let mut cursor = 10usize;
        loop {
            let flags = read_u16(slice, cursor).ok_or(FontError::Truncated)?;
            let component = read_u16(slice, cursor + 2).ok_or(FontError::Truncated)?;
            cursor += 4;

            let arg_words = flags & 0x01 != 0;
            let arg_size = if arg_words { 2 } else { 1 };
            let mut arg1 = read_signed(slice, cursor, arg_words)?;
            let mut arg2 = read_signed(slice, cursor + arg_size, arg_words)?;
            cursor += arg_size * 2;

            // When the args are *point indices* rather than offsets (the
            // format's other composite mode), there is no placement offset
            // to apply — the components are meant to share a contour point,
            // which a scanline renderer simply treats as "no offset".
            if flags & 0x02 == 0 {
                arg1 = 0;
                arg2 = 0;
            }

            let mut inner = Transform::IDENTITY;
            if flags & 0x08 != 0 {
                // Scale: one F2Dot14 word for both axes.
                let scale = read_f2dot14(slice, cursor).ok_or(FontError::Truncated)?;
                cursor += 2;
                inner = Transform {
                    a: scale,
                    d: scale,
                    ..Transform::IDENTITY
                };
            } else if flags & 0x40 != 0 {
                // X and Y scale: two F2Dot14 words.
                let sx = read_f2dot14(slice, cursor).ok_or(FontError::Truncated)?;
                let sy = read_f2dot14(slice, cursor + 2).ok_or(FontError::Truncated)?;
                cursor += 4;
                inner = Transform {
                    a: sx,
                    d: sy,
                    ..Transform::IDENTITY
                };
            } else if flags & 0x80 != 0 {
                // Full 2×2 in F2Dot14.
                let a = read_f2dot14(slice, cursor).ok_or(FontError::Truncated)?;
                let b = read_f2dot14(slice, cursor + 2).ok_or(FontError::Truncated)?;
                let c = read_f2dot14(slice, cursor + 4).ok_or(FontError::Truncated)?;
                let d = read_f2dot14(slice, cursor + 6).ok_or(FontError::Truncated)?;
                cursor += 8;
                inner = Transform {
                    dx: 0,
                    dy: 0,
                    a,
                    b,
                    c,
                    d,
                };
            }

            // The component's placement is the parent transform applied to
            // the component's own offset, and the parent's matrix applies
            // to the whole component result.
            let component_placed = placed.compose(&inner, arg1, arg2);

            let (start, end) = font.loca_range(usize::from(component))?;
            if start != end {
                let body = font.glyf.bytes(font.data).get(start..end).unwrap_or(&[]);
                Self::glyph_outline(font, body, component_placed, depth + 1, out)?;
            }

            if flags & 0x20 == 0 {
                break;
            }
        }
        Ok(())
    }
}

/// 16.16's rendering of "identity" — the scale a component without one gets.
const UNIT_SCALE: i32 = 1 << 16;

/// The affine placement a composite applies to one of its components.
///
/// Words are 16.16; the parent's matrix maps the component's font-unit
/// points into the composite's own font-unit space, where the next level's
/// placement applies in turn.
#[derive(Clone, Copy)]
struct Transform {
    dx: i32,
    dy: i32,
    a: i32,
    b: i32,
    c: i32,
    d: i32,
}

impl Transform {
    /// Translation only: the placement a top-level glyph gets.
    const IDENTITY: Self = Self {
        dx: 0,
        dy: 0,
        a: UNIT_SCALE,
        b: 0,
        c: 0,
        d: UNIT_SCALE,
    };

    /// Map one font-unit point through this placement.
    fn place(&self, x: i32, y: i32) -> (i32, i32) {
        (
            self.dx + mul1616(self.a, x) + mul1616(self.c, y),
            self.dy + mul1616(self.b, x) + mul1616(self.d, y),
        )
    }

    /// Compose the parent transform with a component's own matrix and
    /// offset (`ox`, `oy` in font units), yielding the component's
    /// placement in the parent's space.
    fn compose(&self, inner: &Transform, ox: i32, oy: i32) -> Self {
        let (dx, dy) = self.place(ox, oy);
        Self {
            dx,
            dy,
            a: mul1616(self.a, inner.a) + mul1616(self.c, inner.b),
            b: mul1616(self.b, inner.a) + mul1616(self.d, inner.b),
            c: mul1616(self.a, inner.c) + mul1616(self.c, inner.d),
            d: mul1616(self.b, inner.c) + mul1616(self.d, inner.d),
        }
    }
}

/// A signed quantity in `word` (i16) or `byte` (i8) form.
fn read_signed(slice: &[u8], at: usize, word: bool) -> Result<i32, FontError> {
    if word {
        Ok(i32::from(read_i16(slice, at).ok_or(FontError::Truncated)?))
    } else {
        Ok(i32::from(*slice.get(at).ok_or(FontError::Truncated)? as i8))
    }
}

/// A composite's scale word: F2Dot14, widened to 16.16.
fn read_f2dot14(slice: &[u8], at: usize) -> Option<i32> {
    // 14 fractional bits widening to 16 is a shift, not a reinterpretation.
    read_i16(slice, at).map(|raw| i32::from(raw) << 2)
}

/// Multiply a 16.16 scale by a font-unit coordinate.
fn mul1616(scale: i32, value: i32) -> i32 {
    // The shift's precedence against the multiply is intentional and shown:
    // (scale × value) as a whole is shifted down by 16 fractional bits.
    ((i64::from(scale) * i64::from(value)) >> 16) as i32
}

/// Line metrics scaled to a pixel size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMetrics {
    /// Pixels above the baseline a line occupies.
    pub ascent: u32,
    /// Pixels below the baseline a line occupies.
    pub descent: u32,
    /// Pixels of breathing room between lines.
    pub line_gap: u32,
}

impl LineMetrics {
    /// The full height of one line of text.
    pub fn height(&self) -> u32 {
        self.ascent + self.descent + self.line_gap
    }
}

// --- small bounds-checked readers -------------------------------------------

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_i16(data: &[u8], at: usize) -> Option<i16> {
    read_u16(data, at).map(|v| v as i16)
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
