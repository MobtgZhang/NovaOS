//! PS/2 set-1 scancodes turned into characters and editing keys.
//!
//! # What the application actually receives
//!
//! The kernel's PS/2 driver strips bit 7 — the break bit — and sends the low
//! seven bits as the scancode with a separate state, so an application sees
//! `scancode` and `state` rather than a raw byte stream. Two consequences
//! shape this module:
//!
//! - A release is an ordinary event, not a byte to filter out. Shift must be
//!   *un*set on release, or the second word a user types is capitalised
//!   forever.
//! - The `E0` extended prefix arrives as its own event, already stripped to
//!   `0x60` and already marked as a release. Set 1 has no key with code
//!   `0x60`, so that event is recognisable as the prefix rather than as a
//!   keystroke, and the decoder holds it across events until the real code
//!   follows.
//!
//! # State lives here, not in the caller
//!
//! Shift, caps lock and the extended prefix are properties of the keyboard,
//! not of any one window. Keeping them in the [`Decoder`] means a program
//! feeds it events and gets back keys, with no modifier bookkeeping in the
//! application — and it means the state survives the window losing focus,
//! which is what a user expects from caps lock.

use nova_protocols::input::{INPUT_EVENT_KEY, KeyState};

/// A decoded keystroke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable ASCII byte.
    Char(u8),
    /// Backspace.
    Backspace,
    /// Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Delete.
    Delete,
    /// Home.
    Home,
    /// End.
    End,
}

impl Key {
    /// The byte this key types, if it types one.
    pub const fn char_of(self) -> Option<u8> {
        match self {
            Key::Char(byte) => Some(byte),
            _ => None,
        }
    }
}

/// No key at this position in a scancode table.
const NONE: u8 = 0xFF;

/// The `E0` prefix, as the driver delivers it: `0xE0` with the break bit set.
///
/// Set 1 assigns no key that code, so seeing it means "the next event belongs
/// to the extended block" and nothing else.
const EXTENDED_PREFIX: u16 = 0x60;

/// Unshifted characters for the main block, indexed by scancode.
///
/// `NONE` marks the positions that are not characters: modifiers, locks, and
/// the function keys this milestone does not use. They are still *handled* —
/// the decoder watches them for their state — they simply produce no output.
const BASE: [u8; 0x40] = [
    // 0x00
    NONE, 0x01, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x0E, 0x0F,
    // 0x10
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', 0x1C, NONE, b'a', b's',
    // 0x20
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0x2A, b'\\', b'z', b'x', b'c',
    b'v', // 0x30
    b'b', b'n', b'm', b',', b'.', b'/', 0x36, b'*', 0x38, b' ', 0x3A, NONE, NONE, NONE, NONE, NONE,
];

/// The position of the left shift key.
const LSHIFT: u16 = 0x2A;
/// The position of the right shift key.
const RSHIFT: u16 = 0x36;
/// The position of the left control key.
const CTRL: u16 = 0x1D;
/// The position of the left alt key.
const ALT: u16 = 0x38;
/// The position of the caps lock key.
const CAPS: u16 = 0x3A;

/// Characters the shifted and unshifted tables disagree about.
///
/// Letters are not listed: caps lock and shift both flip their case, and
/// that is decided once in [`Decoder::character`] rather than twice in two
/// tables that could drift apart.
const fn shifted(byte: u8) -> u8 {
    match byte {
        b'1' => b'!',
        b'2' => b'@',
        b'3' => b'#',
        b'4' => b'$',
        b'5' => b'%',
        b'6' => b'^',
        b'7' => b'&',
        b'8' => b'*',
        b'9' => b'(',
        b'0' => b')',
        b'-' => b'_',
        b'=' => b'+',
        b'[' => b'{',
        b']' => b'}',
        b';' => b':',
        b'\'' => b'"',
        b'`' => b'~',
        b'\\' => b'|',
        b',' => b'<',
        b'.' => b'>',
        b'/' => b'?',
        other => other,
    }
}

/// The keyboard's modifier and prefix state.
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    shift: bool,
    caps: bool,
    ctrl: bool,
    alt: bool,
    /// An `E0` prefix has arrived and its key has not yet.
    extended: bool,
}

