//! User-space wrappers for the in-kernel file service.
//!
//! # Everything goes through one service
//!
//! A GUI program does not open files itself: it sends `SERVICE_REQUEST` to
//! `SERVICE_FILE` with a bounded inline payload, and the service resolves the
//! path under the *caller's* path grants. That is §16.1's rule — a process
//! starts with no file authority and keeps only the slice its parent handed
//! it — so the scope `init` passed at `exec` time is the whole of what any of
//! these calls can reach. Nothing in this module can widen it.
//!
//! # The budgets are the protocol's, not this module's
//!
//! `MAX_INLINE_PAYLOAD` is 64 bytes, and every message is a fixed header plus
//! inline data:
//!
//! | operation | header | data | per call |
//! | --- | --- | --- | --- |
//! | `open`, `stat`, `list` | 16 | path | 48-byte path |
//! | `read` | 16 (reply) | file bytes | 48 bytes |
//! | `write` | 32 (request) | file bytes | 32 bytes |
//! | `list_at` | 16 (reply) | records | 48 bytes of records |
//!
//! The service clips to the same numbers, so a larger request is answered with
//! fewer bytes rather than an error — which is why [`read`] and [`save`] loop
//! in these sizes instead of trying to move a whole file at once.
//!
//! # Handles, not paths
//!
//! After [`open`], every operation addresses the file by the handle the
//! service issued. A path could be re-resolved to a different file between two
//! operations; the handle cannot. Each handle must be [`close`]d: the service
//! has a bounded table of them, and one left open is a slot the next client
//! does not get.

use nova_abi::error::ErrorCode;
use nova_protocols::file::{
    FILE_CLOSE_REQUEST_SIZE, FILE_DIR_ENTRY_SIZE, FILE_FLAG_CREATE, FILE_FLAG_TRUNCATE,
    FILE_LIST_REPLY_SIZE, FILE_OP_CLOSE, FILE_OP_DELETE, FILE_OP_LIST_AT, FILE_OP_OPEN,
    FILE_OP_READ, FILE_OP_RENAME, FILE_OP_STAT, FILE_OP_WRITE, FILE_OPEN_REPLY_SIZE,
    FILE_READ_REPLY_SIZE, FILE_READ_REQUEST_SIZE, FILE_VERSION, FILE_WRITE_REQUEST_SIZE, FileKind,
};

use crate::graphics::{MAX_PAYLOAD, read_u32, read_u64, reply_ok, service_request};

/// The file service's identifier, as the kernel publishes it.
///
/// Wire values live in `kernel/src/services/mod.rs`; they are numbers a
/// request names a kernel-owned service by, not capability handles.
pub const SERVICE_FILE: u32 = 4;

/// The longest path this client will put on the wire.
///
/// The path shares a 64-byte payload with a 16-byte header, so it stops at 48
/// bytes. A longer path is refused here rather than truncated by the service:
/// a truncated path names a *different* file, and opening that one would be
/// worse than opening nothing.
pub const MAX_PATH: usize = MAX_PAYLOAD - 16;

/// Bytes one read can return: the reply header plus as much as fits.
pub const READ_CHUNK: usize = MAX_PAYLOAD - FILE_READ_REPLY_SIZE;

/// Bytes one write can carry: the request header plus as much as fits.
pub const WRITE_CHUNK: usize = MAX_PAYLOAD - FILE_WRITE_REQUEST_SIZE;

/// Bytes of directory records one listing page can carry.
pub const PAGE_BYTES: usize = MAX_PAYLOAD - FILE_LIST_REPLY_SIZE;

/// An open file: the service's handle and its size at the moment of the open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Opened {
    /// The handle later operations address the file by.
    pub handle: u64,
    /// The file's size then, in bytes.
    pub size: u64,
}

/// What a path names, and how big it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stat {
    /// Whether the path names a directory.
    pub directory: bool,
    /// Its size in bytes; zero for a directory.
    pub size: u64,
}

/// One page of a directory listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Page {
    /// How many records this page carried.
    pub count: u32,
    /// How many children the directory has in total, however many fitted.
    pub total: u32,
    /// How many bytes of the caller's buffer the records occupy.
    ///
    /// The records are packed head to tail with nothing marking where the
    /// page ends, so a caller walks `&out[..bytes]` and never the whole
    /// buffer: whatever the previous page left behind there is not this
    /// page's business.
    pub bytes: usize,
}

