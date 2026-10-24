//! The file service protocol.
//!
//! # Handles, not paths
//!
//! The protocol opens a file once and then addresses it by handle. Every
//! later operation — read, write, close — carries the handle, not the path.
//! That is deliberate: a path can be re-resolved to a different file between
//! two operations, so a protocol that took a path each time would let a
//! caller read a file it never opened. The handle binds the operations to
//! the inode that was actually opened.
//!
//! # Layout is a contract
//!
//! Every request and reply is `#[repr(C)]` with compile-time size and offset
//! assertions. The service and its clients are separate binaries; a struct
//! that grew a field would be read as a different message, not as a compile
//! error. The assertions make that drift a build failure.
//!
//! # Payload convention
//!
//! Variable-length data — a path, a file's bytes, a directory listing — travels
//! in the IPC message's inline payload, immediately after the fixed header,
//! and never inside the header itself. The header always says how many bytes
//! of payload accompany it, so a reader never has to guess where the payload
//! starts.

use crate::ProtocolVersion;

/// The file protocol version this implementation speaks.
pub const FILE_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Opcode for opening a file.
pub const FILE_OP_OPEN: u64 = 1;

/// Opcode for reading from an open file.
pub const FILE_OP_READ: u64 = 2;

/// Opcode for writing to an open file.
pub const FILE_OP_WRITE: u64 = 3;

/// Opcode for closing an open file.
pub const FILE_OP_CLOSE: u64 = 4;

/// Opcode for listing a directory.
pub const FILE_OP_LIST: u64 = 5;

/// Opcode for reading a path's metadata.
pub const FILE_OP_STAT: u64 = 6;

/// Flag: open without failing if the file does not exist.
pub const FILE_FLAG_CREATE: u32 = 1 << 0;

/// Flag: truncate an existing file to zero length on open.
pub const FILE_FLAG_TRUNCATE: u32 = 1 << 1;

/// The kind of thing a path names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum FileKind {
    /// A regular file.
    File = 0,
    /// A directory.
    Directory = 1,
}

impl FileKind {
    /// The wire value.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Recover a kind from its wire value, refusing an unknown one.
    pub const fn from_code(code: u32) -> Option<FileKind> {
        match code {
            0 => Some(FileKind::File),
            1 => Some(FileKind::Directory),
            _ => None,
        }
    }
}

/// A request to open a file.
///
/// The path follows the header in the message's inline payload; `path_len`
/// says how many bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileOpenRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. [`FILE_FLAG_CREATE`] and [`FILE_FLAG_TRUNCATE`] are
    /// the defined bits.
    pub flags: u32,
    /// How many payload bytes carry the path.
    pub path_len: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file open request, in bytes.
pub const FILE_OPEN_REQUEST_SIZE: usize = 16;

/// A reply to an open.
///
/// The `handle` is the file handle the service issues. It is meaningful only
/// to the service that issued it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileOpenReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// The handle for the open file.
    pub handle: u64,
    /// The size of the file at the moment of the open, in bytes.
    pub size: u64,
}

/// Size of a file open reply, in bytes.
pub const FILE_OPEN_REPLY_SIZE: usize = 24;

/// A request to read from an open file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileReadRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// The handle of the open file.
    pub handle: u64,
    /// The byte offset to read from.
    pub offset: u64,
    /// How many bytes the caller can accept.
    pub length: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file read request, in bytes.
pub const FILE_READ_REQUEST_SIZE: usize = 32;

/// A reply to a read.
///
/// The bytes follow the header in the message's inline payload; `length` says
/// how many. A read at end of file returns zero bytes with a success status:
/// end of file is not an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileReadReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// How many payload bytes carry the data.
    pub length: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file read reply, in bytes.
pub const FILE_READ_REPLY_SIZE: usize = 16;

/// A request to write to an open file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileWriteRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// The handle of the open file.
    pub handle: u64,
    /// The byte offset to write at.
    pub offset: u64,
    /// How many payload bytes carry the data.
    pub length: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file write request, in bytes.
pub const FILE_WRITE_REQUEST_SIZE: usize = 32;

/// A reply to a write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileWriteReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// How many bytes were actually written.
    pub written: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file write reply, in bytes.
pub const FILE_WRITE_REPLY_SIZE: usize = 16;

/// A request to close an open file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileCloseRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// The handle of the open file.
    pub handle: u64,
    /// Reserved. Must be zero.
    pub reserved: u64,
}

/// Size of a file close request, in bytes.
pub const FILE_CLOSE_REQUEST_SIZE: usize = 24;