impl Decoder {
    /// A keyboard with nothing held down.
    pub const fn new() -> Self {
        Self {
            shift: false,
            caps: false,
            ctrl: false,
            alt: false,
            extended: false,
        }
    }
}

impl Default for Decoder {
    /// A keyboard with nothing held down.
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// Whether shift is currently held.
    pub const fn shift(&self) -> bool {
        self.shift
    }

    /// Whether caps lock is on.
    pub const fn caps(&self) -> bool {
        self.caps
    }

    /// Whether control is held.
    pub const fn ctrl(&self) -> bool {
        self.ctrl
    }

    /// Whether alt is held.
    pub const fn alt(&self) -> bool {
        self.alt
    }

    /// Feed one scancode and get back the key it means, if any.
    ///
    /// `pressed` is the event's state. Releases are consumed here rather than
    /// filtered by the caller, because a release is what clears a modifier —
    /// and because a caller that forgot to filter them would see every
    /// keystroke twice.
    ///
    /// Only a *press* produces a [`Key`]; a release produces `None` unless it
    /// is the prefix of an extended pair, which is absorbed here.
    pub fn on_scancode(&mut self, scancode: u16, pressed: bool) -> Option<Key> {
        // The prefix arrives as its own event. It never produces a key, and
        // it must not be mistaken for a keystroke: hold it for the next event
        // and let that one decide.
        if scancode == EXTENDED_PREFIX {
            self.extended = true;
            return None;
        }

        let extended = self.extended;
        // Whatever this event is, it closes the pair. An `E0` whose second
        // byte never arrives would otherwise leave the decoder interpreting
        // every later keystroke as an extended one.
        self.extended = false;

        if !pressed {
            self.release(scancode);
            return None;
        }

        if extended && let Some(key) = Self::extended_key(scancode) {
            return Some(key);
        }

        // Everything else takes the same path with or without a prefix:
        // right control and right alt are the same keys to a user as their
        // left-hand twins, and a prefix whose second byte never arrived must
        // not eat the keystroke that follows it.
        match scancode {
            LSHIFT | RSHIFT => {
                self.shift = true;
                None
            }
            CTRL => {
                self.ctrl = true;
                None
            }
            ALT => {
                self.alt = true;
                None
            }
            CAPS => {
                // Toggled on the press, not on the release: a caps lock that
                // flipped twice per keystroke would look like it did nothing.
                self.caps = !self.caps;
                None
            }
            _ => self.character(scancode),
        }
    }

    /// Feed one input-service event's fields and get back the key, if any.
    ///
    /// Only *key* events reach the decoder: pointer motion carries a `state`
    /// field too, and a caller that passed it through would let a mouse
    /// movement set Shift. What is filtered is the event's kind, not its
    /// state — a release has to be fed in, or Shift would never come back up
    /// and the `E0` prefix, which the driver delivers *as* a release, would
    /// never register and no arrow key would ever work. Releases simply
    /// produce no [`Key`], which is what [`Decoder::on_scancode`] already
    /// guarantees.
    ///
    /// A state that is neither press nor release is refused rather than
    /// guessed at: treating an unknown value as a press would type a
    /// keystroke the device never made.
    pub fn on_input(&mut self, kind: u32, state: u16, scancode: u16) -> Option<Key> {
        if kind != INPUT_EVENT_KEY {
            return None;
        }
        let pressed = match KeyState::from_u16(state) {
            Some(KeyState::Pressed) => true,
            Some(KeyState::Released) => false,
            None => return None,
        };
        self.on_scancode(scancode, pressed)
    }

    /// Apply a release to the modifier state.
    fn release(&mut self, scancode: u16) {
        match scancode {
            LSHIFT | RSHIFT => self.shift = false,
            CTRL => self.ctrl = false,
            ALT => self.alt = false,
            // Caps lock is a latch, not a hold: releasing it changes nothing.
            _ => {}
        }
    }

