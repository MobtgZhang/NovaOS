#!/usr/bin/env python3
"""Generate the NovaOS built-in bitmap font from a TrueType source.

The kernel and the user-space applications need glyph pixels, not outlines:
M6 renders text by writing XRGB8888 words straight into a surface buffer, and
there is no rasteriser in the image. So the printable ASCII range is baked
into `crates/nova-ui/src/font.rs` as one byte per pixel-row per glyph.

The source is DejaVu Sans Mono, whose glyph designs derive from Bitstream Vera
Sans Mono under the permission notice in `crates/nova-ui/FONT-LICENSE.txt`.
That notice allows modification and redistribution provided the copy is not
named "Bitstream" or "Vera" — this font is named `font`, in the `nova-ui`
crate.

Usage:

    python3 tools/gen_font.py

It rewrites `crates/nova-ui/src/font.rs` in place and is deterministic: the
same source font always produces the same file, so re-running it does not
create a spurious diff.
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parent.parent
OUTPUT = REPO / "crates" / "nova-ui" / "src" / "font.rs"

SOURCE_FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
# 13px gives a 7.83px advance in DejaVu Sans Mono, so every glyph fits the
# 8-pixel cell with room to spare; 14px already overflows the widest letters.
SIZE = 13
# The baseline, measured from the top of the 16-pixel cell. Capitals then
# occupy rows 3..11 and descenders reach row 13, leaving two clear rows below
# for the next line's ascenders.
BASELINE = 12
# Pixels at or above this value become ink. DejaVu's antialiasing is grey, so
# a straight "non-zero" test would turn every glyph into a solid block.
THRESHOLD = 110

CELL_WIDTH = 8
CELL_HEIGHT = 16
FIRST_CHAR = 32
LAST_CHAR = 126


def glyph_rows(font: ImageFont.FreeTypeFont, char: str) -> list[int]:
    """Render one character into 16 rows, most-significant bit first."""
    image = Image.new("L", (CELL_WIDTH, CELL_HEIGHT), 0)
    draw = ImageDraw.Draw(image)
    draw.text((0, BASELINE), char, fill=255, font=font, anchor="ls")

    rows: list[int] = []
    for y in range(CELL_HEIGHT):
        bits = 0
        for x in range(CELL_WIDTH):
            if image.getpixel((x, y)) >= THRESHOLD:
                bits |= 1 << (CELL_WIDTH - 1 - x)
        rows.append(bits)
    return rows


def missing_glyph() -> list[int]:
    """A hollow box, the conventional 'this character has no glyph' cell."""
    rows = [0] * CELL_HEIGHT
    for y in (4, 13):
        rows[y] = 0b01111110
    for y in range(5, 13):
        rows[y] = 0b01000001
    return rows


def render(source: Path) -> str:
    if not source.exists():
        raise SystemExit(
            f"source font not found: {source}\n"
            "install fonts-dejavu-core, or edit SOURCE_FONT in this script"
        )
    font = ImageFont.truetype(str(source), SIZE)

    # Prove the size still fits the cell before baking ninety-five glyphs:
    # ink past column 7 would be silently clipped into a wrong letterform.
    probe = Image.new("L", (CELL_WIDTH * 2, CELL_HEIGHT), 0)
    probe_draw = ImageDraw.Draw(probe)
    for code in range(FIRST_CHAR, LAST_CHAR + 1):
        probe_draw.text((0, BASELINE), chr(code), fill=255, font=font, anchor="ls")
        for y in range(CELL_HEIGHT):
            for x in range(CELL_WIDTH, CELL_WIDTH * 2):
                if probe.getpixel((x, y)) >= THRESHOLD:
                    raise SystemExit(
                        f"{chr(code)!r} at {SIZE}px writes past the "
                        f"{CELL_WIDTH}-pixel cell; lower SIZE"
                    )
        probe_draw.rectangle((0, 0, probe.width, probe.height), fill=0)

    lines = [
        "//! The built-in 8x16 bitmap font.",
        "//!",
        "//! # What this is",
        "//!",
        "//! One glyph per printable ASCII character, as sixteen rows of eight",
        "//! pixels packed MSB-first — one byte per row. A pixel is a bit that",
        "//! is set. Rendering is therefore a mask, an AND and a shift against",
        "//! the surface buffer: no rasteriser, no outline parsing, and no",
        "//! allocation at the point text is drawn.",
        "//!",
        "//! # Where the designs come from",
        "//!",
        "//! The glyphs are rasterised from DejaVu Sans Mono, whose designs",
        "//! derive from Bitstream Vera Sans Mono. The permission notice that",
        "//! covers them is in `../FONT-LICENSE.txt`, and it requires two",
        "//! things of a modified copy: that the notice travels with it, and",
        "//! that the copy is not named \"Bitstream\" or \"Vera\". This module is",
        "//! named `font`, and the notice ships beside it.",
        "//!",
        "//! # Why ASCII first",
        "//!",
        "//! §15.4 asks for a legally redistributable bitmap font at M6 with the",
        "//! terminal prioritising ASCII. UTF-8 shaping, fallback and a larger",
        "//! repertoire are M7's work, not this file's.",
        "",
        "/// The width of one glyph, in pixels.",
        f"pub const GLYPH_WIDTH: usize = {CELL_WIDTH};",
        "",
        "/// The height of one glyph, in pixels.",
        f"pub const GLYPH_HEIGHT: usize = {CELL_HEIGHT};",
        "",
        "/// The first character with a glyph.",
        f"pub const FIRST_CHAR: u8 = {FIRST_CHAR};",
        "",
        "/// The last character with a glyph.",
        f"pub const LAST_CHAR: u8 = {LAST_CHAR};",
        "",
        "/// How many glyphs [`GLYPHS`] holds.",
        f"pub const GLYPH_COUNT: usize = {LAST_CHAR - FIRST_CHAR + 1};",
        "",
        "/// One glyph: `GLYPH_HEIGHT` rows, bit 7 of each row the leftmost pixel.",
        f"pub type Glyph = [u8; {CELL_HEIGHT}];",
        "",
        "/// The glyph drawn for a byte outside [`FIRST_CHAR`]..=[`LAST_CHAR`].",
        "///",
        "/// A hollow box rather than a blank: a caller that passes a byte it",
        "/// cannot render should be able to see that it did, and a space-shaped",
        "/// hole in a line of text looks like missing data instead.",
        "pub static MISSING_GLYPH: Glyph = [",
    ]
    lines.extend(f"    0b{row:08b}," for row in missing_glyph())
    lines.extend(
        [
            "];",
            "",
            "/// The glyphs for ASCII 32..=126, indexed by `byte - FIRST_CHAR`.",
            "pub static GLYPHS: [Glyph; GLYPH_COUNT] = [",
        ]
    )

    for code in range(FIRST_CHAR, LAST_CHAR + 1):
        rows = glyph_rows(font, chr(code))
        lines.append(f"    // {code} {chr(code)!r}")
        lines.append("    [")
        lines.extend(f"        0b{row:08b}," for row in rows)
        lines.append("    ],")
    lines.append("];")
    lines.append("")

    lines.extend(
        [
            "/// The glyph for `byte`.",
            "///",
            "/// Bytes outside the printable ASCII range get [`MISSING_GLYPH`]",
            "/// rather than an out-of-bounds index, so a caller can pass a raw",
            "/// byte from a decoded line without checking it first.",
            "pub const fn glyph(byte: u8) -> &'static Glyph {",
            "    if byte >= FIRST_CHAR && byte <= LAST_CHAR {",
            "        &GLYPHS[(byte - FIRST_CHAR) as usize]",
            "    } else {",
            "        &MISSING_GLYPH",
            "    }",
            "}",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    source = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(SOURCE_FONT)
    OUTPUT.write_text(render(source), encoding="utf-8")
    print(f"wrote {OUTPUT.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