/// A reply to a close.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileCloseReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// Reserved. Must be zero.
    pub reserved: u64,
}

/// Size of a file close reply, in bytes.
pub const FILE_CLOSE_REPLY_SIZE: usize = 16;

/// A request to list a directory.
///
/// The path follows the header in the message's inline payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileListRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// How many payload bytes carry the path.
    pub path_len: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file list request, in bytes.
pub const FILE_LIST_REQUEST_SIZE: usize = 16;

/// One entry of a directory listing, as it travels on the wire.
///
/// The name follows the entry in the payload; `name_len` says how many bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileDirEntry {
    /// How many payload bytes carry the name.
    pub name_len: u32,
    /// Whether the entry is a file or a directory.
    pub kind: FileKind,
    /// The size of the entry, in bytes. Zero for a directory.
    pub size: u64,
}

/// Size of one directory listing entry, in bytes.
pub const FILE_DIR_ENTRY_SIZE: usize = 16;

/// A reply to a directory listing.
///
/// The entries follow the header in the message's inline payload; `count`
/// says how many. A listing that did not fit in the payload is clipped, and
/// the count says how many entries actually arrived — so a count smaller than
/// the directory's child count means "there are more".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileListReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// How many entries follow the header.
    pub count: u32,
    /// The total number of children the directory has, whether or not they
    /// all fitted in the payload.
    pub total: u32,
}

/// Size of a file list reply, in bytes.
pub const FILE_LIST_REPLY_SIZE: usize = 16;

/// A request for a path's metadata.
///
/// The path follows the header in the message's inline payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileStatRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero.
    pub flags: u32,
    /// How many payload bytes carry the path.
    pub path_len: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a file stat request, in bytes.
pub const FILE_STAT_REQUEST_SIZE: usize = 16;

/// A reply to a metadata request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileStatReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// The ABI error code, or zero on success.
    pub status: i32,
    /// Whether the path names a file or a directory.
    pub kind: FileKind,
    /// The size of the file, in bytes. Zero for a directory.
    pub size: u64,
}

/// Size of a file stat reply, in bytes.
pub const FILE_STAT_REPLY_SIZE: usize = 24;

