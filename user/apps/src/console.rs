//! Formatted output from ring 3.
//!
//! # How a ring-3 program produces output
//!
//! There is no console driver in M2 — the console service arrives in M4 — so a
//! user program writes to the kernel's serial line through the `WRITE` syscall.
//! The kernel copies the bytes out of the program's own address space and
//! writes them to the console itself, so the program never touches a device
//! and never holds a kernel address.
//!
//! # Why the buffer is a stack array
//!
//! The write is a *copy* into the kernel, so the bytes must exist in a
//! contiguous buffer that the kernel can read in one go. A `Vec` would need a
//! heap, and the heap is exactly what the runtime is trying to avoid
//! depending on. A fixed-size stack buffer is enough: a line of output is
//! short, and a program that needs to print more than fits writes it in
//! pieces.
//!
//! The buffer's *type* matters as well as its size. It is a `[u8; N]`, not a
//! `str`, because the program is writing bytes it has formatted itself and has
//! no reason to claim they are valid UTF-8.

use core::fmt::Write as _;

use crate::syscall;

/// The largest single write the runtime will issue.
///
/// Long enough for a formatted line with a few numbers in it, short enough
/// that the stack frame stays small — a ring-3 stack is a fixed mapping with a
/// guard page below it, and a large frame is a stack overflow waiting for a
/// program that formats deeply.
const BUFFER_SIZE: usize = 256;

/// Write `text` to standard output.
///
/// The bytes reach the serial console before the call returns, so a program
/// that writes and then exits does not lose its last line.
pub fn write_str(text: &str) -> Result<(), syscall::SyscallError> {
    write_bytes(text.as_bytes())
}

/// Write `bytes` to standard output.
pub fn write_bytes(bytes: &[u8]) -> Result<(), syscall::SyscallError> {
    if bytes.is_empty() {
        return Ok(());
    }

    // The kernel copies from the caller's address space, so the buffer has to
    // be a real contiguous region the kernel can read. A stack array is.
    let mut buffer = [0u8; BUFFER_SIZE];
    let count = bytes.len().min(BUFFER_SIZE);
    buffer[..count].copy_from_slice(&bytes[..count]);

    let written = syscall::write(1, &buffer[..count])?;
    if written as usize != count {
        // A short write means the kernel's copy was truncated, which is a
        // kernel bug rather than a program bug. Reporting it as an error is
        // honest; silently accepting a truncated line is not.
        return Err(syscall::SyscallError::ShortWrite);
    }
    Ok(())
}

/// Write a formatted value to standard output.
///
/// The formatting goes into a stack buffer and is written in one call, so a
/// program never sees a line torn in half by two separate writes.
pub fn write_fmt(args: core::fmt::Arguments<'_>) -> Result<(), syscall::SyscallError> {
    let mut buffer = [0u8; BUFFER_SIZE];
    let mut sink = BufferSink {
        buffer: &mut buffer,
        length: 0,
    };

    // A formatting failure means the buffer filled up. The bytes written so
    // far are still worth emitting, so this reports the failure *after*
    // flushing rather than dropping them.
    let formatted = sink.write_fmt(args);
    let written = sink.length;

    if written > 0 {
        write_bytes(&buffer[..written])?;
    }

    match formatted {
        Ok(()) => Ok(()),
        Err(_) => Err(syscall::SyscallError::ShortWrite),
    }
}

/// Print a formatted value to standard output.
///
/// The formatting macro cannot fail in a way the program can act on, so the
/// result is deliberately discarded: a program that cannot write to the
/// console has no better recourse than to carry on.
#[macro_export]
macro_rules! uwrite {
    ($($arg:tt)*) => {
        let _ = $crate::console::write_fmt(format_args!($($arg)*));
    };
}

/// A `core::fmt::Write` sink over a byte buffer.
///
/// A separate type rather than implementing the trait for `[u8]`, because the
/// trait needs a `write_str` that reports how much it managed to write, and
/// the standard library's implementation for `&mut [u8]` advances the slice —
/// which would make the "how much did I write" question depend on the slice's
/// length rather than on an explicit count.
struct BufferSink<'a> {
    /// The buffer being filled.
    buffer: &'a mut [u8],
    /// How many bytes have been written.
    length: usize,
}

impl core::fmt::Write for BufferSink<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if end > self.buffer.len() {
            return Err(core::fmt::Error);
        }
        self.buffer[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}
