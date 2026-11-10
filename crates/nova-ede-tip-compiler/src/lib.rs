//! Rust port of `ede-tip-compiler` — the `strfile`-derived compiler
//! behind EDE's tips.
//!
//! Upstream compiles a fortune-format file (tips separated by a line
//! holding a single `%`) into a binary index: a 24-byte header, a
//! big-endian table of seek offsets, then the NUL-terminated strings.
//! The reader (`ede-tip`) mmaps that file and picks a tip at random.
//!
//! This crate is both halves: [`compile`] writes the format, and
//! [`StrFile`] reads it back, so the round trip is testable without a
//! filesystem. The on-disk layout, byte for byte:
//!
//! ```text
//! offset  size  field
//! 0       4     version (2), big-endian
//! 4       4     number of strings, big-endian
//! 8       4     longest string length, big-endian
//! 12      4     shortest string length, big-endian
//! 16      4     flags, big-endian (STR_RANDOM | STR_ORDERED | STR_ROTATED)
//! 20      4     delimiter byte ('%') + 3 pad bytes
//! 24      4·(n+1)  string offsets, big-endian, first = 24 + 4·(n+1)
//! …             strings, each NUL-terminated
//! ```
//!
//! Upstream's `-x` rot13 flag was set in one place and tested in
//! another, so compiled tips never actually got rotated; here the flag
//! and the behaviour agree.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

/// The format version this crate writes and accepts.
pub const VERSION: u32 = 2;
/// The header size in bytes.
pub const HEADER_SIZE: usize = 24;
/// Randomized offset order in the reader.
pub const STR_RANDOM: u32 = 0x1;
/// Alphabetically ordered offsets.
pub const STR_ORDERED: u32 = 0x2;
/// The strings are rot13'd.
pub const STR_ROTATED: u32 = 0x4;
/// The delimiter between tips.
pub const DELIMITER: u8 = b'%';
/// How many tips one compiled file may hold.
pub const MAX_TIPS: usize = 64;
/// The longest single tip the compiler keeps.
pub const MAX_TIP: usize = 192;

/// A compiled tips file: header facts plus the offsets.
#[derive(Clone, Copy, Debug)]
pub struct StrFile {
    /// Number of strings in the file.
    pub numstr: u32,
    /// Length of the longest string.
    pub longlen: u32,
    /// Length of the shortest string.
    pub shortlen: u32,
    /// The STR_* flags.
    pub flags: u32,
    /// The delimiter byte.
    pub delim: u8,
    /// The string offsets, big-endian, as stored.
    offsets: [u32; MAX_TIPS + 1],
    /// The whole file, for reading strings out of.
    bytes: &'static [u8],
    /// The byte where the strings begin.
    strings_start: usize,
}

impl StrFile {
    /// Open a compiled file. Only version 2 with sane counts parses;
    /// everything else is `None`.
    pub fn open(bytes: &'static [u8]) -> Option<Self> {
        if bytes.len() < HEADER_SIZE {
            return None;
        }
        let version = be_u32(bytes, 0)?;
        if version != VERSION {
            return None;
        }
        let numstr = be_u32(bytes, 4)?;
        if numstr as usize > MAX_TIPS {
            return None;
        }
        let longlen = be_u32(bytes, 8)?;
        let shortlen = be_u32(bytes, 12)?;
        let flags = be_u32(bytes, 16)?;
        let delim = bytes[20];
        let count = numstr as usize + 1;
        if bytes.len() < HEADER_SIZE + count * 4 {
            return None;
        }
        let mut offsets = [0u32; MAX_TIPS + 1];
        for (index, slot) in offsets.iter_mut().enumerate().take(count) {
            *slot = be_u32(bytes, HEADER_SIZE + index * 4)?;
        }
        let strings_start = HEADER_SIZE + count * 4;
        // Offsets must be monotonic and inside the file.
        for index in 1..count {
            if offsets[index] < offsets[index - 1] {
                return None;
            }
        }
        if offsets[numstr as usize] as usize > bytes.len() {
            return None;
        }
        Some(Self {
            numstr,
            longlen,
            shortlen,
            flags,
            delim,
            offsets,
            bytes,
            strings_start,
        })
    }