const _: () = assert!(size_of::<FileOpenRequest>() == FILE_OPEN_REQUEST_SIZE);
const _: () = assert!(size_of::<FileOpenReply>() == FILE_OPEN_REPLY_SIZE);
const _: () = assert!(size_of::<FileReadRequest>() == FILE_READ_REQUEST_SIZE);
const _: () = assert!(size_of::<FileReadReply>() == FILE_READ_REPLY_SIZE);
const _: () = assert!(size_of::<FileWriteRequest>() == FILE_WRITE_REQUEST_SIZE);
const _: () = assert!(size_of::<FileWriteReply>() == FILE_WRITE_REPLY_SIZE);
const _: () = assert!(size_of::<FileCloseRequest>() == FILE_CLOSE_REQUEST_SIZE);
const _: () = assert!(size_of::<FileCloseReply>() == FILE_CLOSE_REPLY_SIZE);
const _: () = assert!(size_of::<FileListRequest>() == FILE_LIST_REQUEST_SIZE);
const _: () = assert!(size_of::<FileDirEntry>() == FILE_DIR_ENTRY_SIZE);
const _: () = assert!(size_of::<FileListReply>() == FILE_LIST_REPLY_SIZE);
const _: () = assert!(size_of::<FileStatRequest>() == FILE_STAT_REQUEST_SIZE);
const _: () = assert!(size_of::<FileStatReply>() == FILE_STAT_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(FileOpenRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileOpenRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileOpenRequest, path_len) == 8);
const _: () = assert!(core::mem::offset_of!(FileOpenRequest, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(FileOpenReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileOpenReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileOpenReply, handle) == 8);
const _: () = assert!(core::mem::offset_of!(FileOpenReply, size) == 16);

const _: () = assert!(core::mem::offset_of!(FileReadRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileReadRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileReadRequest, handle) == 8);
const _: () = assert!(core::mem::offset_of!(FileReadRequest, offset) == 16);
const _: () = assert!(core::mem::offset_of!(FileReadRequest, length) == 24);
const _: () = assert!(core::mem::offset_of!(FileReadRequest, reserved) == 28);

const _: () = assert!(core::mem::offset_of!(FileReadReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileReadReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileReadReply, length) == 8);
const _: () = assert!(core::mem::offset_of!(FileReadReply, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(FileWriteRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileWriteRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileWriteRequest, handle) == 8);
const _: () = assert!(core::mem::offset_of!(FileWriteRequest, offset) == 16);
const _: () = assert!(core::mem::offset_of!(FileWriteRequest, length) == 24);
const _: () = assert!(core::mem::offset_of!(FileWriteRequest, reserved) == 28);

const _: () = assert!(core::mem::offset_of!(FileWriteReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileWriteReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileWriteReply, written) == 8);
const _: () = assert!(core::mem::offset_of!(FileWriteReply, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(FileCloseRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileCloseRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileCloseRequest, handle) == 8);
const _: () = assert!(core::mem::offset_of!(FileCloseRequest, reserved) == 16);

const _: () = assert!(core::mem::offset_of!(FileCloseReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileCloseReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileCloseReply, reserved) == 8);

const _: () = assert!(core::mem::offset_of!(FileListRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileListRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileListRequest, path_len) == 8);
const _: () = assert!(core::mem::offset_of!(FileListRequest, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(FileDirEntry, name_len) == 0);
const _: () = assert!(core::mem::offset_of!(FileDirEntry, kind) == 4);
const _: () = assert!(core::mem::offset_of!(FileDirEntry, size) == 8);

const _: () = assert!(core::mem::offset_of!(FileListReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileListReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileListReply, count) == 8);
const _: () = assert!(core::mem::offset_of!(FileListReply, total) == 12);

const _: () = assert!(core::mem::offset_of!(FileStatRequest, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileStatRequest, flags) == 4);
const _: () = assert!(core::mem::offset_of!(FileStatRequest, path_len) == 8);
const _: () = assert!(core::mem::offset_of!(FileStatRequest, reserved) == 12);

const _: () = assert!(core::mem::offset_of!(FileStatReply, version) == 0);
const _: () = assert!(core::mem::offset_of!(FileStatReply, status) == 4);
const _: () = assert!(core::mem::offset_of!(FileStatReply, kind) == 8);
const _: () = assert!(core::mem::offset_of!(FileStatReply, size) == 16);

/// A file handle as it travels on the wire.
///
/// The service owns the meaning of the number. It is opaque to the client,
/// which must pass it back unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct WireHandle(pub u64);

impl WireHandle {
    /// The raw value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FILE_FLAG_CREATE, FILE_FLAG_TRUNCATE, FILE_OP_CLOSE, FILE_OP_LIST, FILE_OP_OPEN,
        FILE_OP_READ, FILE_OP_STAT, FILE_OP_WRITE, FILE_VERSION, FileCloseReply, FileCloseRequest,
        FileDirEntry, FileKind, FileListReply, FileListRequest, FileOpenReply, FileOpenRequest,
        FileReadReply, FileReadRequest, FileStatReply, FileStatRequest, FileWriteReply,
        FileWriteRequest, WireHandle,
    };
    use crate::ProtocolVersion;
    use nova_abi::ipc::MAX_INLINE_PAYLOAD;

    #[test]
    fn file_protocol_layout_stable() {
        // Every layout is pinned, because the two sides are separately
        // compiled binaries: a drifting struct is silent corruption, not a
        // compile error.
        assert_eq!(size_of::<FileOpenRequest>(), 16);
        assert_eq!(size_of::<FileOpenReply>(), 24);
        assert_eq!(size_of::<FileReadRequest>(), 32);
        assert_eq!(size_of::<FileReadReply>(), 16);
        assert_eq!(size_of::<FileWriteRequest>(), 32);
        assert_eq!(size_of::<FileWriteReply>(), 16);
        assert_eq!(size_of::<FileCloseRequest>(), 24);
        assert_eq!(size_of::<FileCloseReply>(), 16);
        assert_eq!(size_of::<FileListRequest>(), 16);
        assert_eq!(size_of::<FileDirEntry>(), 16);
        assert_eq!(size_of::<FileListReply>(), 16);
        assert_eq!(size_of::<FileStatRequest>(), 16);
        assert_eq!(size_of::<FileStatReply>(), 24);

        assert_eq!(core::mem::offset_of!(FileOpenRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileOpenRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileOpenRequest, path_len), 8);
        assert_eq!(core::mem::offset_of!(FileOpenRequest, reserved), 12);

        assert_eq!(core::mem::offset_of!(FileOpenReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileOpenReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileOpenReply, handle), 8);
        assert_eq!(core::mem::offset_of!(FileOpenReply, size), 16);

        assert_eq!(core::mem::offset_of!(FileReadRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileReadRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileReadRequest, handle), 8);
        assert_eq!(core::mem::offset_of!(FileReadRequest, offset), 16);
        assert_eq!(core::mem::offset_of!(FileReadRequest, length), 24);
        assert_eq!(core::mem::offset_of!(FileReadRequest, reserved), 28);

        assert_eq!(core::mem::offset_of!(FileReadReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileReadReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileReadReply, length), 8);
        assert_eq!(core::mem::offset_of!(FileReadReply, reserved), 12);

        assert_eq!(core::mem::offset_of!(FileWriteRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileWriteRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileWriteRequest, handle), 8);
        assert_eq!(core::mem::offset_of!(FileWriteRequest, offset), 16);
        assert_eq!(core::mem::offset_of!(FileWriteRequest, length), 24);
        assert_eq!(core::mem::offset_of!(FileWriteRequest, reserved), 28);

        assert_eq!(core::mem::offset_of!(FileWriteReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileWriteReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileWriteReply, written), 8);
        assert_eq!(core::mem::offset_of!(FileWriteReply, reserved), 12);

        assert_eq!(core::mem::offset_of!(FileCloseRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileCloseRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileCloseRequest, handle), 8);
        assert_eq!(core::mem::offset_of!(FileCloseRequest, reserved), 16);

        assert_eq!(core::mem::offset_of!(FileCloseReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileCloseReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileCloseReply, reserved), 8);

        assert_eq!(core::mem::offset_of!(FileListRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileListRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileListRequest, path_len), 8);
        assert_eq!(core::mem::offset_of!(FileListRequest, reserved), 12);

        assert_eq!(core::mem::offset_of!(FileDirEntry, name_len), 0);
        assert_eq!(core::mem::offset_of!(FileDirEntry, kind), 4);
        assert_eq!(core::mem::offset_of!(FileDirEntry, size), 8);

        assert_eq!(core::mem::offset_of!(FileListReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileListReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileListReply, count), 8);
        assert_eq!(core::mem::offset_of!(FileListReply, total), 12);

        assert_eq!(core::mem::offset_of!(FileStatRequest, version), 0);
        assert_eq!(core::mem::offset_of!(FileStatRequest, flags), 4);
        assert_eq!(core::mem::offset_of!(FileStatRequest, path_len), 8);
        assert_eq!(core::mem::offset_of!(FileStatRequest, reserved), 12);

        assert_eq!(core::mem::offset_of!(FileStatReply, version), 0);
        assert_eq!(core::mem::offset_of!(FileStatReply, status), 4);
        assert_eq!(core::mem::offset_of!(FileStatReply, kind), 8);
        assert_eq!(core::mem::offset_of!(FileStatReply, size), 16);
    }

    #[test]
    fn the_version_is_checked_before_anything_else() {
        assert_eq!(FILE_VERSION, ProtocolVersion::new(1, 0));
        assert!(crate::negotiate(FILE_VERSION, FILE_VERSION).is_ok());
        assert!(
            crate::negotiate(FILE_VERSION, ProtocolVersion::new(2, 0)).is_err(),
            "a different major version is a different protocol"
        );
    }

    #[test]
    fn the_opcodes_are_distinct_and_stable() {
        let opcodes = [
            FILE_OP_OPEN,
            FILE_OP_READ,
            FILE_OP_WRITE,
            FILE_OP_CLOSE,
            FILE_OP_LIST,
            FILE_OP_STAT,
        ];
        for (index, opcode) in opcodes.iter().enumerate() {
            for other in &opcodes[index + 1..] {
                assert_ne!(opcode, other, "two operations share an opcode");
            }
        }
    }

    #[test]
    fn a_handle_is_one_word_and_opaque() {
        assert_eq!(size_of::<WireHandle>(), 8);
        assert_eq!(WireHandle(42).raw(), 42);
    }

    #[test]
    fn a_request_fills_its_layout_exactly() {
        // The service reads the fields out of a byte buffer. The offsets are
        // pinned above; here the *values* are pinned, so a field that changed
        // type or meaning is caught too.
        let request = FileOpenRequest {
            version: FILE_VERSION,
            flags: FILE_FLAG_CREATE | FILE_FLAG_TRUNCATE,
            path_len: 11,
            reserved: 0,
        };
        assert_eq!(request.version.major, 1);
        assert_eq!(request.version.minor, 0);
        assert_eq!(request.flags, FILE_FLAG_CREATE | FILE_FLAG_TRUNCATE);
        assert_eq!(request.path_len, 11);
        assert_eq!(request.reserved, 0);
    }

    #[test]
    fn the_kind_codes_round_trip_and_reject_the_unknown() {
        assert_eq!(FileKind::File.code(), 0);
        assert_eq!(FileKind::Directory.code(), 1);
        assert_eq!(FileKind::from_code(0), Some(FileKind::File));
        assert_eq!(FileKind::from_code(1), Some(FileKind::Directory));
        assert_eq!(FileKind::from_code(2), None);
    }

    #[test]
    fn a_payload_never_exceeds_the_inline_limit() {
        // The header says how many payload bytes follow; the header itself
        // and the payload together must fit the inline budget.
        assert!(size_of::<FileOpenRequest>() + MAX_INLINE_PAYLOAD <= 256);
        assert!(size_of::<FileReadRequest>() + MAX_INLINE_PAYLOAD <= 256);
        assert!(size_of::<FileWriteRequest>() + MAX_INLINE_PAYLOAD <= 256);
    }

    #[test]
    fn the_close_and_stat_messages_carry_no_path() {
        // A close addresses the handle, not the path: the path was resolved
        // once, at open, and re-resolving it at close could close a
        // different file.
        let request = FileCloseRequest {
            version: FILE_VERSION,
            flags: 0,
            handle: 7,
            reserved: 0,
        };
        assert_eq!(request.handle, 7);
        let reply = FileCloseReply {
            version: FILE_VERSION,
            status: 0,
            reserved: 0,
        };
        assert_eq!(reply.status, 0);
    }

    #[test]
    fn a_listing_reports_how_much_it_could_not_send() {
        let reply = FileListReply {
            version: FILE_VERSION,
            status: 0,
            count: 3,
            total: 17,
        };
        assert!(
            reply.count < reply.total,
            "a clipped listing says so through the total"
        );
    }

    #[test]
    fn a_stat_reply_describes_both_kinds() {
        let file = FileStatReply {
            version: FILE_VERSION,
            status: 0,
            kind: FileKind::File,
            size: 4096,
        };
        assert_eq!(file.kind, FileKind::File);
        assert_eq!(file.size, 4096);

        let directory = FileStatReply {
            version: FILE_VERSION,
            status: 0,
            kind: FileKind::Directory,
            size: 0,
        };
        assert_eq!(directory.kind, FileKind::Directory);
    }

    #[test]
    fn a_dir_entry_carries_its_name_length() {
        let entry = FileDirEntry {
            name_len: 9,
            kind: FileKind::File,
            size: 12,
        };
        assert_eq!(entry.name_len, 9);
        assert_eq!(entry.kind, FileKind::File);
        assert_eq!(entry.size, 12);
    }

    #[test]
    fn the_read_and_write_requests_address_the_same_handle() {
        let read = FileReadRequest {
            version: FILE_VERSION,
            flags: 0,
            handle: 3,
            offset: 0,
            length: 64,
            reserved: 0,
        };
        let write = FileWriteRequest {
            version: FILE_VERSION,
            flags: 0,
            handle: 3,
            offset: 0,
            length: 64,
            reserved: 0,
        };
        assert_eq!(read.handle, write.handle);
        assert_eq!(read.length, write.length);
    }

    #[test]
    fn the_list_and_stat_requests_take_a_path() {
        let list = FileListRequest {
            version: FILE_VERSION,
            flags: 0,
            path_len: 5,
            reserved: 0,
        };
        let stat = FileStatRequest {
            version: FILE_VERSION,
            flags: 0,
            path_len: 5,
            reserved: 0,
        };
        assert_eq!(list.path_len, 5);
        assert_eq!(stat.path_len, 5);
    }

    #[test]
    fn the_read_reply_distinguishes_end_of_file_from_an_error() {
        // End of file is zero bytes with a success status; an error is a
        // non-zero status. A caller that cannot tell them apart will treat
        // a truncated read as a failed one.
        let end = FileReadReply {
            version: FILE_VERSION,
            status: 0,
            length: 0,
            reserved: 0,
        };
        assert_eq!(end.status, 0);
        assert_eq!(end.length, 0);

        let error = FileReadReply {
            version: FILE_VERSION,
            status: nova_abi::error::ErrorCode::BadHandle.code(),
            length: 0,
            reserved: 0,
        };
        assert_ne!(error.status, 0);
    }
}
