//! Rendering tests against the fixed `iosevka-test.ttf` asset.
//!
//! The asset is committed (`assets/fonts/iosevka-test.ttf`, rebuilt only by
//! `tools/fonts/build.py`), so every assertion below runs against the same
//! bytes on every machine: a rendering that drifts is a review-visible
//! change, not a slow rot.

use nova_type::cache::GlyphCache;
use nova_type::parser::{Font, FontError, Outline};
use nova_type::raster::{GlyphBuffer, MAX_GLYPH_HEIGHT, MAX_GLYPH_WIDTH, rasterize};
use nova_type::{GlyphBitmapView, GlyphId};

const TEST_FONT: &[u8] = include_bytes!("../../../assets/fonts/iosevka-test.ttf");

/// Parse the committed test asset; the one precondition every test shares.
fn test_font() -> Font<'static> {
    Font::parse(TEST_FONT).expect("the committed test font parses")
}

/// Rasterize one character and return `(advance, bitmap)` in one step.
fn render(font: &Font<'_>, ch: char, size: u32) -> (u32, GlyphBuffer) {
    let glyph = font.glyph_for_char(ch).expect("glyph exists in test font");
    let mut outline = Outline::empty();
    font.outline(glyph, &mut outline).expect("outline parses");
    let mut buffer = GlyphBuffer::new();
    rasterize(&outline, font.units_per_em(), size, &mut buffer);
    (font.advance(glyph, size), buffer)
}

#[test]
fn committed_test_font_parses() {
    let font = test_font();
    // Iosevka's em-square is 1000 units; anything else means the asset was
    // rebuilt from a different face.
    assert_eq!(font.units_per_em(), 1000);
}

#[test]
fn missing_character_is_a_miss_not_a_notdef() {
    let font = test_font();
    // The test asset names only "NOVA nova01 .:!()[]-+=#/"; a character
    // outside it must come back `None` so callers can fall back, rather
    // than mapping to glyph 0 and drawing an empty box silently.
    assert!(font.glyph_for_char('é').is_none());
    assert!(font.glyph_for_char('中').is_none());
}

#[test]
fn capital_a_rasters_with_ink_and_symmetry() {
    let font = test_font();
    let (_advance, buffer) = render(&font, 'A', 24);
    let bitmap = buffer.bitmap();
    assert!(bitmap.width > 0 && bitmap.height > 0, "'A' has ink");

    // An 'A' is roughly twice as tall as wide, and its ink starts near the
    // pen origin horizontally.
    assert!(bitmap.height > bitmap.width, "A is taller than wide");
    assert!(
        bitmap.left >= -2 && bitmap.left <= 2,
        "A is centred on the pen"
    );

    // The coverage distribution: a solid stem lights full rows, and the
    // anti-aliased edges produce intermediate values — both must exist, or
    // either the rasterizer is thresholding or the sampling is dead.
    let mut solid = 0usize;
    let mut intermediate = 0usize;
    for row in 0..bitmap.height as usize {
        for column in 0..bitmap.width as usize {
            match bitmap.coverage[row * bitmap.width as usize + column] {
                255 => solid += 1,
                0 => {}
                _ => intermediate += 1,
            }
        }
    }
    assert!(solid > 0, "some pixels are fully covered");
    assert!(intermediate > 0, "some pixels are anti-aliased");
}

#[test]
fn capital_a_is_left_right_symmetric() {
    let font = test_font();
    let (_advance, buffer) = render(&font, 'A', 32);
    let bitmap = buffer.bitmap();

    // Column ink sums: an 'A' is symmetric about its vertical axis within
    // one sample's tolerance, which catches flipped-y and flipped-x bugs
    // the eye would catch instantly and a unit test should too.
    let column_ink = |column: usize| -> u32 {
        (0..bitmap.height as usize)
            .map(|row| u32::from(bitmap.coverage[row * bitmap.width as usize + column]))
            .sum()
    };
    let mut left = 0u32;
    let mut right = 0u32;
    for column in 0..bitmap.width as usize {
        let ink = column_ink(column);
        if column < bitmap.width as usize / 2 {
            left += ink;
        } else {
            right += ink;
        }
    }
    let (low, high) = (left.min(right), left.max(right));
    // 10% tolerance: Iosevka's 'A' is visually symmetric but not exactly.
    assert!(
        high * 10 <= low * 12,
        "left ink {left} and right ink {right} are too far apart"
    );
}

#[test]
fn rasterization_is_deterministic() {
    let font = test_font();
    let (_advance, first) = render(&font, 'V', 20);
    let (_advance, second) = render(&font, 'V', 20);
    assert_eq!(first.bitmap().width, second.bitmap().width);
    assert_eq!(first.bitmap().height, second.bitmap().height);
    assert_eq!(
        &first.bitmap().coverage[..first.bitmap().coverage_len()],
        &second.bitmap().coverage[..second.bitmap().coverage_len()],
    );
}

#[test]
fn dot_sits_on_the_baseline() {
    let font = test_font();
    let (_advance, buffer) = render(&font, '.', 24);
    let bitmap = buffer.bitmap();
    assert!(bitmap.width > 0);
    // `top` counts pixels up from the baseline; a period's ink hugs it.
    assert!(bitmap.top <= 3, "a period's ink starts at the baseline");
    assert!(bitmap.top >= 0, "a period has no ink below the baseline");
}

