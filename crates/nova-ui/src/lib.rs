//! NovaUI: the toolkit §15.2 and §16.3 call `nova-ui` — widgets, layout
//! geometry, text, and the key decoding the widgets are edited with.
//!
//! # Why this is a crate rather than a module of the applications
//!
//! Everything here is `core`-only logic: no syscalls, no services, no
//! allocator. That makes it compilable twice — for the bare-metal
//! applications *and* for the host test runner — which is the whole point.
//! `nova-apps` cannot be tested on the host at all: it is a set of
//! `no_std`/`no_main` binaries with their own `_start`, so it is excluded
//! from every host test sweep by an explicitly asserted policy. A widget
//! state machine that lives there is a widget state machine nobody ever runs
//! a test against, and the button that fires on the wrong event is exactly
//! the sort of bug that survives review.
//!
//! # What is here, and what is deliberately not
//!
//! Three controls and the drawing they need: a label-bearing [`Button`], an
//! editable [`TextBox`], and the [`Canvas`] both are drawn into, plus
//! [`Rect`] for their geometry. That is the acceptance criterion G-005 —
//! "按钮/布局/文本框" — and nothing more. There is no layout engine, no theme
//! engine and no widget tree: a M6 application places its controls at explicit
//! coordinates, because a layout engine that guesses is a second program to
//! get right before the desktop can be shown at all.
//!
//! [`keyboard`] is the other half of the same job: a widget that accepts text
//! has to know what a scancode means, and that decoding is pure enough to be
//! tested the same way.
//!
//! # Drawing is clipped, always
//!
//! [`Canvas`] refuses to write outside its surface. The compositor clips the
//! *surface* against the display, but a widget that drew past its own window
//! would corrupt the pixels beside it inside the same buffer, and no
//! compositor can see that. Every primitive here therefore clips to the
//! surface, so a label whose text overflows its button is a shorter label
//! rather than a corrupted window.
//!
//! # One pixel format
//!
//! Everything writes `XRGB8888` words, the only format §15.3 allows. There is
//! no format negotiation in this crate because there is nothing to
//! negotiate: a surface that is not `XRGB8888` never reaches a `Canvas`.

#![no_std]

/// The built-in 8x16 bitmap font, rasterised from DejaVu Sans Mono.
pub mod font;

/// PS/2 set-1 scancodes decoded into characters and editing keys.
pub mod keyboard;

/// The vector typeface a process may select over the bitmap fallback.
pub mod typeface;

use font::{GLYPH_HEIGHT, GLYPH_WIDTH};

/// The XRGB8888 word for a colour.
///
/// Red, green and blue in the top three bytes, with the low byte left clear
/// because `XRGB8888` has no alpha channel and the compositor copies the
/// word through unchanged.
pub const fn colour(red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

/// Write `value` as decimal ASCII into `out`, returning the digits written.
///
/// A buffer too small for the whole number returns an empty string rather
/// than a truncated one: `1024` drawn as `10` is a lie about a file's size,
/// and a label that draws nothing is merely absent. The number is built
/// backwards from its last digit, which is how decimal works — the digits
/// come out least-significant first and are then reversed into the caller's
/// buffer.
pub fn decimal(value: usize, out: &mut [u8]) -> &str {
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut rest = value;
    while rest > 0 && count < digits.len() {
        digits[count] = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
    }
    if count == 0 {
        digits[0] = b'0';
        count = 1;
    }
    if out.len() < count {
        return "";
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    core::str::from_utf8(&out[..count]).unwrap_or("")
}

/// A rectangle in surface coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    /// Left edge, signed so a control may sit partly off the left of a
    /// window during an animation or a drag.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// A rectangle from its top-left corner and size.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether a point lies inside, with the right and bottom edges excluded
    /// so two adjacent rectangles never both claim their shared edge.
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }

    /// The same rectangle moved inward on every side.
    pub const fn inset(&self, amount: u32) -> Rect {
        let shrink = amount.saturating_mul(2);
        Rect {
            x: self.x + amount as i32,
            y: self.y + amount as i32,
            width: self.width.saturating_sub(shrink),
            height: self.height.saturating_sub(shrink),
        }
    }
}

/// The desktop's palette, so the three windows look like one system.
/// The EDE 2.x colour scheme.
///
/// Every value here is taken from the assets EDE itself ships — the
/// pekwm `default` theme (a restyle of EDE 1.x's `classic2`) for the
/// window chrome, and the same theme's menu images for the menu
/// colours — so the desktop reads as EDE rather than as a dark theme
/// wearing EDE's layout:
///
/// - focused title bar `#4B6983`, unfocused `#EAE8E3` (title.png /
///   title-unfocus.png),
/// - window border `#C6C6C6` (border.png),
/// - title buttons `#E6E6E6` with `#222` glyphs (close.png),
/// - menu header `#7F9EBC`, menu body `#FBFBFB` (top-menu.png /
///   bottom-menu.png).
pub mod palette {
    use crate::colour;

    /// Window background: EDE's warm light gray.
    pub const SURFACE: u32 = colour(234, 232, 227);
    /// Title bar and pressed-button face: the focused-title blue.
    pub const CHROME: u32 = colour(75, 105, 131);
    /// A pressed button, and a darker accent.
    pub const CHROME_SUNK: u32 = colour(58, 82, 104);
    /// Default text on a light surface.
    pub const TEXT: u32 = colour(34, 34, 34);
    /// Text on a chrome (blue) background.
    pub const TEXT_ON_CHROME: u32 = colour(255, 255, 255);
    /// A text box's background.
    pub const FIELD: u32 = colour(255, 255, 255);
    /// A control's outline.
    pub const BORDER: u32 = colour(198, 198, 198);
    /// A selected list row: the title blue, with white text.
    pub const SELECTED: u32 = colour(75, 105, 131);
    /// The status line's background.
    pub const STATUS: u32 = colour(234, 232, 227);
    /// The bevel's light edge (top/left of an up-box).
    pub const FACE_LIGHT: u32 = colour(245, 244, 241);
    /// The bevel's dark edge (bottom/right of an up-box).
    pub const FACE_DARK: u32 = colour(168, 166, 161);
    /// A title button's face.
    pub const BUTTON_FACE: u32 = colour(230, 230, 230);
    /// An unfocused title bar.
    pub const TITLE_UNFOCUS: u32 = colour(234, 232, 227);
    /// Text on an unfocused title bar.
    pub const TEXT_DIM: u32 = colour(119, 119, 119);
    /// A menu's body.
    pub const MENU_FACE: u32 = colour(251, 251, 251);
    /// A menu's header bar.
    pub const MENU_HEADER: u32 = colour(127, 158, 188);
}

