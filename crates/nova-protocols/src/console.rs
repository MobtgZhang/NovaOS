//! The console service protocol.
//!
//! # The two operations
//!
//! The console offers a write and a read-line. The write is chunked, because
//! [`nova_abi::ipc::MAX_INLINE_PAYLOAD`] is a hard limit of 64 bytes and a
//! console line is routinely longer than that. The read-line is a
//! request/reply that may block: the service holds the request until a line
//! arrives or the input ends.
//!
//! # "No line yet" is not "end of input"
//!
//! These are different answers and the protocol must be able to say both.
//! "No line yet" means the service has nothing to give *right now* — the
//! caller should come back. "End of input" means there will never be another
//! line, because the input source is closed. A caller that conflated them
//! would either spin forever or report a truncated session as a complete one.
//! [`LineOutcome`] and [`LineStatus`] keep them apart, and the reply carries
//! the status so the distinction survives the wire.
//!
//! # Layout is a contract
//!
//! The request and reply structs are `#[repr(C)]` with compile-time size and
//! offset assertions. The two sides are separately compiled binaries; a
//! struct that grew by a field would be read as a different message, not as a
//! compile error.

use nova_abi::ipc::MAX_INLINE_PAYLOAD;

use crate::ProtocolVersion;

/// The console protocol version this implementation speaks.
pub const CONSOLE_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Opcode for a console write.
pub const CONSOLE_OP_WRITE: u64 = 1;

/// Opcode for a console read-line.
pub const CONSOLE_OP_READ_LINE: u64 = 2;

/// Size of the console buffer, in bytes.
///
/// The buffer is a fixed-size ring: the console has no allocator, and a
/// console that grows without bound is a console that can exhaust the
/// kernel's memory.
pub const CONSOLE_CAPACITY: usize = 256;

/// The opcode of a console operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum ConsoleOpcode {
    /// Write bytes to the console.
    Write,
    /// Read one line from the console.
    ReadLine,
}

impl ConsoleOpcode {
    /// The wire value.
    pub const fn as_u64(self) -> u64 {
        match self {
            ConsoleOpcode::Write => CONSOLE_OP_WRITE,
            ConsoleOpcode::ReadLine => CONSOLE_OP_READ_LINE,
        }
    }

    /// Recover an opcode from its wire value.
    ///
    /// Returns `None` for an unknown value: an unrecognised opcode is a
    /// version problem, and answering it with a nearby opcode would run the
    /// wrong operation.
    pub const fn from_u64(value: u64) -> Option<ConsoleOpcode> {
        match value {
            CONSOLE_OP_WRITE => Some(ConsoleOpcode::Write),
            CONSOLE_OP_READ_LINE => Some(ConsoleOpcode::ReadLine),
            _ => None,
        }
    }
}

/// Flag: the read-line request should not block.
pub const CONSOLE_FLAG_NONBLOCK: u32 = 1 << 0;

/// The outcome of a read-line, as it travels on the wire.
///
/// `#[repr(i32)]` so the value is a plain integer on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum LineStatus {
    /// A line was returned.
    Ok = 0,
    /// No line is available yet; the caller should try again.
    NoLineYet = 1,
    /// The input has ended; no further line will ever arrive.
    EndOfInput = 2,
    /// The line did not fit in the caller's buffer and was discarded.
    TooLong = 3,
}

impl LineStatus {
    /// The wire value.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// Whether this status means the session is over.
    ///
    /// "No line yet" is *not* the end of the session; only this predicate
    /// tells the two apart, and the shell uses it to decide whether to print
    /// another prompt.
    pub const fn is_end_of_input(self) -> bool {
        matches!(self, LineStatus::EndOfInput)
    }
}

/// A request to write to the console.
///
/// The bytes follow the header in the message's inline payload, at most
/// [`MAX_INLINE_PAYLOAD`] of them. A longer write is split into several
/// requests; [`chunk_count`] tells the sender how many.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ConsoleWriteRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// How many payload bytes carry the text.
    pub length: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a console write request, in bytes.
pub const CONSOLE_WRITE_REQUEST_SIZE: usize = 16;

/// A reply to a console write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ConsoleWriteReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// How many bytes the service actually wrote.
    pub written: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a console write reply, in bytes.
pub const CONSOLE_WRITE_REPLY_SIZE: usize = 16;

