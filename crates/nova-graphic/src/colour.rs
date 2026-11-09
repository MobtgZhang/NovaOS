//! Colour constants mirroring a Qt6-ish light-blue/grey palette.

/// A 32-bit XRGB8888 colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Colour(pub u32);

impl Colour {
    pub const TRANSPARENT: Colour = Colour(0);
    pub const WHITE: Colour = Colour(0x00FFFFFF);
    pub const BLACK: Colour = Colour(0x00000000);
    pub const PANEL: Colour = Colour(0x002E3440);
    pub const PANEL_LIGHT: Colour = Colour(0x003E4452);
    pub const BORDER: Colour = Colour(0x005D6478);
    pub const HIGHLIGHT: Colour = Colour(0x0068AAC4);
    pub const HIGHLIGHT_DARK: Colour = Colour(0x004A8A9A);
    pub const TEXT: Colour = Colour(0x00D9DEE8);
    pub const TEXT_MUTED: Colour = Colour(0x008A93A4);
    pub const ERROR: Colour = Colour(0x00D65745);
    pub const SUCCESS: Colour = Colour(0x00698A3F);
}

impl From<Colour> for u32 {
    fn from(c: Colour) -> u32 {
        c.0
    }
}