/// An XRGB8888 surface a `Canvas` draws into.
///
/// The canvas does not own the pixels: the application mapped them with `MAP`
/// and hands the pointer over. Every write is clipped to `width` x `height`,
/// so the worst a wrong size can do is draw nothing — never scribble past the
/// end of the mapping.
pub struct Canvas {
    pixels: *mut u32,
    /// Pixels between the first pixel of one row and the next.
    stride: usize,
    /// Visible width in pixels.
    width: usize,
    /// Visible height in pixels.
    height: usize,
}

impl Canvas {
    /// A canvas over a mapped surface buffer.
    ///
    /// `buffer` is the pointer `MAP` returned for a surface buffer of
    /// `width * height` `XRGB8888` words laid out with `width` words per row.
    /// The pointer must stay mapped and exclusively owned for as long as the
    /// canvas is alive; a surface buffer is per-process and unmapped only
    /// when the program exits, so in practice it outlives every canvas the
    /// program makes.
    pub fn new(buffer: *mut u8, width: u32, height: u32) -> Canvas {
        Canvas {
            // SAFETY: the caller's contract above — the pointer is a mapped
            // surface buffer of at least `width * height` words. The canvas
            // never writes outside that region because every write goes
            // through `set`, which clips first.
            pixels: buffer as *mut u32,
            stride: width as usize,
            width: width as usize,
            height: height as usize,
        }
    }