    /// The key an extended (`E0`) press names, or `None`.
    ///
    /// `None` means "not an extended key": the caller then decodes the
    /// scancode as an ordinary one. That is what makes right control and
    /// right alt behave like their left-hand twins, and what stops an
    /// orphaned prefix from swallowing the next keystroke.
    fn extended_key(scancode: u16) -> Option<Key> {
        // The extended block repeats the main block's codes for the keypad and
        // the navigation cluster; only the navigation keys matter to M6.
        match scancode {
            0x48 => Some(Key::Up),
            0x50 => Some(Key::Down),
            0x4B => Some(Key::Left),
            0x4D => Some(Key::Right),
            0x53 => Some(Key::Delete),
            0x47 => Some(Key::Home),
            0x4F => Some(Key::End),
            // Extended enter and keypad slash are the characters the main
            // block already produces, but they arrive with the prefix.
            0x1C => Some(Key::Enter),
            0x35 => Some(Key::Char(b'/')),
            _ => None,
        }
    }

    /// The character a main-block press types, or `None`.
    fn character(&self, scancode: u16) -> Option<Key> {
        let index = scancode as usize;
        let byte = *BASE.get(index)?;
        if byte == NONE {
            return None;
        }

        // Editing keys are not characters, and they are not affected by any
        // modifier: shift-backspace is still backspace.
        let editing = match byte {
            0x0E => Some(Key::Backspace),
            0x1C => Some(Key::Enter),
            0x01 => Some(Key::Escape),
            0x0F => Some(Key::Tab),
            _ => None,
        };
        if editing.is_some() {
            return editing;
        }

        Some(Key::Char(self.character_byte(byte)))
    }