/// Write the protocol version every request starts with.
fn write_version(bytes: &mut [u8]) {
    bytes[0..2].copy_from_slice(&FILE_VERSION.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&FILE_VERSION.minor.to_le_bytes());
}

/// Read a `u32` field out of a reply, failing as malformed when it is absent.
fn field(bytes: &[u8], offset: usize) -> Result<u32, ErrorCode> {
    read_u32(bytes, offset).ok_or(ErrorCode::Invalid)
}

/// Read a `u64` field out of a reply, failing as malformed when it is absent.
fn field_u64(bytes: &[u8], offset: usize) -> Result<u64, ErrorCode> {
    read_u64(bytes, offset).ok_or(ErrorCode::Invalid)
}

/// Open `path` with `flags`, returning its handle and size.
///
/// [`FILE_FLAG_CREATE`] makes a missing file, and [`FILE_FLAG_TRUNCATE`]
/// empties one that exists; both are writes and both need a write grant on
/// the path. The service refuses a truncation it cannot perform — on a mounted
/// disk, which has no truncate — rather than quietly ignoring the flag, so a
/// save that asked for an empty file and did not get one fails here instead of
/// writing over stale bytes.
pub fn open(path: &[u8], flags: u32) -> Result<Opened, ErrorCode> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; MAX_PAYLOAD];
    write_version(&mut request);
    request[4..8].copy_from_slice(&flags.to_le_bytes());
    request[8..12].copy_from_slice(&(path.len() as u32).to_le_bytes());
    request[16..16 + path.len()].copy_from_slice(path);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_OPEN,
        &request[..16 + path.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])?;
    if length < FILE_OPEN_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }
    Ok(Opened {
        handle: field_u64(&reply, 8)?,
        size: field_u64(&reply, 16)?,
    })
}

/// Read up to `out.len()` bytes from an open file, returning how many arrived.
///
/// Zero means end of file. The service advances its own position for each
/// read — the protocol carries an offset for completeness, but the byte stream
/// is sequential — so a caller reads a file by calling this until it returns
/// zero, which is what [`read_all`] does.
pub fn read(handle: u64, out: &mut [u8]) -> Result<usize, ErrorCode> {
    if out.is_empty() {
        return Ok(0);
    }
    if out.len() > READ_CHUNK {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; FILE_READ_REQUEST_SIZE];
    write_version(&mut request);
    request[8..16].copy_from_slice(&handle.to_le_bytes());
    request[24..28].copy_from_slice(&(out.len() as u32).to_le_bytes());

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(SERVICE_FILE, FILE_OP_READ, &request, &mut reply)?;
    reply_ok(&reply[..length])?;
    if length < FILE_READ_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }

    // The reply is the 16-byte header followed by the data. `count` is what
    // the service decided to send; both it and the caller's buffer bound the
    // copy, so a service that sent more than promised cannot run past `out`.
    let count = field(&reply, 8)? as usize;
    let count = count.min(out.len()).min(length - FILE_READ_REPLY_SIZE);
    out[..count].copy_from_slice(&reply[FILE_READ_REPLY_SIZE..FILE_READ_REPLY_SIZE + count]);
    Ok(count)
}

/// Write `data` to an open file, returning how many bytes were accepted.
///
/// A short write is not an error — a byte stream can always be written
/// further later — but a *zero* write with data left over means the
/// filesystem is full, and [`save`] treats it that way.
pub fn write(handle: u64, data: &[u8]) -> Result<usize, ErrorCode> {
    if data.is_empty() {
        return Ok(0);
    }
    if data.len() > WRITE_CHUNK {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; FILE_WRITE_REQUEST_SIZE + WRITE_CHUNK];
    write_version(&mut request);
    request[8..16].copy_from_slice(&handle.to_le_bytes());
    request[24..28].copy_from_slice(&(data.len() as u32).to_le_bytes());
    request[32..32 + data.len()].copy_from_slice(data);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_WRITE,
        &request[..32 + data.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])?;
    field(&reply, 8).map(|written| written as usize)
}