    /// Width in pixels.
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Write one pixel, silently dropping it outside the surface.
    fn set(&mut self, x: i32, y: i32, word: u32) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return;
        }
        // SAFETY: `x < width` and `y < height`, so `y * stride + x` is inside
        // the `stride * height` words the constructor was given, and the
        // buffer is exclusively owned for the canvas's lifetime.
        unsafe {
            *self.pixels.add(y * self.stride + x) = word;
        }
    }

    /// Paint one pixel, clipping silently to the surface bounds.
    pub fn set_pixel(&mut self, x: i32, y: i32, word: u32) {
        self.set(x, y, word);
    }

    /// Fill the whole surface.
    pub fn clear(&mut self, word: u32) {
        self.fill_rect(Rect::new(0, 0, self.width as u32, self.height as u32), word);
    }

    /// Fill a rectangle, clipped to the surface.
    pub fn fill_rect(&mut self, rect: Rect, word: u32) {
        let left = rect.x.max(0);
        let top = rect.y.max(0);
        let right = (rect.x.saturating_add(rect.width as i32)).min(self.width as i32);
        let bottom = (rect.y.saturating_add(rect.height as i32)).min(self.height as i32);
        for y in top..bottom {
            for x in left..right {
                self.set(x, y, word);
            }
        }
    }

    /// Draw a one-pixel outline around a rectangle.
    pub fn stroke_rect(&mut self, rect: Rect, word: u32) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, 1), word);
        self.fill_rect(
            Rect::new(rect.x, rect.y + rect.height as i32 - 1, rect.width, 1),
            word,
        );
        self.fill_rect(Rect::new(rect.x, rect.y, 1, rect.height), word);
        self.fill_rect(
            Rect::new(rect.x + rect.width as i32 - 1, rect.y, 1, rect.height),
            word,
        );
    }

    /// Draw one glyph cell.
    ///
    /// `background` of `None` leaves the pixels behind the glyph's clear bits
    /// untouched, which is what lets text be drawn over an existing panel;
    /// `Some(word)` paints them, which is what a text box needs so the
    /// previous contents cannot show through.
    pub fn draw_glyph(
        &mut self,
        x: i32,
        y: i32,
        byte: u8,
        foreground: u32,
        background: Option<u32>,
    ) {
        let glyph = font::glyph(byte);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..GLYPH_WIDTH {
                let lit = bits & (1u8 << (GLYPH_WIDTH - 1 - column)) != 0;
                let word = match (lit, background) {
                    (true, _) => Some(foreground),
                    (false, Some(word)) => Some(word),
                    (false, None) => None,
                };
                if let Some(word) = word {
                    self.set(x + column as i32, y + row as i32, word);
                }
            }
        }
    }

    /// Read back one pixel, or `None` outside the surface.
    ///
    /// The vector text path blends against whatever is already on the
    /// pixel, which needs the read as well as the write.
    fn get(&self, x: i32, y: i32) -> Option<u32> {
        if x < 0 || y < 0 {
            return None;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return None;
        }
        // SAFETY: `x < width` and `y < height`, so the index is inside the
        // buffer the constructor was given.
        Some(unsafe { *self.pixels.add(y * self.stride + x) })
    }

    /// The width one character advances the pen, with the active vector
    /// face or the bitmap cell.
    pub fn char_advance(&self, ch: char) -> u32 {
        typeface::with_face(|face| face.char_advance(ch)).unwrap_or(GLYPH_WIDTH as u32)
    }

    /// The height of one text row, with the active vector face or the
    /// bitmap cell.
    pub fn text_row_height(&self) -> u32 {
        typeface::with_face(|face| face.row_height()).unwrap_or(GLYPH_HEIGHT as u32)
    }

    /// Draw ASCII text at `(x, y)`, one glyph per pixel column.
    ///
    /// When a vector face is active ([`typeface::set_face`]) the text is
    /// UTF-8 decoded and rendered from the face's outlines, anti-aliased,
    /// each character advancing by the face's own metric — CJK glyphs land
    /// at their full width, not a cell per byte. The coverage blends
    /// against whatever is already on each pixel: callers that need an
    /// opaque background paint it first, exactly as the bitmap path's
    /// `Some(background)` callers already do.
    ///
    /// Without a face, bytes outside printable ASCII are drawn as the
    /// missing-glyph cell rather than skipped, so a client that passes a
    /// Latin-1 or UTF-8 byte sees a box where the character should be
    /// instead of a silently shortened line.
    pub fn draw_text(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        foreground: u32,
        background: Option<u32>,
    ) {
        let Some(face) = typeface::active_face() else {
            self.draw_text_bitmap(x, y, text, foreground, background);
            return;
        };
        self.draw_text_vector(face, x, y, text, foreground);
    }

    /// The bitmap text path: one 8x16 cell per byte.
    fn draw_text_bitmap(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        foreground: u32,
        background: Option<u32>,
    ) {
        for (index, byte) in text.bytes().enumerate() {
            let column = x + (index as i32) * GLYPH_WIDTH as i32;
            if column >= self.width as i32 {
                break;
            }
            self.draw_glyph(column, y, byte, foreground, background);
        }
    }

    /// The vector text path: UTF-8 characters, per-character advance,
    /// coverage alpha-blended onto the surface.
    fn draw_text_vector(
        &mut self,
        face: &mut typeface::Face,
        mut pen_x: i32,
        y: i32,
        text: &str,
        foreground: u32,
    ) {
        let baseline = y + face.ascent() as i32;
        for ch in text.chars() {
            let (bitmap, advance) = match face.glyph(ch) {
                Some(glyph) => glyph,
                None => {
                    // No outline for this character: skip its advance so a
                    // mixed line keeps the rest of its text in step.
                    pen_x += face.char_advance(ch) as i32;
                    continue;
                }
            };

            let origin_x = pen_x + bitmap.left;
            let origin_y = baseline - bitmap.top;
            for row in 0..bitmap.height {
                let py = origin_y + row as i32;
                for column in 0..bitmap.width {
                    let alpha =
                        bitmap.coverage[row as usize * bitmap.width as usize + column as usize];
                    if alpha == 0 {
                        continue;
                    }
                    let px = origin_x + column as i32;
                    let Some(existing) = self.get(px, py) else {
                        continue;
                    };
                    let blended = mix(foreground, existing, alpha);
                    self.set(px, py, blended);
                }
            }
            pen_x += advance as i32;
        }
    }

    /// Alpha-blend a tightly packed RGBA image onto the canvas.
    ///
    /// The source is `width * height` pixels in RGBA byte order, alpha as
    /// coverage per pixel — the same straight-alpha blend the compositor
    /// and the text path apply. It exists for images a face cannot
    /// supply: the edeneu application icons the desktop shell ships.
    pub fn blit_rgba(&mut self, x: i32, y: i32, width: u32, height: u32, src: &[u8]) {
        let expected = width as usize * height as usize * 4;
        if src.len() != expected {
            return;
        }
        for row in 0..height as i32 {
            for col in 0..width as i32 {
                let dx = x + col;
                let dy = y + row;
                let offset = ((row * width as i32 + col) * 4) as usize;
                let alpha = src[offset + 3];
                if alpha == 0 {
                    continue;
                }
                let Some(dst) = self.get(dx, dy) else {
                    continue;
                };
                self.set(
                    dx,
                    dy,
                    mix3(src[offset], src[offset + 1], src[offset + 2], alpha, dst),
                );
            }
        }
    }

    /// The width `text` occupies when drawn with [`Canvas::draw_text`].
    pub fn text_width(text: &str) -> u32 {
        match typeface::with_face(|face| text.chars().map(|ch| face.char_advance(ch)).sum::<u32>())
        {
            Some(width) => width,
            None => (text.len() as u32).saturating_mul(GLYPH_WIDTH as u32),
        }
    }

    /// Draw `text` centred inside `rect`, clipped to it.
    ///
    /// A label longer than its rectangle is drawn from the rectangle's left
    /// edge and clipped by the surface, so a button whose label does not fit
    /// shows its beginning rather than nothing.
    pub fn draw_label(&mut self, rect: Rect, text: &str, foreground: u32, background: Option<u32>) {
        let width = Self::text_width(text);
        let x = rect.x + (rect.width.saturating_sub(width) / 2) as i32;
        let y = rect.y + (rect.height.saturating_sub(GLYPH_HEIGHT as u32) / 2) as i32;
        self.draw_text(x, y, text, foreground, background);
    }
}

/// Blend a straight-alpha RGB source pixel over an XRGB8888 `dst` at
/// `alpha` coverage (0..=255), rounding to nearest.
fn mix3(sr: u8, sg: u8, sb: u8, alpha: u8, dst: u32) -> u32 {
    let a = u32::from(alpha);
    let inv = 255 - a;
    let channel = |fg: u8, shift: u32| -> u32 {
        let bg = (dst >> shift) & 0xFF;
        ((u32::from(fg) * a + bg * inv + 127) / 255) << shift
    };
    channel(sr, 16) | channel(sg, 8) | channel(sb, 0)
}