    /// Case-fold one base character under the current modifiers.
    fn character_byte(&self, byte: u8) -> u8 {
        let letter = byte.is_ascii_lowercase();
        // Caps lock and shift are both case selectors, and using both at once
        // cancels out — which is the behaviour every keyboard has.
        let upper = if letter {
            self.shift != self.caps
        } else {
            self.shift
        };
        if upper {
            shifted(byte.to_ascii_uppercase())
        } else {
            byte
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoder, EXTENDED_PREFIX, Key};

    /// Feed a make/break pair and return what the make produced.
    fn tap(decoder: &mut Decoder, scancode: u16) -> Option<Key> {
        let made = decoder.on_scancode(scancode, true);
        let _broken = decoder.on_scancode(scancode, false);
        made
    }

    #[test]
    fn a_plain_press_types_its_letter() {
        let mut decoder = Decoder::new();
        assert_eq!(tap(&mut decoder, 0x1E), Some(Key::Char(b'a')));
        assert_eq!(tap(&mut decoder, 0x17), Some(Key::Char(b'i')));
        assert_eq!(tap(&mut decoder, 0x39), Some(Key::Char(b' ')));
    }

    #[test]
    fn shift_uppercases_and_releases_uncase() {
        let mut decoder = Decoder::new();
        assert!(
            decoder.on_scancode(0x2A, true).is_none(),
            "shift is not a key"
        );
        assert_eq!(decoder.on_scancode(0x1E, true), Some(Key::Char(b'A')));
        // The release must clear the modifier, or every later word is
        // capitalised.
        assert!(decoder.on_scancode(0x2A, false).is_none());
        assert_eq!(decoder.on_scancode(0x1E, true), Some(Key::Char(b'a')));
    }

    #[test]
    fn shift_turns_digits_into_their_symbols() {
        let mut decoder = Decoder::new();
        decoder.on_scancode(0x2A, true);
        assert_eq!(decoder.on_scancode(0x03, true), Some(Key::Char(b'@')));
        assert_eq!(decoder.on_scancode(0x0B, true), Some(Key::Char(b')')));
        // A symbol with no shifted form is unchanged rather than dropped.
        assert_eq!(decoder.on_scancode(0x10, true), Some(Key::Char(b'Q')));
    }

    #[test]
    fn caps_lock_latches_and_cancels_against_shift() {
        let mut decoder = Decoder::new();
        decoder.on_scancode(0x3A, true);
        decoder.on_scancode(0x3A, false);
        assert!(decoder.caps());
        assert_eq!(decoder.on_scancode(0x1E, true), Some(Key::Char(b'A')));

        // Caps lock does not touch digits.
        decoder.on_scancode(0x2A, true);
        assert_eq!(decoder.on_scancode(0x02, true), Some(Key::Char(b'!')));
        // Shift plus caps is lowercase for a letter.
        assert_eq!(decoder.on_scancode(0x1F, true), Some(Key::Char(b's')));
    }

    #[test]
    fn editing_keys_are_keys_not_characters() {
        let mut decoder = Decoder::new();
        assert_eq!(tap(&mut decoder, 0x0E), Some(Key::Backspace));
        assert_eq!(tap(&mut decoder, 0x1C), Some(Key::Enter));
        assert_eq!(tap(&mut decoder, 0x01), Some(Key::Escape));
        assert_eq!(tap(&mut decoder, 0x0F), Some(Key::Tab));
        // A modifier position has no character at all.
        assert_eq!(tap(&mut decoder, 0x2A), None);
        assert_eq!(tap(&mut decoder, 0x3B), None, "F1 is unmapped, not a box");
    }

    #[test]
    fn the_extended_prefix_is_absorbed_and_the_next_code_is_navigation() {
        let mut decoder = Decoder::new();
        // The prefix arrives as a stripped break byte; it must not be read as
        // a keystroke of its own.
        assert_eq!(decoder.on_scancode(EXTENDED_PREFIX, false), None);
        assert_eq!(decoder.on_scancode(0x48, true), Some(Key::Up));
        // The pair is closed: the next ordinary press is ordinary again.
        assert_eq!(decoder.on_scancode(0x4B, false), None);
        assert_eq!(tap(&mut decoder, 0x1E), Some(Key::Char(b'a')));
    }

    #[test]
    fn an_orphaned_prefix_does_not_corrupt_the_following_key() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.on_scancode(EXTENDED_PREFIX, false), None);
        // The second byte never arrives. The next key must still be itself.
        assert_eq!(tap(&mut decoder, 0x30), Some(Key::Char(b'b')));
    }

    #[test]
    fn extended_keys_reach_the_editing_keys() {
        let mut decoder = Decoder::new();
        for (scancode, expected) in [
            (0x4B, Key::Left),
            (0x4D, Key::Right),
            (0x50, Key::Down),
            (0x53, Key::Delete),
            (0x47, Key::Home),
            (0x4F, Key::End),
        ] {
            decoder.on_scancode(EXTENDED_PREFIX, false);
            assert_eq!(decoder.on_scancode(scancode, true), Some(expected));
            decoder.on_scancode(scancode, false);
        }
    }

    #[test]
    fn a_release_never_produces_a_key() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.on_scancode(0x1E, false), None);
        assert_eq!(tap(&mut decoder, 0x1E), Some(Key::Char(b'a')));
    }

    #[test]
    fn only_key_events_reach_the_decoder() {
        use nova_protocols::input::{INPUT_EVENT_KEY, INPUT_EVENT_POINTER, KeyState};

        let mut decoder = Decoder::new();

        // Pointer motion is not a keystroke, whatever its state claims: a
        // mouse event must not be able to press Shift.
        assert_eq!(
            decoder.on_input(INPUT_EVENT_POINTER, KeyState::Pressed.as_u16(), 0x2A),
            None
        );
        assert!(!decoder.shift(), "a mouse event set a modifier");

        // A release has to reach the decoder — it is what takes Shift back
        // down — but it never types anything.
        decoder.on_input(INPUT_EVENT_KEY, KeyState::Pressed.as_u16(), 0x2A);
        assert!(decoder.shift());
        assert_eq!(
            decoder.on_input(INPUT_EVENT_KEY, KeyState::Released.as_u16(), 0x2A),
            None
        );
        assert!(
            !decoder.shift(),
            "a release filtered out before the decoder would leave shift stuck"
        );

        // The `E0` prefix arrives as a release too, so filtering releases
        // would leave every arrow key waiting for a prefix that never came.
        assert_eq!(
            decoder.on_input(
                INPUT_EVENT_KEY,
                KeyState::Released.as_u16(),
                EXTENDED_PREFIX
            ),
            None
        );
        assert_eq!(
            decoder.on_input(INPUT_EVENT_KEY, KeyState::Pressed.as_u16(), 0x48),
            Some(Key::Up)
        );

        // A state that is neither press nor release is refused rather than
        // read as a press.
        assert_eq!(decoder.on_input(INPUT_EVENT_KEY, 7, 0x1E), None);

        // And a real press is the case that types.
        assert_eq!(
            decoder.on_input(INPUT_EVENT_KEY, KeyState::Pressed.as_u16(), 0x1E),
            Some(Key::Char(b'a')),
            "a real press is the one case that types"
        );
    }
}
