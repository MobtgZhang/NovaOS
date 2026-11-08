#!/usr/bin/env python3
"""Build the NovaOS font assets from upstream font releases.

The system ships three embedded faces plus one test face, all cut from the
upstream families the system standard names:

- ``iosevka-console.ttf``   — be5invis/Iosevka, the kernel console and UEFI
  boot-manager face: ASCII, box drawing, and arrows, small enough to embed
  twice without a second thought.
- ``sarasa-terminal.ttf``   — be5invis/Sarasa-Gothic (Sarasa Mono SC), the
  terminal face: ASCII plus the GB2312 level-1 hanzi set and CJK punctuation.
- ``han-sans-gui.ttf``      — adobe-fonts/source-han-sans (SC Regular), the
  GUI application face: the same coverage, cut from the Adobe drawing.
- ``iosevka-test.ttf``      — a hand-picked handful of Iosevka glyphs, the
  fixed test asset the host test suite renders against.

Source Han ships as CFF outlines; the renderer draws TrueType quadratics, so
the face is converted with cu2qu before subsetting. Everything else is
``fontTools.subset`` with explicit codepoint sets — the assets are
reproducible from this script, never hand-edited binaries.

Usage: python3 tools/fonts/build.py <download-dir> [repo-assets-dir]
"""

import sys
from pathlib import Path

from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont, newTable
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.pens.cu2quPen import Cu2QuPen

# ASCII printable, the one range every face must cover.
ASCII = "".join(chr(c) for c in range(0x20, 0x7F))

# Arrows and box drawing: what a boot console and a menu draw around text.
CONSOLE_SYMBOLS = "".join(chr(c) for c in range(0x2190, 0x2194))
CONSOLE_BOXES = "".join(chr(c) for c in range(0x2500, 0x25A0))

# CJK punctuation the terminal and the GUI both meet in real text.
CJK_PUNCT = (
    "".join(chr(c) for c in range(0x3000, 0x3007))
    + "、。〈〉《》【】〔〕（）“”‘’！，：；？—…·「」『』"
)


def gb2312_level1() -> str:
    """GB2312 level-1 hanzi: 3755 characters, the standard 'common words'
    set a terminal is expected to spell out of the box, derived from the
    encoding itself rather than a hand-kept list."""
    hanzi = []
    for high in range(0xB0, 0xD8):
        for low in range(0xA1, 0xFF):
            try:
                hanzi.append(bytes([high, low]).decode("gb2312"))
            except UnicodeDecodeError:
                continue
    return "".join(hanzi)


def otf_to_ttf(source: Path, target: Path, max_err: float = 1.0) -> None:
    """Convert one CFF-flavoured OTF to TrueType quadratic outlines.

    cu2qu turns each cubic into quadratics within a `max_err`-per-em error
    budget — invisible at screen sizes — and TTGlyphPen re-draws the result
    as `glyf` entries. Metrics and mappings carry over untouched.
    """
    font = TTFont(str(source))
    assert font.sfntVersion == "OTTO", "not a CFF-flavoured font"
    glyph_order = font.getGlyphOrder()

    glyph_set = font.getGlyphSet()
    glyf = newTable("glyf")
    glyf.glyphOrder = glyph_order
    glyf.glyphs = {}
    for name in glyph_order:
        pen = TTGlyphPen(glyph_set)
        quad_pen = Cu2QuPen(pen, max_err=max_err, reverse_direction=True)
        glyph_set[name].draw(quad_pen)
        glyf[name] = pen.glyph()
    font["glyf"] = glyf
    # `loca` is compiled from `glyf` on save — but only when the table
    # object exists for the writer to find.
    font["loca"] = newTable("loca")

    for tag in ("CFF ", "VORG"):
        if tag in font:
            del font[tag]

    # A `glyf` font needs the version-1 `maxp` with its composite limits;
    # the converted face never uses composites, so the limits are zeros.
    maxp = newTable("maxp")
    maxp.tableVersion = 0x00010000
    maxp.numGlyphs = len(glyph_order)
    for field in (
        "maxPoints", "maxContours", "maxCompositePoints",
        "maxCompositeContours", "maxZones", "maxTwilightPoints",
        "maxStorage", "maxFunctionDefs", "maxInstructionDefs",
        "maxStackElements", "maxSizeOfInstructions",
        "maxComponentElements", "maxComponentDepth",
    ):
        setattr(maxp, field, 0)
    maxp.maxZones = 1
    font["maxp"] = maxp

    font.sfntVersion = "\x00\x01\x00\x00"
    # Long `loca`: converted CJK outlines push table offsets past 64 KiB.
    font["head"].indexToLocFormat = 1
    font.save(str(target))


def cut(source: Path, target: Path, chars: str) -> None:
    """Subset one face down to `chars` and drop everything a renderer that
    draws free outlines cannot use anyway."""
    options = Options()
    options.hinting = False
    options.desubroutinize = True
    options.drop_tables += ["GSUB", "GPOS", "vhea", "vmtx", "Fea"]
    options.notdef_outline = True
    options.name_IDs = [1, 2]
    font = TTFont(str(source))
    subsetter = Subsetter(options)
    subsetter.populate(text=chars)
    subsetter.subset(font)
    font.save(str(target))
    print(f"{target.name}: {target.stat().st_size:,} bytes")


def main() -> None:
    downloads = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/tmp/opencode/fonts")
    assets = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("assets/fonts")
    assets.mkdir(parents=True, exist_ok=True)
    work = downloads / "converted"
    work.mkdir(parents=True, exist_ok=True)

    iosevka = downloads / "extracted" / "Iosevka-Regular.ttf"
    sarasa = downloads / "extracted" / "SarasaMonoSC-Regular.ttf"
    han_otf = (
        downloads / "extracted" / "OTF" / "SimplifiedChinese"
        / "SourceHanSansSC-Regular.otf"
    )
    for source in (iosevka, sarasa, han_otf):
        if not source.exists():
            raise SystemExit(f"missing upstream font: {source}")

    # Source Han's outlines are cubics; convert once, cache by mtime.
    han_ttf = work / "SourceHanSansSC-Regular.ttf"
    if not han_ttf.exists() or han_ttf.stat().st_mtime < han_otf.stat().st_mtime:
        print(f"converting CFF -> glyf: {han_otf.name}")
        otf_to_ttf(han_otf, han_ttf)

    hanzi = gb2312_level1()

    cut(iosevka, assets / "iosevka-console.ttf", ASCII + CONSOLE_SYMBOLS + CONSOLE_BOXES)
    cut(sarasa, assets / "sarasa-terminal.ttf", ASCII + CJK_PUNCT + hanzi)
    cut(han_ttf, assets / "han-sans-gui.ttf", ASCII + CJK_PUNCT + hanzi)

    # The fixed test asset: exactly the glyphs the host test suite names.
    cut(iosevka, assets / "iosevka-test.ttf", "NOVA nova01 .:!()[]-+=#/")


if __name__ == "__main__":
    main()