/// Blend an XRGB8888 `foreground` over `background` at `alpha` coverage
/// (0..=255), rounding to nearest.
///
/// The vector text path's per-pixel composite. Both words are read as
/// opaque colours — the surface has no alpha channel, so the coverage is
/// the only thing being mixed.
fn mix(foreground: u32, background: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let channel = |shift: u32| -> u32 {
        let fg = (foreground >> shift) & 0xFF;
        let bg = (background >> shift) & 0xFF;
        ((fg * a + bg * (255 - a) + 127) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// A push button.
///
/// The button activates on the *release* that lands inside it, not on the
/// press. That is the convention every desktop follows, and it exists so a
/// user who presses a button and then changes their mind can drag off it and
/// let go without firing anything — [`Button::on_pointer`] cancels the press
/// the moment the pointer leaves the rectangle.
#[derive(Clone, Copy, Debug)]
pub struct Button {
    /// Where the button sits.
    pub rect: Rect,
    /// The label, ASCII.
    pub label: &'static str,
    /// Whether the button is held down.
    down: bool,
}

impl Button {
    /// A button at `rect` with `label`.
    pub const fn new(rect: Rect, label: &'static str) -> Self {
        Self {
            rect,
            label,
            down: false,
        }
    }

    /// Whether the button is currently held.
    pub const fn is_down(&self) -> bool {
        self.down
    }

    /// Feed one pointer event; returns true when a press was released inside.
    ///
    /// `primary` is whether the primary button is down *after* this event. A
    /// press outside leaves the button up; a press inside followed by a drag
    /// off it cancels; a release inside a held button fires.
    pub fn on_pointer(&mut self, x: i32, y: i32, primary: bool) -> bool {
        let inside = self.rect.contains(x, y);
        if primary {
            self.down = inside;
            return false;
        }
        let activated = self.down && inside;
        self.down = false;
        activated
    }

    /// Draw the button in its current state.
    ///
    /// EDE's buttons are FLTK up-boxes: a light edge along the top and
    /// left, a dark edge along the bottom and right, and the label
    /// shifting a pixel down-and-right while held. The bevel is drawn
    /// with four one-pixel fills because the canvas has no line
    /// primitive — and because a bevel *is* four rectangles.
    pub fn draw(&self, canvas: &mut Canvas) {
        let face = palette::SURFACE;
        canvas.fill_rect(self.rect, face);
        let (light, dark, offset) = if self.down {
            (palette::FACE_DARK, palette::FACE_LIGHT, 1)
        } else {
            (palette::FACE_LIGHT, palette::FACE_DARK, 0)
        };
        let (x, y) = (self.rect.x, self.rect.y);
        let (w, h) = (self.rect.width, self.rect.height);
        canvas.fill_rect(Rect::new(x, y, w, 1), light);
        canvas.fill_rect(Rect::new(x, y, 1, h), light);
        canvas.fill_rect(Rect::new(x, y.saturating_add(h as i32 - 1), w, 1), dark);
        canvas.fill_rect(Rect::new(x.saturating_add(w as i32 - 1), y, 1, h), dark);
        let label_rect = Rect::new(x + offset, y + offset, w, h);
        canvas.draw_label(label_rect, self.label, palette::TEXT, Some(face));
    }
}

/// The height of an EDE window's title bar, in pixels.
///
/// The apps lay their first content row out below this, so it is part
/// of their layout contract, not just a drawing constant.
pub const TITLEBAR_HEIGHT: i32 = 22;

/// The width of one title-bar button.
const TITLE_BUTTON: i32 = 18;
/// The gap between title-bar buttons and the bar's right edge.
const TITLE_BUTTON_MARGIN: i32 = 2;

/// Which title-bar button a click landed on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TitlebarHit {
    /// The ✕ button: close the window.
    Close,
    /// The □ button: maximize (visual only until the compositor grows
    /// surfaces on request).
    Maximize,
    /// The — button: minimize to the taskbar.
    Iconify,
    /// Anywhere else on the bar, or off it entirely.
    None,
}

/// Hit-test the title bar of a window `width` pixels wide.
///
/// The three buttons hug the right edge in EDE/pekwm order, left to
/// right: iconify, maximize, close. Geometry only — the caller decides
/// what each hit means.
pub fn titlebar_hit(x: i32, y: i32, width: i32) -> TitlebarHit {
    if !(0..TITLEBAR_HEIGHT).contains(&y) || !(0..width).contains(&x) {
        return TitlebarHit::None;
    }
    // The buttons sit in a 2px-inset strip: vertically 2..20, ending
    // 2px short of the right edge — exactly where `draw_titlebar`
    // paints them.
    let button_top = 2;
    let button_bottom = TITLEBAR_HEIGHT - 2;
    let right_edge = width - TITLE_BUTTON_MARGIN;
    if y < button_top || y >= button_bottom || x >= right_edge {
        return TitlebarHit::None;
    }
    let close_x = right_edge - TITLE_BUTTON;
    let max_x = close_x - TITLE_BUTTON;
    let icon_x = max_x - TITLE_BUTTON;
    if x >= close_x {
        TitlebarHit::Close
    } else if x >= max_x {
        TitlebarHit::Maximize
    } else if x >= icon_x {
        TitlebarHit::Iconify
    } else {
        TitlebarHit::None
    }
}

/// Draw an EDE window title bar across the top of a window.
///
/// Focused windows get the steel-blue bar (`#4B6983`, from EDE's
/// shipped `title.png`) with white text; unfocused windows get the
/// warm gray (`#EAE8E3`, from `title-unfocus.png`) with dim text and a
/// border line under the bar. The three buttons on the right are drawn
/// as their glyphs — □, — and ✕ — in the theme's `#E6E6E6` face.
pub fn draw_titlebar(canvas: &mut Canvas, width: i32, title: &str, focused: bool) {
    draw_titlebar_at(canvas, 0, 0, width, title, focused);
}

/// [`draw_titlebar`] at a non-zero origin — a dialog's title bar
/// starts at `(x, y)` rather than at the surface's corner.
pub fn draw_titlebar_at(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    width: i32,
    title: &str,
    focused: bool,
) {
    let (bar, text) = if focused {
        (palette::CHROME, palette::TEXT_ON_CHROME)
    } else {
        (palette::TITLE_UNFOCUS, palette::TEXT_DIM)
    };
    let bar_rect = Rect::new(x, y, width as u32, TITLEBAR_HEIGHT as u32);
    canvas.fill_rect(bar_rect, bar);
    if !focused {
        canvas.fill_rect(
            Rect::new(x, y + TITLEBAR_HEIGHT - 1, width as u32, 1),
            palette::BORDER,
        );
    }
    canvas.draw_text(x + 8, y + 3, title, text, Some(bar));

    // The three buttons, right-aligned: iconify, maximize, close.
    let button_y = y + 2;
    let glyph_y = button_y + 4;
    for (index, kind) in [
        TitlebarHit::Iconify,
        TitlebarHit::Maximize,
        TitlebarHit::Close,
    ]
    .into_iter()
    .enumerate()
    {
        let left = x + width - TITLE_BUTTON_MARGIN - (index as i32 + 1) * TITLE_BUTTON;
        canvas.fill_rect(
            Rect::new(
                left,
                button_y,
                TITLE_BUTTON as u32,
                (TITLEBAR_HEIGHT - 4) as u32,
            ),
            palette::BUTTON_FACE,
        );
        canvas.stroke_rect(
            Rect::new(
                left,
                button_y,
                TITLE_BUTTON as u32,
                (TITLEBAR_HEIGHT - 4) as u32,
            ),
            palette::BORDER,
        );
        match kind {
            // Iconify: a bar sitting on the button's floor.
            TitlebarHit::Iconify => {
                canvas.fill_rect(Rect::new(left + 4, glyph_y + 7, 9, 2), palette::TEXT);
            }
            // Maximize: an outlined square.
            TitlebarHit::Maximize => {
                canvas.stroke_rect(Rect::new(left + 4, glyph_y, 9, 8), palette::TEXT);
            }
            // Close: an X, two diagonal runs of pixels.
            TitlebarHit::Close => {
                for step in 0..8 {
                    canvas.set_pixel(left + 4 + step, glyph_y + step, palette::TEXT);
                    canvas.set_pixel(left + 11 - step, glyph_y + step, palette::TEXT);
                }
            }
            TitlebarHit::None => {}
        }
    }
}
///
/// The text lives in the caller's buffer rather than inside the field, so a
/// program can hold a megabyte of text without this struct becoming a
/// megabyte — and so the buffer can outlive a field that is destroyed and
/// replaced. The field keeps only where the cursor is and how much is
/// written, which is all it needs to draw.
#[derive(Clone, Copy, Debug)]
pub struct TextBox {
    /// Where the field sits.
    pub rect: Rect,
    /// Bytes in use at the front of the caller's buffer.
    len: usize,
    /// Insertion point, in bytes, always `<= len`.
    cursor: usize,
    /// Whether the field currently receives keys.
    pub focused: bool,
}

impl TextBox {
    /// An empty field at `rect`.
    pub const fn new(rect: Rect) -> Self {
        Self {
            rect,
            len: 0,
            cursor: 0,
            focused: false,
        }
    }

    /// Bytes currently in the field.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the field holds nothing.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The text, as the caller's buffer sees it.
    pub fn text<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        let end = self.len.min(bytes.len());
        &bytes[..end]
    }

    /// Replace the field's contents with `bytes`, truncated to the buffer.
    pub fn set_text(&mut self, bytes: &mut [u8], text: &[u8]) {
        let count = text.len().min(bytes.len());
        bytes[..count].copy_from_slice(&text[..count]);
        self.len = count;
        self.cursor = count;
    }

    /// Adopt `len` bytes of the caller's buffer as the field's contents.
    ///
    /// For text the caller has already arranged in its own buffer — a
    /// scrollback that shifts its oldest line off the front when it fills,
    /// say — [`TextBox::set_text`] would copy bytes onto themselves, and this
    /// only has to say how many of them count. The cursor moves to the end,
    /// which is where the next append belongs.
    ///
    /// The length is not clamped to the buffer: drawing and editing both
    /// clamp against the buffer they are given, so a count from a larger
    /// buffer can draw no more than the smaller one holds.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        self.cursor = len;
    }

    /// Empty the field.
    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
    }

    /// Insert one byte at the cursor.
    ///
    /// A full buffer refuses the byte rather than dropping the oldest one:
    /// silently deleting the user's first character to make room for their
    /// last is not an edit anybody asked for.
    pub fn insert(&mut self, bytes: &mut [u8], byte: u8) -> bool {
        if self.len >= bytes.len() || self.cursor > self.len {
            return false;
        }
        for index in (self.cursor..self.len).rev() {
            bytes[index + 1] = bytes[index];
        }
        bytes[self.cursor] = byte;
        self.len += 1;
        self.cursor += 1;
        true
    }

    /// Delete the character before the cursor.
    ///
    /// A UTF-8 multi-byte character is deleted whole: a cursor left inside
    /// one would let the next insertion split it into invalid bytes.
    pub fn backspace(&mut self, bytes: &mut [u8]) -> bool {
        if self.cursor == 0 || self.cursor > self.len || self.cursor > bytes.len() {
            return false;
        }
        let Some(start) = char_start_before(bytes, self.cursor) else {
            return false;
        };
        for index in self.cursor..self.len {
            bytes[index - (self.cursor - start)] = bytes[index];
        }
        self.len -= self.cursor - start;
        self.cursor = start;
        true
    }

    /// Move the cursor one character left, never splitting a UTF-8
    /// sequence.
    pub fn cursor_left(&mut self, bytes: &[u8]) {
        let Some(start) = char_start_before(bytes, self.cursor) else {
            return;
        };
        self.cursor = start;
    }

    /// Move the cursor one character right, never past the end and never
    /// into the middle of a UTF-8 sequence.
    pub fn cursor_right(&mut self, bytes: &[u8]) {
        if self.cursor >= self.len {
            return;
        }
        self.cursor = char_end_after(bytes, self.cursor).min(self.len);
    }

    /// The cursor's byte index.
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Draw the field and its contents, scrolled so the cursor stays visible.
    ///
    /// Layout is per *character*, not per byte: a UTF-8 multi-byte character
    /// is one glyph that advances by the active face's own metric, so CJK
    /// text occupies its full width rather than a cell per byte. A `\n` ends
    /// a row; a character that does not fit wraps onto the next one. That
    /// makes the field a small multi-line editor rather than one long line
    /// that runs off the side — which is what a text box in an editor has to
    /// be.
    ///
    /// Rows above the window are laid out and skipped rather than ignored:
    /// every character belongs to exactly one row, so scrolling changes
    /// which rows are *drawn* and never which characters appear in them.
    pub fn draw(&self, canvas: &mut Canvas, bytes: &[u8]) {
        canvas.fill_rect(self.rect, palette::FIELD);
        canvas.stroke_rect(
            self.rect,
            if self.focused {
                palette::SELECTED
            } else {
                palette::BORDER
            },
        );

        let inner = self.rect.inset(4);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let row_height = canvas.text_row_height();
        let rows = (inner.height / row_height).max(1) as usize;

        let end = self.len.min(bytes.len());
        let text = &bytes[..end];
        let (cursor_row, cursor_pen) =
            layout_position(text, self.cursor.min(end), inner.width, canvas);
        let scroll = cursor_row.saturating_sub(rows.saturating_sub(1));

        let mut index = 0usize;
        let mut row = 0usize;
        while index < end && row < scroll + rows {
            let start = index;
            let mut pen = 0u32;
            let mut overflowed = false;
            // Lay out one row: as many characters as fit, or the newline
            // that ends it, whichever comes first.
            while index < end {
                let Some((ch, size)) = decode_char(&text[index..]) else {
                    // Not a valid UTF-8 start byte. Draw the missing-glyph
                    // treatment for it — one bitmap cell's worth of width,
                    // so a corrupt byte is visible rather than silently
                    // swallowed — and move past it.
                    if pen + GLYPH_WIDTH as u32 > inner.width {
                        overflowed = true;
                        break;
                    }
                    if row >= scroll {
                        canvas.draw_glyph(
                            inner.x + pen as i32,
                            inner.y + ((row - scroll) as i32) * row_height as i32,
                            text[index],
                            palette::TEXT,
                            Some(palette::FIELD),
                        );
                    }
                    pen += GLYPH_WIDTH as u32;
                    index += 1;
                    continue;
                };
                if ch == '\n' {
                    index += size;
                    break;
                }
                let advance = canvas.char_advance(ch);
                if pen + advance > inner.width {
                    overflowed = true;
                    break;
                }
                if row >= scroll {
                    let mut encoded = [0u8; 4];
                    canvas.draw_text(
                        inner.x + pen as i32,
                        inner.y + ((row - scroll) as i32) * row_height as i32,
                        ch.encode_utf8(&mut encoded),
                        palette::TEXT,
                        None,
                    );
                }
                pen += advance;
                index += size;
            }
            // A row that filled exactly and is followed by a newline: the
            // wrap already moved the cursor to where that newline would, so
            // consuming it here is what stops it from opening a line of its
            // own. Without this, every line typed to exactly the field's
            // width would be followed by a blank one.
            if overflowed && index < end && text[index] == b'\n' {
                index += 1;
            }
            if index == start {
                // No bytes consumed: a row that cannot make progress would
                // spin here forever.
                break;
            }
            row += 1;
        }

        if self.focused && cursor_row >= scroll && cursor_row < scroll + rows {
            // A one-pixel caret, not a block: a block would overwrite the
            // character the user is about to type in front of.
            canvas.fill_rect(
                Rect::new(
                    inner.x + cursor_pen as i32,
                    inner.y + ((cursor_row - scroll) as i32) * row_height as i32,
                    1,
                    row_height,
                ),
                palette::TEXT,
            );
        }
    }
}