    /// The tip at `index` (0-based), with trailing whitespace trimmed.
    pub fn tip(&self, index: usize) -> Option<&str> {
        if index >= self.numstr as usize {
            return None;
        }
        let start = self.offsets[index] as usize;
        let end = self.offsets[index + 1] as usize;
        if start < self.strings_start || end > self.bytes.len() || start > end {
            return None;
        }
        let raw = &self.bytes[start..end];
        // The stored range ends at the next string's start; trim the
        // NUL terminator and any trailing newlines.
        let end_trimmed = raw
            .iter()
            .rposition(|&byte| byte != 0 && byte != b'\n')
            .map(|position| position + 1)
            .unwrap_or(0);
        core::str::from_utf8(&raw[..end_trimmed]).ok()
    }

    /// How many tips the file holds.
    pub const fn count(&self) -> usize {
        self.numstr as usize
    }

    /// The flags as booleans.
    pub const fn is_random(&self) -> bool {
        self.flags & STR_RANDOM != 0
    }

    /// Whether the offsets were written in alphabetical order.
    pub const fn is_ordered(&self) -> bool {
        self.flags & STR_ORDERED != 0
    }

    /// Whether the strings are rot13'd; the reader un-rotates.
    pub const fn is_rotated(&self) -> bool {
        self.flags & STR_ROTATED != 0
    }

    /// The tip at `index`, un-rotating into `scratch` when the file
    /// says the strings are rot13'd. Returns `None` when the tip is
    /// missing or `scratch` is too small for the decoded text.
    pub fn tip_decoded<'a>(&'a self, index: usize, scratch: &'a mut [u8]) -> Option<&'a str> {
        let text = self.tip(index)?;
        if !self.is_rotated() {
            return Some(text);
        }
        let bytes = text.as_bytes();
        if bytes.len() > scratch.len() {
            return None;
        }
        scratch[..bytes.len()].copy_from_slice(bytes);
        for byte in scratch[..bytes.len()].iter_mut() {
            *byte = match *byte {
                b'a'..=b'z' => b'a' + (*byte - b'a' + 13) % 26,
                b'A'..=b'Z' => b'A' + (*byte - b'A' + 13) % 26,
                other => other,
            };
        }
        core::str::from_utf8(&scratch[..bytes.len()]).ok()
    }
}

/// Write a compiled file from fortune-format `source` into `out`,
/// returning the bytes written. Returns `None` when `out` is too small
/// or the source holds no tips.
///
/// `flags` selects `STR_RANDOM`/`STR_ORDERED`/`STR_ROTATED`; the
/// delimiter is always `%`.
pub fn compile(source: &[u8], flags: u32, out: &mut [u8]) -> Option<usize> {
    // Collect the tips as slices of `source`.
    let mut tips: [[u8; MAX_TIP]; MAX_TIPS] = [[0; MAX_TIP]; MAX_TIPS];
    let mut lens = [0usize; MAX_TIPS];
    let mut count = 0usize;
    for chunk in SplitIter::new(source) {
        let trimmed = trim(chunk);
        if trimmed.is_empty() {
            continue;
        }
        if count >= MAX_TIPS {
            break;
        }
        let take = trimmed.len().min(MAX_TIP);
        tips[count][..take].copy_from_slice(&trimmed[..take]);
        lens[count] = take;
        count += 1;
    }
    if count == 0 {
        return None;
    }

    // The compiler rotates on write; do it in place before ordering so
    // the stored order is alphabetical over the stored bytes.
    if flags & STR_ROTATED != 0 {
        for index in 0..count {
            for byte in tips[index][..lens[index]].iter_mut() {
                *byte = match *byte {
                    b'a'..=b'z' => b'a' + (*byte - b'a' + 13) % 26,
                    b'A'..=b'Z' => b'A' + (*byte - b'A' + 13) % 26,
                    other => other,
                };
            }
        }
    }

    // Optionally order the strings alphabetically.
    if flags & STR_ORDERED != 0 {
        for i in 1..count {
            let mut j = i;
            while j > 0 && tips[j][..lens[j]] < tips[j - 1][..lens[j - 1]] {
                tips.swap(j, j - 1);
                lens.swap(j, j - 1);
                j -= 1;
            }
        }
    }

    // Header + offsets + strings.
    let offsets_start = HEADER_SIZE;
    let strings_start = offsets_start + (count + 1) * 4;
    let mut end = strings_start;
    for &len in lens[..count].iter() {
        end += len + 1; // + NUL
    }
    if out.len() < end {
        return None;
    }

    let longlen = lens[..count].iter().copied().max().unwrap_or(0) as u32;
    let shortlen = lens[..count].iter().copied().min().unwrap_or(0) as u32;

    be_store(out, 0, VERSION);
    be_store(out, 4, count as u32);
    be_store(out, 8, longlen);
    be_store(out, 12, shortlen);
    be_store(out, 16, flags);
    out[20] = DELIMITER;
    out[21] = 0;
    out[22] = 0;
    out[23] = 0;

    let mut offset = strings_start as u32;
    for (index, &len) in lens[..count].iter().enumerate() {
        be_store(out, offsets_start + index * 4, offset);
        offset += len as u32 + 1;
    }
    // The sentinel: one past the last string.
    be_store(out, offsets_start + count * 4, offset);

    let mut cursor = strings_start;
    for index in 0..count {
        let take = lens[index];
        out[cursor..cursor + take].copy_from_slice(&tips[index][..take]);
        cursor += take;
        out[cursor] = 0;
        cursor += 1;
    }
    Some(end)
}