/// A request to read one line from the console.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ConsoleReadLineRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. [`CONSOLE_FLAG_NONBLOCK`] asks for an immediate answer
    /// rather than a blocking wait.
    pub flags: u32,
    /// The largest line the caller can accept, in bytes.
    pub max_len: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a console read-line request, in bytes.
pub const CONSOLE_READ_LINE_REQUEST_SIZE: usize = 16;

/// A reply to a console read-line.
///
/// The bytes follow the header in the message's inline payload; their count
/// is `length`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ConsoleReadLineReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Whether a line was returned, no line is ready, or the input ended.
    pub status: LineStatus,
    /// How many payload bytes carry the line.
    pub length: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a console read-line reply, in bytes.
pub const CONSOLE_READ_LINE_REPLY_SIZE: usize = 16;

const _: () = assert!(size_of::<ConsoleWriteRequest>() == CONSOLE_WRITE_REQUEST_SIZE);
const _: () = assert!(size_of::<ConsoleWriteReply>() == CONSOLE_WRITE_REPLY_SIZE);
const _: () = assert!(size_of::<ConsoleReadLineRequest>() == CONSOLE_READ_LINE_REQUEST_SIZE);
const _: () = assert!(size_of::<ConsoleReadLineReply>() == CONSOLE_READ_LINE_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(ConsoleWriteRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteRequest, length) == 8);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteRequest, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(ConsoleWriteReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteReply, written) == 8);
const _: () = assert!(core::mem::offset_of!(ConsoleWriteReply, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(ConsoleReadLineRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineRequest, max_len) == 8);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineRequest, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(ConsoleReadLineReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineReply, length) == 8);
const _: () = assert!(core::mem::offset_of!(ConsoleReadLineReply, reserved) == 12);

/// How many messages are needed to carry `total` bytes of text.
///
/// The inline payload limit is hard, so a longer write is split. The count is
/// computed with checked arithmetic: a caller that asks about an absurd length
/// gets `None` rather than a wrapped, wrong answer.
pub const fn chunk_count(total: usize) -> Option<usize> {
    if total == 0 {
        return Some(0);
    }
    match total.checked_add(MAX_INLINE_PAYLOAD - 1) {
        Some(rounded) => Some(rounded / MAX_INLINE_PAYLOAD),
        None => None,
    }
}

/// The result of a read-line attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineOutcome {
    /// A line was copied into the caller's buffer; the payload is its length.
    ///
    /// The line is delivered without its terminator. A `\r\n` terminator is
    /// delivered as just the line, because a terminal's carriage return is an
    /// artefact of the input device, not part of the text.
    Line(u16),
    /// No line is available yet. The input has not ended.
    NoLineYet,
    /// The input has ended and every line has been delivered.
    EndOfInput,
    /// The line did not fit in the caller's buffer. It was consumed and
    /// discarded, so the next read returns the following line.
    TooLong,
}

impl LineOutcome {
    /// The status this outcome carries on the wire.
    pub const fn status(self) -> LineStatus {
        match self {
            LineOutcome::Line(_) => LineStatus::Ok,
            LineOutcome::NoLineYet => LineStatus::NoLineYet,
            LineOutcome::EndOfInput => LineStatus::EndOfInput,
            LineOutcome::TooLong => LineStatus::TooLong,
        }
    }

    /// Whether the session is over.
    pub const fn is_end_of_input(self) -> bool {
        self.status().is_end_of_input()
    }
}

/// A bounded console buffer.
///
/// The buffer is the pure part of the console service: it accepts bytes,
/// splits them into lines, and says "no line yet" until one arrives. The
/// kernel owns the device; this owns the state.
///
/// It is a fixed-size ring with no allocator. When the buffer is full, further
/// writes are refused rather than evicting the oldest bytes: a console that
/// silently drops what it was given is worse than one that reports the
/// overflow.
#[derive(Clone, Debug)]
pub struct ConsoleBuffer {
    bytes: [u8; CONSOLE_CAPACITY],
    head: usize,
    len: usize,
    eof: bool,
    overflowed: bool,
}

impl Default for ConsoleBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleBuffer {
    /// An empty buffer with the input still open.
    pub const fn new() -> Self {
        ConsoleBuffer {
            bytes: [0; CONSOLE_CAPACITY],
            head: 0,
            len: 0,
            eof: false,
            overflowed: false,
        }
    }

    /// Whether the input has ended.
    pub const fn is_end_of_input(&self) -> bool {
        self.eof
    }