/// Decode one UTF-8 character from the front of `bytes`, returning it with
/// its length in bytes.
///
/// `None` for a byte that cannot start a character: the caller draws that
/// case as a missing glyph rather than pretending it was text.
fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let first = *bytes.first()?;
    let length = match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        // 0xC0 and 0xC1 are overlong encodings, and 0x80..=0xBF are
        // continuation bytes with no lead — neither decodes.
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return None,
    };
    let slice = bytes.get(..length)?;
    let mut chars = core::str::from_utf8(slice).ok()?.chars();
    let ch = chars.next()?;
    // A valid encoding decodes to exactly one character; a multi-character
    // slice would mean the length above lied.
    if chars.next().is_some() {
        return None;
    }
    Some((ch, length))
}

/// Which row a byte index sits in, and the pen's x-offset inside that row,
/// for a field `width` pixels wide.
///
/// This is the layout [`TextBox::draw`] draws — a newline ends a row, a
/// character that does not fit wraps, and a newline directly after a full
/// row belongs to that row — because the caret must be placed by the same
/// rules that placed the glyphs. Two implementations that disagreed would
/// put the caret on the line above or below the one it is actually in, and
/// only when the text happened to wrap at exactly the wrong moment.
fn layout_position(text: &[u8], cursor: usize, width: u32, canvas: &Canvas) -> (usize, u32) {
    let mut row = 0usize;
    let mut pen = 0u32;
    let mut index = 0usize;
    while index < cursor {
        let Some((ch, size)) = decode_char(&text[index..]) else {
            pen += GLYPH_WIDTH as u32;
            index += 1;
            continue;
        };
        if ch == '\n' {
            row += 1;
            pen = 0;
            index += size;
            continue;
        }
        let advance = canvas.char_advance(ch);
        if pen + advance > width {
            // The same absorption the drawing loop performs, and only when
            // the newline is already behind the cursor: a cursor resting
            // *on* the newline has not consumed it, and sits at the start
            // of the row the wrap just opened.
            if index + size < cursor && text.get(index + size) == Some(&b'\n') {
                index += size;
            }
            row += 1;
            pen = 0;
        }
        pen += advance;
        index += size;
    }
    (row, pen)
}