/// Split fortune text on the `%`-line separator.
struct SplitIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> SplitIter<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
}

impl<'a> Iterator for SplitIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.bytes.len() {
            return None;
        }
        let mut index = self.pos;
        while index + 1 < self.bytes.len() {
            if self.bytes[index] == DELIMITER && self.bytes[index + 1] == b'\n' {
                let chunk = &self.bytes[self.pos..index];
                self.pos = index + 2;
                return Some(chunk);
            }
            index += 1;
        }
        // A trailing delimiter with no newline still terminates.
        if self.bytes[self.bytes.len() - 1] == DELIMITER {
            let chunk = &self.bytes[self.pos..self.bytes.len() - 1];
            self.pos = self.bytes.len();
            return Some(chunk);
        }
        let chunk = &self.bytes[self.pos..];
        self.pos = self.bytes.len();
        Some(chunk)
    }
}

/// Trim leading/trailing whitespace and newlines.
fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

/// Read a big-endian u32.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Store a big-endian u32.
fn be_store(out: &mut [u8], at: usize, value: u32) {
    if at + 4 <= out.len() {
        out[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;
    use std::vec;

    const BOOK: &[u8] = b"Welcome to NovaOS EDE.\n%\n\
Did you know? The comet is drawn, not an image.\n%\n\
Press the Nova button for applications.\n%\n";

    /// Compile a book and open it from a leaked box — the reader wants
    /// a `'static` slice, which the test stands in for with a leak.
    fn compiled(source: &[u8], flags: u32) -> &'static mut [u8] {
        let mut buffer = vec![0u8; 4096];
        let length = compile(source, flags, &mut buffer).unwrap();
        buffer.truncate(length);
        Box::leak(buffer.into_boxed_slice())
    }

    #[test]
    fn the_header_is_big_endian_version_2() {
        let file = compiled(BOOK, 0);
        // version 2, three strings.
        assert_eq!(&file[0..4], &[0, 0, 0, 2]);
        assert_eq!(&file[4..8], &[0, 0, 0, 3]);
        // flags zero.
        assert_eq!(&file[16..20], &[0, 0, 0, 0]);
        // delimiter '%' and three pad bytes.
        assert_eq!(file[20], b'%');
    }

    #[test]
    fn round_trip_through_the_index() {
        let file = compiled(BOOK, 0);
        let strfile = StrFile::open(file).unwrap();
        assert_eq!(strfile.count(), 3);
        assert_eq!(strfile.tip(0), Some("Welcome to NovaOS EDE."));
        assert_eq!(
            strfile.tip(1),
            Some("Did you know? The comet is drawn, not an image.")
        );
        assert_eq!(
            strfile.tip(2),
            Some("Press the Nova button for applications.")
        );
        assert_eq!(strfile.tip(3), None);
    }

    #[test]
    fn the_offsets_table_is_big_endian() {
        let file = compiled(BOOK, 0);
        let strfile = StrFile::open(file).unwrap();
        let first_offset = strfile.offsets[0] as usize;
        assert_eq!(
            first_offset,
            HEADER_SIZE + 4 * 4,
            "three tips, four offsets"
        );
        // Big-endian on disk: the stored first offset bytes.
        assert_eq!(&file[24..28], &(first_offset as u32).to_be_bytes());
    }

    #[test]
    fn ordering_sorts_the_strings() {
        let file = compiled(BOOK, STR_ORDERED);
        let strfile = StrFile::open(file).unwrap();
        assert!(strfile.is_ordered());
        let first = strfile.tip(0).unwrap();
        let second = strfile.tip(1).unwrap();
        assert!(
            first <= second,
            "ordered file starts with the alphabetically first tip"
        );
    }

    #[test]
    fn the_random_flag_is_recorded() {
        let file = compiled(BOOK, STR_RANDOM);
        let strfile = StrFile::open(file).unwrap();
        assert!(strfile.is_random());
        assert!(!strfile.is_ordered());
    }

    #[test]
    fn long_and_short_lengths_are_recorded() {
        let file = compiled(BOOK, 0);
        let strfile = StrFile::open(file).unwrap();
        assert_eq!(strfile.longlen, 47, "the longest tip");
        assert_eq!(strfile.shortlen, 22, "the shortest tip");
    }

    #[test]
    fn rot13_round_trips_through_the_scratch_buffer() {
        let file = compiled(BOOK, STR_ROTATED);
        let strfile = StrFile::open(file).unwrap();
        assert!(strfile.is_rotated());
        // The stored bytes are rotated: the first tip no longer reads
        // as the original.
        assert_ne!(strfile.tip(0), Some("Welcome to NovaOS EDE."));
        let mut scratch = [0u8; MAX_TIP];
        assert_eq!(
            strfile.tip_decoded(0, &mut scratch),
            Some("Welcome to NovaOS EDE."),
            "decoded back to the original"
        );
        // And a non-rotated file returns its tip untouched.
        let plain = compiled(BOOK, 0);
        let plain = StrFile::open(plain).unwrap();
        assert_eq!(
            plain.tip_decoded(0, &mut scratch),
            Some("Welcome to NovaOS EDE.")
        );
    }

    #[test]
    fn an_empty_source_produces_nothing() {
        let mut buffer = [0u8; 64];
        assert!(compile(b"", 0, &mut buffer).is_none());
        assert!(compile(b"%\n%\n", 0, &mut buffer).is_none());
    }

    #[test]
    fn a_small_buffer_is_rejected() {
        let mut buffer = [0u8; 8];
        assert!(compile(BOOK, 0, &mut buffer).is_none());
    }

    #[test]
    fn a_wrong_version_is_rejected() {
        let file = compiled(BOOK, 0);
        let mut tampered = file.to_vec();
        tampered[3] = 9; // version 9
        let tampered: &'static [u8] = Box::leak(tampered.into_boxed_slice());
        assert!(StrFile::open(tampered).is_none());
    }

    #[test]
    fn truncated_offsets_are_rejected() {
        let file = compiled(BOOK, 0);
        let truncated: &'static [u8] =
            Box::leak(file[..HEADER_SIZE + 4].to_vec().into_boxed_slice());
        assert!(StrFile::open(truncated).is_none());
    }

    #[test]
    fn offsets_must_be_monotonic() {
        // version 2, one string, but the second offset goes backwards.
        let mut file = vec![0u8; HEADER_SIZE + 8 + 2];
        file[3] = 2;
        file[7] = 1;
        // offsets at 24 and 28: first = 34, second = 30 (backwards).
        file[24..28].copy_from_slice(&34u32.to_be_bytes());
        file[28..32].copy_from_slice(&30u32.to_be_bytes());
        let file: &'static [u8] = Box::leak(file.into_boxed_slice());
        assert!(StrFile::open(file).is_none());
    }
}