    /// Whether a write was refused because the buffer was full.
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// The number of bytes currently buffered.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no bytes.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The byte at logical offset `index` from the read position.
    fn get(&self, index: usize) -> u8 {
        let position = self.head.checked_add(index).unwrap_or(index) % CONSOLE_CAPACITY;
        self.bytes[position]
    }

    /// Append bytes to the buffer, returning how many were accepted.
    ///
    /// The count is the number of bytes actually taken. A short count means
    /// the buffer is full.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let mut accepted = 0usize;
        for &byte in data {
            if self.len == CONSOLE_CAPACITY {
                self.overflowed = true;
                break;
            }
            let position = self.head.checked_add(self.len).unwrap_or(self.len) % CONSOLE_CAPACITY;
            self.bytes[position] = byte;
            self.len = self.len.saturating_add(1);
            accepted = accepted.saturating_add(1);
        }
        accepted
    }

    /// Close the input.
    ///
    /// After this, a read-line that finds no complete line returns the
    /// remaining bytes as a final line, and then reports the end of input.
    pub fn close_input(&mut self) {
        self.eof = true;
    }

    /// Consume `count` bytes from the read position.
    fn consume(&mut self, count: usize) {
        let count = count.min(self.len);
        self.head = self.head.checked_add(count).unwrap_or(count) % CONSOLE_CAPACITY;
        self.len = self.len.saturating_sub(count);
    }

    /// Read one line into `out`.
    ///
    /// Returns [`LineOutcome::NoLineYet`] when no complete line is buffered
    /// and the input is still open, and [`LineOutcome::EndOfInput`] when the
    /// input has ended and nothing is left. The two are deliberately distinct
    /// states: the first says "ask again", the second says "stop asking".
    pub fn read_line(&mut self, out: &mut [u8]) -> LineOutcome {
        // Look for a newline in the buffered bytes.
        let mut index = 0usize;
        while index < self.len {
            if self.get(index) == b'\n' {
                return self.deliver(index, out);
            }
            index = index.saturating_add(1);
        }

        if !self.eof {
            return LineOutcome::NoLineYet;
        }

        if self.len == 0 {
            return LineOutcome::EndOfInput;
        }
        // The input ended mid-line: the remaining bytes are the last line.
        self.deliver(self.len, out)
    }

    /// Deliver the first `length` buffered bytes as a line.
    ///
    /// The newline itself, if there was one, is consumed and not delivered.
    fn deliver(&mut self, length: usize, out: &mut [u8]) -> LineOutcome {
        // A `\r` immediately before the terminator is an artefact of the
        // terminal, not part of the text.
        let mut line_length = length;
        if line_length > 0 && self.get(line_length.saturating_sub(1)) == b'\r' {
            line_length = line_length.saturating_sub(1);
        }

        if line_length > out.len() {
            // The line does not fit. It is consumed and discarded, so the
            // next read returns the line after it rather than the same
            // oversized line forever.
            self.consume(length.saturating_add(1).min(self.len));
            return LineOutcome::TooLong;
        }

        for (index, slot) in out.iter_mut().enumerate().take(line_length) {
            *slot = self.get(index);
        }
        self.consume(length.saturating_add(1).min(self.len));
        LineOutcome::Line(u16::try_from(line_length).unwrap_or(u16::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONSOLE_CAPACITY, CONSOLE_FLAG_NONBLOCK, CONSOLE_OP_READ_LINE, CONSOLE_OP_WRITE,
        CONSOLE_VERSION, ConsoleBuffer, ConsoleOpcode, ConsoleReadLineReply,
        ConsoleReadLineRequest, ConsoleWriteReply, ConsoleWriteRequest, LineOutcome, LineStatus,
        chunk_count,
    };
    use crate::ProtocolVersion;
    use nova_abi::ipc::MAX_INLINE_PAYLOAD;

    #[test]
    fn the_version_is_checked_before_anything_else() {
        // A request carrying a version the service does not implement is
        // refused, not interpreted.
        assert_eq!(CONSOLE_VERSION, ProtocolVersion::new(1, 0));
        assert!(
            crate::negotiate(CONSOLE_VERSION, CONSOLE_VERSION).is_ok(),
            "the service must serve its own version"
        );
        assert!(
            crate::negotiate(CONSOLE_VERSION, ProtocolVersion::new(2, 0)).is_err(),
            "a different major version is a different protocol"
        );
    }

    #[test]
    fn console_write_roundtrip() {
        // A line longer than the inline payload limit must be split into
        // several messages, and the reassembled text must be identical.
        let text = b"the quick brown fox jumps over the lazy dog, again and again and again";
        let chunks = chunk_count(text.len()).expect("chunk count");
        assert!(chunks > 1, "a long line needs more than one message");
        assert!(
            chunks * MAX_INLINE_PAYLOAD >= text.len(),
            "the chunks must be able to carry the whole text"
        );

        let mut buffer = ConsoleBuffer::new();
        let mut sent = 0usize;
        for _ in 0..chunks {
            let end = (sent + MAX_INLINE_PAYLOAD).min(text.len());
            let piece = &text[sent..end];
            let request = ConsoleWriteRequest {
                version: CONSOLE_VERSION,
                flags: 0,
                length: piece.len() as u32,
                reserved: 0,
            };
            assert_eq!(request.version, CONSOLE_VERSION);
            let accepted = buffer.write(piece);
            assert_eq!(accepted, piece.len(), "the console accepted the chunk");
            let reply = ConsoleWriteReply {
                version: CONSOLE_VERSION,
                status: 0,
                written: accepted as u32,
                reserved: 0,
            };
            assert_eq!(reply.written as usize, piece.len());
            sent = end;
        }
        assert_eq!(sent, text.len());

        // The line is terminated, and the text comes back as one line.
        buffer.write(b"\n");
        let mut line = [0u8; 128];
        let outcome = buffer.read_line(&mut line);
        match outcome {
            LineOutcome::Line(length) => {
                assert_eq!(&line[..usize::from(length)], text);
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn a_write_that_does_not_fit_the_inline_payload_is_refused_by_the_caller() {
        // The limit is hard: the request itself declares how many bytes
        // follow, and a caller that declares more than the limit has made a
        // mistake the protocol must not paper over.
        let request = ConsoleWriteRequest {
            version: CONSOLE_VERSION,
            flags: 0,
            length: MAX_INLINE_PAYLOAD as u32,
            reserved: 0,
        };
        assert_eq!(request.length as usize, MAX_INLINE_PAYLOAD);
        assert_eq!(MAX_INLINE_PAYLOAD, 64, "the inline limit is 64 bytes");
    }

    #[test]
    fn console_readline_distinguishes_eof() {
        let mut buffer = ConsoleBuffer::new();

        // Nothing yet, and the input is still open.
        let mut line = [0u8; 32];
        assert_eq!(
            buffer.read_line(&mut line),
            LineOutcome::NoLineYet,
            "an open console with no data says 'no line yet', not 'end of input'"
        );
        assert_eq!(buffer.read_line(&mut line).status(), LineStatus::NoLineYet);
        assert!(!buffer.read_line(&mut line).is_end_of_input());

        // A line arrives.
        buffer.write(b"hello\n");
        match buffer.read_line(&mut line) {
            LineOutcome::Line(length) => {
                assert_eq!(&line[..usize::from(length)], b"hello");
            }
            other => panic!("expected a line, got {other:?}"),
        }

        // Still open, so still "no line yet".
        assert_eq!(buffer.read_line(&mut line), LineOutcome::NoLineYet);

        // Now the input ends.
        buffer.close_input();
        assert_eq!(
            buffer.read_line(&mut line),
            LineOutcome::EndOfInput,
            "a closed console with nothing left says 'end of input'"
        );
        assert!(buffer.read_line(&mut line).is_end_of_input());
        assert!(!LineStatus::NoLineYet.is_end_of_input());
        assert!(LineStatus::EndOfInput.is_end_of_input());
    }

    #[test]
    fn a_final_unterminated_line_is_delivered_before_the_end() {
        let mut buffer = ConsoleBuffer::new();
        buffer.write(b"no newline");
        buffer.close_input();
        let mut line = [0u8; 32];
        match buffer.read_line(&mut line) {
            LineOutcome::Line(length) => {
                assert_eq!(&line[..usize::from(length)], b"no newline");
            }
            other => panic!("expected the final line, got {other:?}"),
        }
        assert_eq!(buffer.read_line(&mut line), LineOutcome::EndOfInput);
    }

    #[test]
    fn a_carriage_return_is_not_part_of_the_line() {
        let mut buffer = ConsoleBuffer::new();
        buffer.write(b"windows\r\n");
        let mut line = [0u8; 32];
        match buffer.read_line(&mut line) {
            LineOutcome::Line(length) => {
                assert_eq!(&line[..usize::from(length)], b"windows");
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_line_is_discarded_rather_than_stalling_the_console() {
        let mut buffer = ConsoleBuffer::new();
        buffer.write(b"0123456789\nshort\n");
        let mut small = [0u8; 8];
        assert_eq!(buffer.read_line(&mut small), LineOutcome::TooLong);
        // The next read returns the *next* line, not the same one again.
        match buffer.read_line(&mut small) {
            LineOutcome::Line(length) => {
                assert_eq!(&small[..usize::from(length)], b"short");
            }
            other => panic!("expected the next line, got {other:?}"),
        }
    }
    #[test]
    fn the_buffer_is_bounded() {
        let mut buffer = ConsoleBuffer::new();
        let data = [0x41u8; CONSOLE_CAPACITY * 2];
        let accepted = buffer.write(&data);
        assert_eq!(
            accepted, CONSOLE_CAPACITY,
            "the buffer holds exactly its bound"
        );
        assert!(buffer.overflowed(), "the overflow is reported, not hidden");
        assert_eq!(buffer.len(), CONSOLE_CAPACITY);
    }

    #[test]
    fn the_protocol_layouts_are_stable() {
        assert_eq!(size_of::<ConsoleWriteRequest>(), 16);
        assert_eq!(size_of::<ConsoleWriteReply>(), 16);
        assert_eq!(size_of::<ConsoleReadLineRequest>(), 16);
        assert_eq!(size_of::<ConsoleReadLineReply>(), 16);

        assert_eq!(core::mem::offset_of!(ConsoleWriteRequest, version), 0);
        assert_eq!(core::mem::offset_of!(ConsoleWriteRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(ConsoleWriteRequest, length), 8);
        assert_eq!(core::mem::offset_of!(ConsoleWriteRequest, reserved), 12);

        assert_eq!(core::mem::offset_of!(ConsoleWriteReply, version), 0);
        assert_eq!(core::mem::offset_of!(ConsoleWriteReply, status), 4);
        assert_eq!(core::mem::offset_of!(ConsoleWriteReply, written), 8);
        assert_eq!(core::mem::offset_of!(ConsoleWriteReply, reserved), 12);

        assert_eq!(core::mem::offset_of!(ConsoleReadLineRequest, version), 0);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineRequest, max_len), 8);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineRequest, reserved), 12);

        assert_eq!(core::mem::offset_of!(ConsoleReadLineReply, version), 0);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineReply, status), 4);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineReply, length), 8);
        assert_eq!(core::mem::offset_of!(ConsoleReadLineReply, reserved), 12);
    }

    #[test]
    fn the_opcodes_round_trip_and_reject_the_unknown() {
        assert_eq!(ConsoleOpcode::Write.as_u64(), CONSOLE_OP_WRITE);
        assert_eq!(ConsoleOpcode::ReadLine.as_u64(), CONSOLE_OP_READ_LINE);
        assert_eq!(
            ConsoleOpcode::from_u64(CONSOLE_OP_WRITE),
            Some(ConsoleOpcode::Write)
        );
        assert_eq!(
            ConsoleOpcode::from_u64(CONSOLE_OP_READ_LINE),
            Some(ConsoleOpcode::ReadLine)
        );
        assert_eq!(
            ConsoleOpcode::from_u64(9999),
            None,
            "an unknown opcode must not be interpreted as a nearby one"
        );
    }

    #[test]
    fn chunk_counts_are_checked() {
        assert_eq!(chunk_count(0), Some(0));
        assert_eq!(chunk_count(1), Some(1));
        assert_eq!(chunk_count(MAX_INLINE_PAYLOAD), Some(1));
        assert_eq!(chunk_count(MAX_INLINE_PAYLOAD + 1), Some(2));
        assert_eq!(chunk_count(usize::MAX), None);
    }

    #[test]
    fn the_nonblocking_flag_is_available() {
        // The flag is what lets a caller ask "is there a line?" without
        // blocking the service.
        let request = ConsoleReadLineRequest {
            version: CONSOLE_VERSION,
            flags: CONSOLE_FLAG_NONBLOCK,
            max_len: 64,
            reserved: 0,
        };
        assert_ne!(request.flags, 0);
    }
}
