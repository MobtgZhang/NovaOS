//! A minimal kernel-side stdin byte ring.
//!
//! User-space input has two sources on NovaOS: the UART (the 16550 behind the
//! serial console), which is where a QEMU companion or a serial-term
//! connection reaches the machine, and the keyboard, through 8042 set-1
//! scancodes routed out of `drivers::input`. Both end up here as decoded
//! bytes, so `read(0)` from user space can pull either.
//!
//! # Why bytes, not scancodes
//!
//! `read(0)` is an old file descriptor, and every user-space process already
//! speaks bytes. Translating scancodes to ASCII is what the console needs to
//! feel like a terminal — the shell should not have to carry a scancode
//! table just to know the key under the letter key. Translation happens at
//! the moment the byte is written here, not when a reader asks for it.
//!
//! # Why a fixed size
//!
//! A ring of 64 bytes is enough for a fast typist to never lose a keystroke
//! at 100 WPM. A larger buffer would swallow backpressure; a smaller one
//! would drop keys in bursts. No allocator, no panic, no in-band flag.

use crate::sync::SpinLock;

const CAPACITY: usize = 64;

/// One fixed-size byte ring, one slot shorter than capacity.
struct Ring {
    bytes: [u8; CAPACITY],
    /// Where the next byte will be written.
    head: usize,
    /// Where the next byte will be read.
    tail: usize,
    /// How many bytes are ready to read (the difference between the two is
    /// kept explicit so a full ring and an empty ring are distinguishable).
    count: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Write one byte. Returns `false` when the ring is full and the byte is
    /// dropped. The caller does not block — that is the whole point: writer
    /// and reader never wait for each other, they only ever fall behind.
    fn write(&mut self, byte: u8) -> bool {
        if self.count == CAPACITY {
            return false;
        }
        self.bytes[self.head] = byte;
        self.head = (self.head + 1) % CAPACITY;
        self.count += 1;
        true
    }

    /// Read one byte, or `None` when the ring is empty.
    fn read(&mut self) -> Option<u8> {
        if self.count == 0 {
            return None;
        }
        let byte = self.bytes[self.tail];
        self.tail = (self.tail + 1) % CAPACITY;
        self.count -= 1;
        Some(byte)
    }
}

/// The one ring every reader shares.
static STDIN: SpinLock<Ring> = SpinLock::new(Ring::new());

/// Write one decoded byte to the ring.
///
/// # Safety
///
/// Must be called with interrupts masked (the call sites all are) and from
/// one context at a time — the 8042 poll is single-driver.
pub unsafe fn write_byte(byte: u8) {
    let mut ring = STDIN.lock();
    let _ = ring.write(byte);
}

/// Read one decoded byte, or `None` when nothing is pending.
///
/// # Safety
///
/// Same discipline as [`write_byte`].
pub unsafe fn read_byte() -> Option<u8> {
    let mut ring = STDIN.lock();
    ring.read()
}

/// PS/2 Keyboard Scan Code Set 1 → ASCII decoding.
///
/// This is the table a typical emergency console needs: every printable
/// character on the US layout, plus backspace, tab, enter and space. The
/// kernel is a terminal, not a keyboard-layout manager, so anything exotic
/// (OEM keys, media keys, numpad) is ignored on purpose.
///
/// # Why a plain array
///
/// The whole table is 256 entries, with folding for shift. A table lookup is
/// one instruction; a `match` that the compiler has not been able to collapse
/// into a jump table costs branch-inner-loops measurable on IPC traffic. The
/// table is `const`, so it lives in `.rodata`, not anywhere writable.
///
/// # What is *not* here
///
/// No dead-key composition, no numpad handling, no locale switching. US
/// layout only. If those arrive later they grow as their own tables.

/// The unshifted column, `0x00..=0x58`.
const UNSHIFTED: [u8; 58] = [
    0x1B, 0x00, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0x08,
    0x09, // 0x00..=0x0F
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', 0x0A, 0x00, b'a',
    b's', // 0x10..=0x1F
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0x00, b'\\', b'z', b'x', b'c',
    b'v', // 0x20..=0x2F
    b'b', b'n', b'm', b',', b'.', b'/', 0x00, b'*', 0x00, b' ', // 0x30..=0x39
];

/// The shifted column (LShift/RShift held), mirroring the same indices as
/// [`UNSHIFTED`]. Punctuation and digits change; letters become capital;
/// nothing reaches past ASCII.
const SHIFTED: [u8; 58] = [
    0x1B, 0x00, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0x08,
    0x00, // 0x00..=0x0F
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', 0x0A, 0x00, b'A',
    b'S', // 0x10..=0x1F
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0x00, b'|', b'Z', b'X', b'C',
    b'V', // 0x20..=0x2F
    b'B', b'N', b'M', b'<', b'>', b'?', 0x00, b'*', 0x00, b' ', // 0x30..=0x39
];

/// Scancodes that always shift (call them when their *press* arrives).
pub const LSHIFT: u8 = 0x2A;
pub const RSHIFT: u8 = 0x36;

/// Decode one set-1 press scancode to its ASCII byte, or `0` for a key that
/// does not map (modifier, numlock-only, media keys, ...).
///
/// The high bit (release) must have been stripped by the caller already.
#[inline]
pub const fn decode_press(scancode: u8, shifted: bool) -> u8 {
    let index = scancode as usize;
    if index >= UNSHIFTED.len() {
        return 0;
    }
    let table = if shifted { &SHIFTED } else { &UNSHIFTED };
    table[index]
}

/// A key is held if the byte had the high bit clear. The caller feeds bytes
/// straight from the 8042 output buffer here.
#[inline]
pub const fn is_press(byte: u8) -> bool {
    byte & 0x80 == 0
}

/// The low seven bits are the scancode. Caller strips the release bit.
#[inline]
pub const fn scancode_of(byte: u8) -> u8 {
    byte & 0x7F
}