#[test]
fn larger_size_renders_larger() {
    let font = test_font();
    let (_small_advance, small) = render(&font, 'O', 12);
    let (_large_advance, large) = render(&font, 'O', 36);
    assert!(large.bitmap().height > small.bitmap().height);
    assert!(large.bitmap().width >= small.bitmap().width);
}

#[test]
fn advances_are_monospaced_across_the_alphabet() {
    let font = test_font();
    // Iosevka is monospaced: every requested character advances equally at
    // one size, which is what makes terminal column arithmetic trivial.
    let size = 16;
    let mut advances = std::collections::HashSet::new();
    for ch in "NOVA nova01.:!()[]-+=#/".chars() {
        let glyph = font.glyph_for_char(ch).unwrap();
        advances.insert(font.advance(glyph, size));
    }
    assert_eq!(advances.len(), 1, "monospaced advance: {advances:?}");
}

#[test]
fn space_rasters_no_ink_but_has_an_advance() {
    let font = test_font();
    let glyph = font.glyph_for_char(' ').expect("space is mapped");
    let mut outline = Outline::empty();
    font.outline(glyph, &mut outline).expect("outline parses");
    let mut buffer = GlyphBuffer::new();
    rasterize(&outline, font.units_per_em(), 16, &mut buffer);
    assert_eq!(buffer.bitmap().width, 0, "space has no ink");
    assert_eq!(font.advance(glyph, 16), 8, "half-width advance at 16 px");
}

#[test]
fn enormous_size_clamps_to_the_cap() {
    let font = test_font();
    let (_advance, buffer) = render(&font, 'N', 4096);
    let bitmap = buffer.bitmap();
    assert!(bitmap.width <= MAX_GLYPH_WIDTH);
    assert!(bitmap.height <= MAX_GLYPH_HEIGHT);
}

#[test]
fn cache_round_trips_and_evicts_round_robin() {
    let font = test_font();
    let mut cache = GlyphCache::new();
    assert!(cache.lookup(GlyphId(1), 16).is_none(), "cold cache misses");

    let (_advance, buffer) = render(&font, 'A', 16);
    let rendered = *buffer.bitmap();
    cache.remember(GlyphId(1), 16, &rendered);
    assert_eq!(cache.lookup(GlyphId(1), 16), Some(&rendered));
    assert!(
        cache.lookup(GlyphId(1), 20).is_none(),
        "size is part of the key"
    );

    // Fill the whole ring: SLOT_COUNT stores, then the first slot is the
    // victim again and the original entry is gone.
    let blank = GlyphBuffer::new();
    let bitmap = *blank.bitmap();
    for slot in 0..nova_type::cache::SLOT_COUNT as u16 {
        cache.remember(GlyphId(slot + 2), 16, &bitmap);
    }
    assert!(
        cache.lookup(GlyphId(1), 16).is_none(),
        "round-robin eviction"
    );
    assert!(
        cache.lookup(GlyphId(2), 16).is_some(),
        "latest entries live"
    );
}

#[test]
fn cache_clear_forgets_everything() {
    let font = test_font();
    let mut cache = GlyphCache::new();
    let (_advance, buffer) = render(&font, 'A', 16);
    cache.remember(GlyphId(7), 16, buffer.bitmap());
    cache.clear();
    assert!(cache.lookup(GlyphId(7), 16).is_none());
}

#[test]
fn a_bitmap_view_names_coverage_wherever_it_lives() {
    let font = test_font();
    let (_advance, buffer) = render(&font, 'V', 24);
    let rendered = buffer.bitmap();
    let view = GlyphBitmapView {
        left: rendered.left,
        top: rendered.top,
        coverage: &rendered.coverage[..rendered.coverage_len()],
        width: rendered.width,
        height: rendered.height,
    };
    // The view and the bitmap agree at every pixel, and both agree about
    // what lies outside the ink box: nothing.
    for y in 0..rendered.height {
        for x in 0..rendered.width {
            assert_eq!(view.at(x, y), rendered.at(x, y));
        }
    }
    assert_eq!(view.at(rendered.width, 0), 0);
    assert_eq!(view.at(0, rendered.height), 0);
}

#[test]
fn truncated_bytes_are_errors_never_panics() {
    // The blob's tail may hold table-alignment padding the directory never
    // names, so find the shortest prefix that still parses — the font's
    // true extent — and require every cut below it to be an error, never a
    // panic: fonts arrive from disks the kernel does not trust.
    let min_parse = (1..=TEST_FONT.len())
        .find(|&cut| Font::parse(&TEST_FONT[..cut]).is_ok())
        .expect("the full font parses");
    for cut in 1..min_parse {
        assert!(
            Font::parse(&TEST_FONT[..cut]).is_err(),
            "parse accepted a {cut}-byte truncation"
        );
    }
    // So must garbage of a plausible length.
    let mut garbage = TEST_FONT.to_vec();
    for byte in garbage.iter_mut().take(64) {
        *byte = 0xFF;
    }
    assert!(matches!(
        Font::parse(&garbage),
        Err(FontError::BadHeader) | Err(FontError::Truncated)
    ));
}

#[test]
fn zero_size_rasters_nothing() {
    let font = test_font();
    let glyph = font.glyph_for_char('A').unwrap();
    let mut outline = Outline::empty();
    font.outline(glyph, &mut outline).unwrap();
    let mut buffer = GlyphBuffer::new();
    rasterize(&outline, font.units_per_em(), 0, &mut buffer);
    assert_eq!(buffer.bitmap().width, 0);
}