/// Close a handle, returning its slot to the service.
///
/// The result is reported, because close is the only place a caller learns
/// whether its writes became durable — but a caller that is abandoning a file
/// after a failed save still has to call it, or the slot stays taken.
pub fn close(handle: u64) -> Result<(), ErrorCode> {
    let mut request = [0u8; FILE_CLOSE_REQUEST_SIZE];
    write_version(&mut request);
    request[8..16].copy_from_slice(&handle.to_le_bytes());

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(SERVICE_FILE, FILE_OP_CLOSE, &request, &mut reply)?;
    reply_ok(&reply[..length])
}

/// Delete the file at `path`.
///
/// Only a mounted disk's files can be deleted — the RAM filesystem holds its
/// files for the whole session — so a path outside a mount is refused as
/// [`ErrorCode::Unsupported`]. Deleting needs a write grant on the path, the
/// same right creating it needed.
pub fn delete(path: &[u8]) -> Result<(), ErrorCode> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; MAX_PAYLOAD];
    write_version(&mut request);
    request[8..12].copy_from_slice(&(path.len() as u32).to_le_bytes());
    request[16..16 + path.len()].copy_from_slice(path);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_DELETE,
        &request[..16 + path.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])
}

/// Rename the file at `from` to `to`.
///
/// Both names must live in the same filesystem — the operation never crosses
/// a mount boundary — and both need a write grant. A handle to the file
/// stays valid across the rename, exactly as a UNIX rename leaves an open
/// descriptor alone.
pub fn rename(from: &[u8], to: &[u8]) -> Result<(), ErrorCode> {
    if from.is_empty() || to.is_empty() || from.len() > MAX_PATH || to.len() > MAX_PATH {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; MAX_PAYLOAD];
    write_version(&mut request);
    request[8..12].copy_from_slice(&(from.len() as u32).to_le_bytes());
    request[12..16].copy_from_slice(&(to.len() as u32).to_le_bytes());
    request[16..16 + from.len()].copy_from_slice(from);
    request[16 + from.len()..16 + from.len() + to.len()].copy_from_slice(to);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_RENAME,
        &request[..16 + from.len() + to.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])
}

/// Ask what `path` names, without opening it.
pub fn stat(path: &[u8]) -> Result<Stat, ErrorCode> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; MAX_PAYLOAD];
    write_version(&mut request);
    request[8..12].copy_from_slice(&(path.len() as u32).to_le_bytes());
    request[16..16 + path.len()].copy_from_slice(path);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_STAT,
        &request[..16 + path.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])?;
    if length < 16 + 8 {
        return Err(ErrorCode::Invalid);
    }
    let kind = field(&reply, 8)?;
    Ok(Stat {
        directory: kind == FileKind::Directory.code(),
        size: field_u64(&reply, 16)?,
    })
}

/// List `path`'s children starting at index `offset`, writing records into
/// `out`.
///
/// `out` must be at least [`PAGE_BYTES`] long: the service fills a page to
/// that budget whatever room the caller has, and a reply the caller cannot
/// hold is refused rather than clipped. Use [`Records`] to walk what arrived,
/// then call again with `offset` advanced by `count` for the next page.
///
/// An offset past the end is an empty page with `count == 0`, not an error —
/// the directory may have shrunk between two pages, and a client that treated
/// that as a failure could never finish a listing it had already started.
pub fn list_at(path: &[u8], offset: u32, out: &mut [u8]) -> Result<Page, ErrorCode> {
    if path.is_empty() || path.len() > MAX_PATH || out.len() < PAGE_BYTES {
        return Err(ErrorCode::Invalid);
    }

    let mut request = [0u8; MAX_PAYLOAD];
    write_version(&mut request);
    request[8..12].copy_from_slice(&(path.len() as u32).to_le_bytes());
    request[12..16].copy_from_slice(&offset.to_le_bytes());
    request[16..16 + path.len()].copy_from_slice(path);

    let mut reply = [0u8; MAX_PAYLOAD];
    let length = service_request(
        SERVICE_FILE,
        FILE_OP_LIST_AT,
        &request[..16 + path.len()],
        &mut reply,
    )?;
    reply_ok(&reply[..length])?;
    if length < FILE_LIST_REPLY_SIZE {
        return Err(ErrorCode::Invalid);
    }

    let count = field(&reply, 8)?;
    let bytes = (length - FILE_LIST_REPLY_SIZE).min(out.len());
    out[..bytes].copy_from_slice(&reply[FILE_LIST_REPLY_SIZE..FILE_LIST_REPLY_SIZE + bytes]);
    Ok(Page {
        count,
        total: field(&reply, 12)?,
        bytes,
    })
}