/// The index of the character boundary at or immediately before `cursor`.
///
/// `None` when `cursor` is zero or past the bytes' end — a caller whose
/// cursor is out of bounds refuses to move rather than silently clamping.
fn char_start_before(bytes: &[u8], cursor: usize) -> Option<usize> {
    if cursor == 0 || cursor > bytes.len() {
        return None;
    }
    let mut start = cursor - 1;
    // Continuation bytes (0b10xxxxxx) trail their lead byte; walking back
    // over them lands on the character's first byte.
    while start > 0 && bytes[start] & 0xC0 == 0x80 {
        start -= 1;
    }
    Some(start)
}

/// The index of the character boundary immediately after `cursor`.
fn char_end_after(bytes: &[u8], cursor: usize) -> usize {
    let mut end = cursor + 1;
    while end < bytes.len() && bytes[end] & 0xC0 == 0x80 {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::{Button, Canvas, Rect, TextBox, colour, decimal, palette};

    /// How many pixels of ink fall in the horizontal band `y0..y1`.
    ///
    /// "Ink" is the palette's text colour. The background and the border are
    /// different colours, so a non-zero count means a glyph or a caret was
    /// actually drawn there and not merely the field's own paint.
    fn ink(pixels: &[u32], width: usize, y0: usize, y1: usize) -> usize {
        pixels
            .iter()
            .enumerate()
            .filter(|(index, word)| {
                let y = index / width;
                y >= y0 && y < y1 && **word == palette::TEXT
            })
            .count()
    }

    /// A canvas over a caller-owned pixel array, so a test can read back what
    /// was drawn instead of only trusting that the call returned.
    fn canvas(pixels: &mut [u32], width: u32, height: u32) -> Canvas {
        // SAFETY: the slice is `width * height` words long — the caller passes
        // an array sized exactly that — and it is borrowed mutably for as long
        // as the canvas lives, so nothing else can write those pixels.
        let buffer = pixels.as_mut_ptr() as *mut u8;
        Canvas::new(buffer, width, height)
    }

    fn pixel(pixels: &[u32], width: usize, x: usize, y: usize) -> u32 {
        pixels[y * width + x]
    }

    #[test]
    fn decimal_writes_the_whole_number_or_nothing() {
        let mut out = [0u8; 8];
        assert_eq!(decimal(0, &mut out), "0");
        assert_eq!(decimal(42, &mut out), "42");
        assert_eq!(decimal(1024, &mut out), "1024");

        let mut tiny = [0u8; 2];
        assert_eq!(decimal(7, &mut tiny), "7");
        assert_eq!(
            decimal(1024, &mut tiny),
            "",
            "1024 drawn as 10 would be a lie about a file's size"
        );
    }

    #[test]
    fn a_rectangle_excludes_its_right_and_bottom_edges() {
        let rect = Rect::new(10, 10, 4, 4);
        assert!(rect.contains(10, 10));
        assert!(rect.contains(13, 13));
        assert!(!rect.contains(14, 10), "the right edge belongs to nobody");
        assert!(!rect.contains(10, 14), "the bottom edge belongs to nobody");
        assert!(!rect.contains(9, 10));
    }

    #[test]
    fn inset_shrinks_from_every_side() {
        let rect = Rect::new(0, 0, 20, 10).inset(2);
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (2, 2, 16, 6));
    }

    #[test]
    fn a_button_fires_on_the_release_that_lands_inside() {
        let mut button = Button::new(Rect::new(0, 0, 10, 10), "Save");
        assert!(!button.on_pointer(5, 5, true), "a press only arms it");
        assert!(button.is_down());
        assert!(button.on_pointer(5, 5, false), "the release inside fires");
        assert!(!button.is_down());
    }

    #[test]
    fn dragging_off_a_held_button_cancels_it() {
        let mut button = Button::new(Rect::new(0, 0, 10, 10), "Save");
        assert!(!button.on_pointer(5, 5, true));
        assert!(
            !button.on_pointer(50, 50, true),
            "leaving cancels the press"
        );
        assert!(
            !button.on_pointer(5, 5, false),
            "so the release does nothing"
        );
    }

    #[test]
    fn a_press_outside_never_arms_the_button() {
        let mut button = Button::new(Rect::new(0, 0, 10, 10), "Save");
        assert!(!button.on_pointer(50, 50, true));
        assert!(!button.on_pointer(5, 5, false));
    }

    #[test]
    fn a_text_box_edits_within_its_buffer() {
        let mut bytes = [0u8; 8];
        let mut field = TextBox::new(Rect::new(0, 0, 100, 20));
        assert!(field.insert(&mut bytes, b'a'));
        assert!(field.insert(&mut bytes, b'b'));
        assert_eq!(field.text(&bytes), b"ab");
        assert!(field.backspace(&mut bytes));
        assert_eq!(field.text(&bytes), b"a");
        assert_eq!(field.cursor(), 1);
    }

    #[test]
    fn a_full_text_box_refuses_rather_than_discarding() {
        let mut bytes = [0u8; 2];
        let mut field = TextBox::new(Rect::new(0, 0, 100, 20));
        assert!(field.insert(&mut bytes, b'a'));
        assert!(field.insert(&mut bytes, b'b'));
        assert!(!field.insert(&mut bytes, b'c'), "the buffer is full");
        assert_eq!(field.text(&bytes), b"ab");
    }

    #[test]
    fn loading_text_moves_the_cursor_to_the_end() {
        let mut bytes = [0u8; 16];
        let mut field = TextBox::new(Rect::new(0, 0, 100, 20));
        field.set_text(&mut bytes, b"hello");
        assert_eq!(field.text(&bytes), b"hello");
        assert_eq!(field.cursor(), 5);
        field.clear();
        assert!(field.is_empty());
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn a_text_box_paints_a_background_behind_every_glyph() {
        // A field redrawn over an old one must not let the old pixels show
        // through the clear bits of the new glyphs. Every pixel inside the
        // field is repainted — background or ink — so nothing of what was
        // underneath survives.
        let mut bytes = [0u8; 8];
        let mut field = TextBox::new(Rect::new(0, 0, 40, 24));
        field.set_text(&mut bytes, b"M");

        let stale = colour(7, 7, 7);
        let mut pixels = [stale; 40 * 24];
        let mut drawing = canvas(&mut pixels, 40, 24);
        field.draw(&mut drawing, &bytes);

        let inner = field.rect.inset(4);
        let survivors = pixels
            .iter()
            .enumerate()
            .filter(|(index, word)| {
                let (x, y) = (index % 40, index / 40);
                inner.contains(x as i32, y as i32) && **word == stale
            })
            .count();
        assert_eq!(
            survivors, 0,
            "pixels the field did not repaint would show the old window through it"
        );
    }

    #[test]
    fn a_canvas_clips_a_rectangle_that_runs_off_the_edge() {
        // Clipping is what stops a widget that overran its own window from
        // scribbling over the pixels beside the buffer — a bug no compositor
        // can see, because it happens inside one window's own memory.
        let mut pixels = [0u32; 4 * 4];
        let mut drawing = canvas(&mut pixels, 4, 4);
        drawing.clear(colour(1, 2, 3));
        drawing.fill_rect(Rect::new(-2, 1, 9, 1), colour(9, 9, 9));

        assert_eq!(pixel(&pixels, 4, 0, 0), colour(1, 2, 3));
        assert_eq!(
            pixel(&pixels, 4, 0, 1),
            colour(9, 9, 9),
            "the part inside the surface is drawn"
        );
        assert_eq!(
            pixel(&pixels, 4, 3, 2),
            colour(1, 2, 3),
            "the row below is untouched"
        );
    }

    #[test]
    fn a_newline_starts_a_new_row_instead_of_a_blank_one() {
        // A field four glyphs wide: its inner area is 32x40, so two rows of
        // sixteen pixels each, at y 4..20 and y 20..36.
        let mut bytes = [0u8; 16];
        let mut field = TextBox::new(Rect::new(0, 0, 40, 48));
        field.set_text(&mut bytes, b"abcd\nx");

        let mut pixels = [colour(1, 1, 1); 40 * 48];
        let mut drawing = canvas(&mut pixels, 40, 48);
        field.draw(&mut drawing, &bytes);

        assert!(ink(&pixels, 40, 4, 20) > 0, "the first row holds abcd");
        assert!(
            ink(&pixels, 40, 20, 36) > 0,
            "the newline opened the second row for x, rather than leaving a \
             blank one in between"
        );
    }

    #[test]
    fn the_caret_sits_on_the_row_the_cursor_is_in() {
        // The cursor is past the newline of a line that exactly filled the
        // row. The caret belongs at the start of the next row, and since
        // that row is empty the count of ink there is exactly the caret's
        // own sixteen pixels — nothing else could be mistaken for it.
        let mut bytes = [0u8; 16];
        let mut field = TextBox::new(Rect::new(0, 0, 40, 48));
        field.set_text(&mut bytes, b"abcd\n");
        field.focused = true;

        let mut pixels = [colour(1, 1, 1); 40 * 48];
        let mut drawing = canvas(&mut pixels, 40, 48);
        field.draw(&mut drawing, &bytes);

        assert_eq!(
            ink(&pixels, 40, 20, 36),
            16,
            "a one-pixel caret, sixteen rows tall, on the second row"
        );
    }

    // --- The EDE title bar ------------------------------------------------

    #[test]
    fn titlebar_buttons_hit_where_they_are_drawn() {
        // A 100px-wide bar: buttons are 18px each with a 2px right margin.
        // close [80,98), maximize [62,80), iconify [44,62), vertically 2..20.
        use super::{TitlebarHit, titlebar_hit};
        assert_eq!(titlebar_hit(90, 10, 100), TitlebarHit::Close);
        assert_eq!(titlebar_hit(70, 10, 100), TitlebarHit::Maximize);
        assert_eq!(titlebar_hit(50, 10, 100), TitlebarHit::Iconify);
        assert_eq!(
            titlebar_hit(30, 10, 100),
            TitlebarHit::None,
            "left of the buttons"
        );
        assert_eq!(
            titlebar_hit(99, 10, 100),
            TitlebarHit::None,
            "the right margin"
        );
        assert_eq!(
            titlebar_hit(90, 1, 100),
            TitlebarHit::None,
            "above the buttons"
        );
        assert_eq!(
            titlebar_hit(90, 21, 100),
            TitlebarHit::None,
            "below the buttons"
        );
        assert_eq!(titlebar_hit(-1, 10, 100), TitlebarHit::None);
    }

    #[test]
    fn the_titlebar_painting_matches_the_theme() {
        use super::{TITLEBAR_HEIGHT, draw_titlebar};
        let mut pixels = [0u32; 100 * 24];
        {
            let mut canvas = canvas(&mut pixels, 100, 24);
            draw_titlebar(&mut canvas, 100, "Terminal", true);
        }
        // The focused bar is EDE's steel blue, edge to edge.
        assert_eq!(pixel(&pixels, 100, 4, 10), palette::CHROME);
        assert_eq!(pixel(&pixels, 100, 40, 10), palette::CHROME);
        // The three button faces are the theme's light gray.
        assert_eq!(pixel(&pixels, 100, 90, 10), palette::BUTTON_FACE);
        assert_eq!(pixel(&pixels, 100, 70, 10), palette::BUTTON_FACE);
        // Unfocused: the warm gray instead.
        let mut gray = [0u32; 100 * 24];
        {
            let mut canvas = canvas(&mut gray, 100, 24);
            draw_titlebar(&mut canvas, 100, "Terminal", false);
        }
        assert_eq!(pixel(&gray, 100, 40, 10), palette::TITLE_UNFOCUS);
        // The bar is exactly TITLEBAR_HEIGHT tall: row below is untouched.
        assert_eq!(pixel(&gray, 100, 40, TITLEBAR_HEIGHT as usize), 0);
    }

    #[test]
    fn the_palette_is_edes_own() {
        // Pinned to the assets EDE ships (pekwm `default` theme), so a
        // future "cleanup" cannot quietly drift the desktop away from
        // EDE's colours.
        assert_eq!(palette::CHROME, colour(75, 105, 131)); // title.png
        assert_eq!(palette::TITLE_UNFOCUS, colour(234, 232, 227)); // title-unfocus.png
        assert_eq!(palette::BORDER, colour(198, 198, 198)); // border.png
        assert_eq!(palette::BUTTON_FACE, colour(230, 230, 230)); // close.png
        assert_eq!(palette::MENU_HEADER, colour(127, 158, 188)); // top-menu.png
        assert_eq!(palette::MENU_FACE, colour(251, 251, 251)); // bottom-menu.png
    }
}