/// A directory entry parsed out of a listing page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record<'a> {
    /// The entry's name, as the directory holds it.
    pub name: &'a [u8],
    /// Whether the entry is a directory.
    pub directory: bool,
    /// Its size in bytes; zero for a directory.
    pub size: u64,
}

/// Walk the records inside one listing page.
///
/// Records are variable length — a 16-byte header followed by the name — so
/// the page is parsed rather than indexed. A malformed tail stops the walk
/// instead of panicking: a page that does not add up is a reason to stop
/// reading, not a reason to trust the next fourteen bytes.
#[derive(Clone, Copy, Debug)]
pub struct Records<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Records<'a> {
    /// A walker over the records of one page.
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    /// The next entry, or `None` when the page is spent.
    pub fn next(&mut self) -> Option<Record<'a>> {
        let rest = self.bytes.get(self.cursor..)?;
        if rest.len() < FILE_DIR_ENTRY_SIZE {
            // Fewer bytes than one record header: nothing complete is left.
            self.cursor = self.bytes.len();
            return None;
        }
        let name_len = read_u32(rest, 0)? as usize;
        let kind = read_u32(rest, 4)?;
        let size = read_u64(rest, 8)?;
        let end = FILE_DIR_ENTRY_SIZE.checked_add(name_len)?;
        let name = rest.get(FILE_DIR_ENTRY_SIZE..end)?;
        self.cursor += end;
        Some(Record {
            name,
            directory: kind == FileKind::Directory.code(),
            size,
        })
    }
}

/// Read a whole file into `out`, returning how many bytes it held.
///
/// A file longer than `out` fills it and returns `out.len()`; the caller can
/// tell by comparing with `stat` if the difference matters. The handle is
/// closed on every path, including the failing one — an error that leaked a
/// handle would slowly exhaust the service's table.
pub fn read_all(path: &[u8], out: &mut [u8]) -> Result<usize, ErrorCode> {
    let file = open(path, 0)?;
    let mut total = 0usize;

    let outcome = loop {
        if total >= out.len() {
            break Ok(total);
        }
        match read(file.handle, &mut out[total..]) {
            Ok(0) => break Ok(total),
            Ok(count) => total += count,
            Err(error) => break Err(error),
        }
    };

    let closed = close(file.handle);
    match (outcome, closed) {
        (Ok(total), Ok(())) => Ok(total),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Replace a file's contents with `bytes`, creating it if it is missing.
///
/// The open asks for [`FILE_FLAG_CREATE`] *and* [`FILE_FLAG_TRUNCATE`]:
/// creating makes the file exist, and truncating empties whatever was there.
/// Without the second, a shorter save would leave the old file's tail behind
/// and the file would read back as the new text with the old text glued to
/// the end.
///
/// Returns how many bytes were written. A filesystem that accepted only part
/// of the text is an error rather than a partial success, because a caller
/// that believes it saved everything would not look again.
pub fn save(path: &[u8], bytes: &[u8]) -> Result<usize, ErrorCode> {
    let file = open(path, FILE_FLAG_CREATE | FILE_FLAG_TRUNCATE)?;

    let mut total = 0usize;
    let outcome = loop {
        if total >= bytes.len() {
            break Ok(total);
        }
        let end = total.saturating_add(WRITE_CHUNK).min(bytes.len());
        match write(file.handle, &bytes[total..end]) {
            Ok(0) => break Err(ErrorCode::Quota),
            Ok(count) => total += count,
            Err(error) => break Err(error),
        }
    };

    let closed = close(file.handle);
    match (outcome, closed) {
        (Ok(total), Ok(())) => Ok(total),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}
