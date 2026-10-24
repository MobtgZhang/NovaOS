//! The bounded in-memory filesystem.
//!
//! # Why a static pool
//!
//! The kernel has no heap allocator, so the filesystem cannot use `Vec` or
//! `Box`. Every structure here is a fixed-size array with a compile-time
//! bound, and every bound is a named constant so that the kernel can size its
//! accounting against it.
//!
//! # Why file handles
//!
//! Resolving a path on every read is both slow and wrong: between the `open`
//! and the `read`, the file may have been replaced, so the caller would read a
//! file it never opened. An [`FileHandle`] names a *slot in the open-file
//! table*, and the table entry names the inode. The path is resolved exactly
//! once, at `open`.
//!
//! A handle carries a generation as well as a slot index. Closing a handle
//! invalidates it, and reusing the slot issues a new generation, so a stale
//! handle cannot silently address whatever took its place. That is the same
//! ABA defence the capability table uses, and it is why the handle is not a
//! bare index.
//!
//! # Checked arithmetic
//!
//! Every length, offset, and size computation in this file is checked. A
//! filesystem is exactly the place where an unchecked `wrapping_add` turns
//! into a memory-safety bug: the length of a buffer is what says how much of
//! it is real.

use nova_abi::cap::Rights;
use nova_abi::error::ErrorCode;

use crate::path::{CanonicalPath, MAX_NAME_BYTES, PathError, canonicalize};
use crate::scope::ScopeSet;

/// Maximum number of inodes (files and directories) the filesystem holds.
pub const MAX_INODES: usize = 64;

/// Maximum number of simultaneously open files.
pub const MAX_OPEN_FILES: usize = 32;

/// Maximum size of a single file, in bytes.
pub const MAX_FILE_BYTES: usize = 1024;

/// Maximum total size of all file contents together, in bytes.
///
/// This is the filesystem's real limit: it is what bounds the memory the
/// kernel is willing to spend on file data, independent of how many files
/// exist.
pub const MAX_TOTAL_BYTES: usize = 16 * 1024;

/// Maximum number of children a single directory may hold.
pub const MAX_CHILDREN: usize = 16;

/// The kind of thing an inode names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    /// A regular file, with content.
    File,
    /// A directory, with children.
    Directory,
}

/// A filesystem failure.
///
/// [`nova_abi::error::ErrorCode`] is frozen and has no `ENOENT`-style codes, so
/// this enum is richer than the ABI codes on purpose: the filesystem knows the
/// difference between "no such file" and "not a directory", and the kernel
/// maps it onto the ABI code it has. Everything that is not a distinct ABI
/// code maps to [`ErrorCode::Invalid`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// No such file or directory.
    NotFound,
    /// A path component that is not a directory was used as one.
    NotADirectory,
    /// A directory was used where a file was required.
    IsADirectory,
    /// The file or directory already exists.
    AlreadyExists,
    /// The path was malformed. See [`PathError`] for the detail.
    InvalidPath(PathError),
    /// The file handle is invalid, stale, or already closed.
    BadHandle,
    /// The caller's scope does not cover the request.
    Denied,
    /// A resource bound was reached: no space, too many inodes, too many
    /// open files.
    Quota,
    /// The argument was well-formed but unusable.
    InvalidArgument,
}

impl FsError {
    /// The ABI error code this failure maps to.
    ///
    /// `nova-abi` has no `ENOENT`/`ENOTDIR`/`EEXIST`, so those distinctions are
    /// collapsed into [`ErrorCode::Invalid`]. The distinctions survive inside
    /// the filesystem, where they are useful for logging; they are lost at the
    /// syscall boundary, which is the ABI's documented behaviour.
    pub const fn code(self) -> ErrorCode {
        match self {
            FsError::NotFound => ErrorCode::Invalid,
            FsError::NotADirectory => ErrorCode::Invalid,
            FsError::IsADirectory => ErrorCode::Invalid,
            FsError::AlreadyExists => ErrorCode::Invalid,
            FsError::InvalidPath(_) => ErrorCode::Invalid,
            FsError::BadHandle => ErrorCode::BadHandle,
            FsError::Denied => ErrorCode::Denied,
            FsError::Quota => ErrorCode::Quota,
            FsError::InvalidArgument => ErrorCode::Invalid,
        }
    }

    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            FsError::NotFound => "E_NOT_FOUND",
            FsError::NotADirectory => "E_NOT_A_DIRECTORY",
            FsError::IsADirectory => "E_IS_A_DIRECTORY",
            FsError::AlreadyExists => "E_ALREADY_EXISTS",
            FsError::InvalidPath(_) => "E_INVALID_PATH",
            FsError::BadHandle => "E_BAD_HANDLE",
            FsError::Denied => "E_DENIED",
            FsError::Quota => "E_QUOTA",
            FsError::InvalidArgument => "E_INVALID_ARGUMENT",
        }
    }
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::InvalidPath(error) => write!(f, "{}", error.name()),
            other => f.write_str(other.name()),
        }
    }
}

/// A handle to an open file.
///
/// The handle is a slot index plus a generation. The index alone is unsafe:
/// after a close, the slot is reused, and a stale index would address the new
/// file. The generation makes the reuse visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileHandle {
    slot: u32,
    generation: u32,
}

impl FileHandle {
    /// The table slot this handle names.
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// The generation of the slot this handle was issued against.
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// What the filesystem knows about a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Metadata {
    /// Whether the path names a file or a directory.
    pub kind: FileKind,
    /// The size of the file's contents, in bytes. Zero for a directory.
    pub size: u32,
}

/// One entry of a directory listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirEntry {
    /// The child's name, without any directory separators.
    pub name: [u8; MAX_NAME_BYTES],
    /// How many bytes of `name` are meaningful.
    pub name_len: u8,
    /// Whether the child is a file or a directory.
    pub kind: FileKind,
    /// The child's size, in bytes. Zero for a directory.
    pub size: u32,
}

impl DirEntry {
    /// The child's name as bytes.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..usize::from(self.name_len)]
    }

    /// The child's name as UTF-8, if it is valid UTF-8.
    pub fn name_str(&self) -> Option<&str> {
        core::str::from_utf8(self.name_bytes()).ok()
    }
}

/// An inode: the filesystem's record of a file or directory.
#[derive(Clone, Copy, Debug)]
struct Inode {
    kind: FileKind,
    name: [u8; MAX_NAME_BYTES],
    name_len: u8,
    parent: u16,
    data: [u8; MAX_FILE_BYTES],
    data_len: u32,
    children: [u16; MAX_CHILDREN],
    child_count: u8,
    in_use: bool,
}

/// Sentinel for "no parent" — the root inode's parent.
const NO_PARENT: u16 = u16::MAX;

/// One entry of the open-file table.
#[derive(Clone, Copy, Debug)]
struct OpenEntry {
    inode: u16,
    offset: u32,
    generation: u32,
    in_use: bool,
}

/// The RAM filesystem.
///
/// All state is inline: the struct is a fixed-size value that the kernel can
/// place in a static, and it needs no allocator.
#[derive(Clone, Debug)]
pub struct RamFs {
    inodes: [Inode; MAX_INODES],
    open: [OpenEntry; MAX_OPEN_FILES],
    inode_count: usize,
    total_bytes: usize,
}

impl Default for RamFs {
    fn default() -> Self {
        Self::new()
    }
}

impl RamFs {
    /// A filesystem containing only the root directory.
    pub fn new() -> Self {
        let mut fs = RamFs {
            inodes: [blank_inode(); MAX_INODES],
            open: [blank_open(); MAX_OPEN_FILES],
            inode_count: 0,
            total_bytes: 0,
        };
        // Slot 0 is always the root: every path resolves from it.
        fs.inodes[0] = Inode {
            kind: FileKind::Directory,
            name: [0; MAX_NAME_BYTES],
            name_len: 0,
            parent: NO_PARENT,
            data: [0; MAX_FILE_BYTES],
            data_len: 0,
            children: [0; MAX_CHILDREN],
            child_count: 0,
            in_use: true,
        };
        fs.inode_count = 1;
        fs
    }

    /// The number of inodes in use, including the root.
    pub const fn inode_count(&self) -> usize {
        self.inode_count
    }

    /// The number of open files.
    pub fn open_count(&self) -> usize {
        self.open.iter().filter(|entry| entry.in_use).count()
    }

    /// The total number of content bytes stored across all files.
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Canonicalise a caller-supplied path.
    ///
    /// A convenience so that callers do not have to remember to canonicalise
    /// before touching the filesystem: every public path-taking method
    /// canonicalises internally, which makes "resolve `.` and `..` first" a
    /// property of the filesystem rather than a discipline of its callers.
    pub fn canonical(&self, path: &[u8]) -> Result<CanonicalPath, FsError> {
        canonicalize(path).map_err(FsError::InvalidPath)
    }

    /// Resolve a canonical path to an inode index.
    fn resolve(&self, path: &CanonicalPath) -> Result<usize, FsError> {
        let mut current: usize = 0;
        for component in path.components() {
            if self.inodes[current].kind != FileKind::Directory {
                return Err(FsError::NotADirectory);
            }
            match self.find_child(current, component) {
                Some(index) => current = index,
                None => return Err(FsError::NotFound),
            }
        }
        Ok(current)
    }

    /// Find the child of `dir` with the given name.
    fn find_child(&self, dir: usize, name: &[u8]) -> Option<usize> {
        let inode = &self.inodes[dir];
        for index in 0..usize::from(inode.child_count) {
            let child = usize::from(inode.children[index]);
            let candidate = &self.inodes[child];
            if candidate.name_len as usize == name.len()
                && &candidate.name[..usize::from(candidate.name_len)] == name
            {
                return Some(child);
            }
        }
        None
    }

    /// Create a regular file at `path`.
    ///
    /// The parent directory must exist. Creating a file that already exists is
    /// refused rather than truncating it: truncation is a separate, explicit
    /// operation, and conflating the two lets a `create` silently destroy
    /// data.
    pub fn create_file(&mut self, path: &[u8]) -> Result<(), FsError> {
        self.create(path, FileKind::File)
    }

    /// Create a directory at `path`.
    pub fn create_dir(&mut self, path: &[u8]) -> Result<(), FsError> {
        self.create(path, FileKind::Directory)
    }

    fn create(&mut self, raw: &[u8], kind: FileKind) -> Result<(), FsError> {
        let path = canonicalize(raw).map_err(FsError::InvalidPath)?;
        if path.is_root() {
            return Err(FsError::AlreadyExists);
        }
        let name = path.file_name().ok_or(FsError::InvalidArgument)?;
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(FsError::InvalidArgument);
        }
        let parent = match path.parent() {
            Some(parent) => parent,
            None => return Err(FsError::InvalidArgument),
        };
        let parent_index = self.resolve(&parent)?;
        if self.inodes[parent_index].kind != FileKind::Directory {
            return Err(FsError::NotADirectory);
        }
        if self.find_child(parent_index, name).is_some() {
            return Err(FsError::AlreadyExists);
        }
        if usize::from(self.inodes[parent_index].child_count) >= MAX_CHILDREN {
            return Err(FsError::Quota);
        }
        if self.inode_count >= MAX_INODES {
            return Err(FsError::Quota);
        }

        let slot = self.inode_count;
        let mut inode = blank_inode();
        inode.kind = kind;
        inode.name_len = name.len() as u8;
        inode.name[..name.len()].copy_from_slice(name);
        inode.parent = parent_index as u16;
        inode.in_use = true;
        self.inodes[slot] = inode;

        let child_index = usize::from(self.inodes[parent_index].child_count);
        self.inodes[parent_index].children[child_index] = slot as u16;
        self.inodes[parent_index].child_count =
            u8::try_from(child_index.saturating_add(1)).unwrap_or(u8::MAX);

        self.inode_count = self.inode_count.saturating_add(1);
        Ok(())
    }

    /// Open a file, returning a handle.
    ///
    /// The path is resolved once, here. Everything afterwards goes through the
    /// handle, so a rename or replacement of the file does not change what the
    /// caller is reading.
    pub fn open(&mut self, path: &[u8]) -> Result<FileHandle, FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        let inode = self.resolve(&canonical)?;
        if self.inodes[inode].kind != FileKind::File {
            return Err(FsError::IsADirectory);
        }
        self.alloc_handle(inode)
    }

    /// Open a file only if the caller's scopes permit it.
    ///
    /// The scope check happens *before* the path is resolved, so a caller
    /// without a grant learns nothing about whether the file exists — the
    /// denial is uniform whether the path is absent or merely out of scope.
    pub fn open_scoped<const N: usize>(
        &mut self,
        scopes: &ScopeSet<N>,
        path: &[u8],
        required: Rights,
    ) -> Result<FileHandle, FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        if scopes.denies(&canonical, required) {
            return Err(FsError::Denied);
        }
        self.open(canonical.as_bytes())
    }

    /// Allocate an open-table entry for `inode`.
    fn alloc_handle(&mut self, inode: usize) -> Result<FileHandle, FsError> {
        for slot in 0..MAX_OPEN_FILES {
            if self.open[slot].in_use {
                continue;
            }
            let generation = self.open[slot]
                .generation
                .checked_add(1)
                .ok_or(FsError::Quota)?;
            // The generation starts at 1 so that a zeroed handle is never a
            // valid one, matching the capability table's "slot 0 is never
            // valid" discipline.
            if generation == 0 {
                return Err(FsError::Quota);
            }
            self.open[slot] = OpenEntry {
                inode: inode as u16,
                offset: 0,
                generation,
                in_use: true,
            };
            return Ok(FileHandle {
                slot: slot as u32,
                generation,
            });
        }
        Err(FsError::Quota)
    }

    /// Resolve a handle to its open-table slot, or report it as stale.
    fn slot_of(&self, handle: FileHandle) -> Result<usize, FsError> {
        let slot = usize::try_from(handle.slot).map_err(|_| FsError::BadHandle)?;
        if slot >= MAX_OPEN_FILES {
            return Err(FsError::BadHandle);
        }
        let entry = self.open[slot];
        if !entry.in_use || entry.generation != handle.generation {
            // The slot was closed, or reused by a later open. Either way the
            // handle the caller holds is not the handle the table knows.
            return Err(FsError::BadHandle);
        }
        Ok(slot)
    }

    /// Read from an open file into `buffer`.
    ///
    /// Returns the number of bytes copied. A read at end of file copies zero
    /// bytes and succeeds; the caller distinguishes that from an error by the
    /// return value, not by an error code.
    pub fn read(&mut self, handle: FileHandle, buffer: &mut [u8]) -> Result<usize, FsError> {
        let slot = self.slot_of(handle)?;
        let inode = usize::from(self.open[slot].inode);
        let offset =
            usize::try_from(self.open[slot].offset).map_err(|_| FsError::InvalidArgument)?;
        let length =
            usize::try_from(self.inodes[inode].data_len).map_err(|_| FsError::InvalidArgument)?;
        if offset >= length {
            return Ok(0);
        }
        let available = length.saturating_sub(offset);
        let count = available.min(buffer.len());
        let end = offset.checked_add(count).ok_or(FsError::InvalidArgument)?;
        buffer[..count].copy_from_slice(&self.inodes[inode].data[offset..end]);
        let new_offset = end;
        self.open[slot].offset = u32::try_from(new_offset).map_err(|_| FsError::InvalidArgument)?;
        Ok(count)
    }

    /// Write to an open file, returning the number of bytes accepted.
    ///
    /// The write is clipped to the file's and the filesystem's bounds rather
    /// than failing outright, because a partially accepted write is what a
    /// caller of a byte stream expects. The bounds are checked before any
    /// byte is copied.
    pub fn write(&mut self, handle: FileHandle, data: &[u8]) -> Result<usize, FsError> {
        let slot = self.slot_of(handle)?;
        let inode = usize::from(self.open[slot].inode);
        let offset =
            usize::try_from(self.open[slot].offset).map_err(|_| FsError::InvalidArgument)?;
        let current =
            usize::try_from(self.inodes[inode].data_len).map_err(|_| FsError::InvalidArgument)?;
        if offset > MAX_FILE_BYTES {
            return Err(FsError::Quota);
        }
        let wanted_end = offset.checked_add(data.len()).ok_or(FsError::Quota)?;
        let file_limit = wanted_end.min(MAX_FILE_BYTES);

        let new_len = current.max(file_limit);
        let growth = new_len.saturating_sub(current);
        if self.total_bytes.checked_add(growth).ok_or(FsError::Quota)? > MAX_TOTAL_BYTES {
            return Err(FsError::Quota);
        }

        let count = file_limit.saturating_sub(offset);
        self.inodes[inode].data[offset..file_limit].copy_from_slice(&data[..count]);
        self.inodes[inode].data_len = u32::try_from(new_len).map_err(|_| FsError::Quota)?;
        self.total_bytes = self.total_bytes.checked_add(growth).ok_or(FsError::Quota)?;
        let new_offset = offset.checked_add(count).ok_or(FsError::InvalidArgument)?;
        self.open[slot].offset = u32::try_from(new_offset).map_err(|_| FsError::InvalidArgument)?;
        Ok(count)
    }

    /// Close an open file.
    ///
    /// The slot is released and its generation advanced, so any handle the
    /// caller still holds is immediately stale.
    pub fn close(&mut self, handle: FileHandle) -> Result<(), FsError> {
        let slot = self.slot_of(handle)?;
        self.open[slot].in_use = false;
        self.open[slot].offset = 0;
        // Advancing the generation on close means a handle to the *closed* file
        // is already invalid; advancing again on the next open means a handle
        // to a *reused* slot is invalid too. Both directions are needed: the
        // first catches use-after-close, the second catches use-after-reuse.
        self.open[slot].generation = self.open[slot]
            .generation
            .checked_add(1)
            .ok_or(FsError::Quota)?;
        Ok(())
    }

    /// Read the metadata of a path.
    pub fn metadata(&self, path: &[u8]) -> Result<Metadata, FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        let inode = self.resolve(&canonical)?;
        Ok(Metadata {
            kind: self.inodes[inode].kind,
            size: self.inodes[inode].data_len,
        })
    }

    /// List a directory into `out`, returning the number of entries written.
    ///
    /// The listing is clipped to the buffer rather than failing, because the
    /// caller decides how much room it has. A directory with more children
    /// than the buffer holds is reported by the returned count being equal to
    /// the buffer length.
    pub fn list_dir(&self, path: &[u8], out: &mut [DirEntry]) -> Result<usize, FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        let inode = self.resolve(&canonical)?;
        if self.inodes[inode].kind != FileKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let child_count = usize::from(self.inodes[inode].child_count);
        let mut written = 0usize;
        for index in 0..child_count {
            if written >= out.len() {
                break;
            }
            let child = usize::from(self.inodes[inode].children[index]);
            let child_inode = &self.inodes[child];
            out[written] = DirEntry {
                name: child_inode.name,
                name_len: child_inode.name_len,
                kind: child_inode.kind,
                size: child_inode.data_len,
            };
            written = written.saturating_add(1);
        }
        Ok(written)
    }

    /// The number of children a directory has.
    pub fn child_count(&self, path: &[u8]) -> Result<usize, FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        let inode = self.resolve(&canonical)?;
        if self.inodes[inode].kind != FileKind::Directory {
            return Err(FsError::NotADirectory);
        }
        Ok(usize::from(self.inodes[inode].child_count))
    }
}

/// A zeroed inode, used to initialise the pool.
const fn blank_inode() -> Inode {
    Inode {
        kind: FileKind::File,
        name: [0; MAX_NAME_BYTES],
        name_len: 0,
        parent: NO_PARENT,
        data: [0; MAX_FILE_BYTES],
        data_len: 0,
        children: [0; MAX_CHILDREN],
        child_count: 0,
        in_use: false,
    }
}

/// A zeroed open-table entry, used to initialise the table.
const fn blank_open() -> OpenEntry {
    OpenEntry {
        inode: 0,
        offset: 0,
        generation: 0,
        in_use: false,
    }
}

#[cfg(test)]
mod tests {
    use nova_abi::cap::Rights;

    use super::{
        DirEntry, FileKind, FsError, MAX_FILE_BYTES, MAX_OPEN_FILES, MAX_TOTAL_BYTES, RamFs,
    };
    use crate::scope::{FileScope, ScopeSet};

    fn fs_with_files() -> RamFs {
        let mut fs = RamFs::new();
        fs.create_dir(b"/docs").expect("create /docs");
        fs.create_file(b"/docs/notes.txt").expect("create notes");
        fs.create_dir(b"/etc").expect("create /etc");
        fs.create_file(b"/etc/passwd").expect("create passwd");
        fs
    }

    #[test]
    fn file_read_write_ramfs() {
        let mut fs = fs_with_files();
        let handle = fs.open(b"/docs/notes.txt").expect("open notes");

        let written = fs.write(handle, b"hello nova").expect("write succeeds");
        assert_eq!(written, 10);

        // The read position is shared with the write position, so the read
        // starts at the end of what was written; seeking is not part of the
        // M4 surface, and a fresh handle reads from the start.
        let handle = fs.open(b"/docs/notes.txt").expect("reopen notes");
        let mut buffer = [0u8; 32];
        let read = fs.read(handle, &mut buffer).expect("read succeeds");
        assert_eq!(read, 10);
        assert_eq!(&buffer[..read], b"hello nova");

        // The metadata agrees with the content.
        let metadata = fs.metadata(b"/docs/notes.txt").expect("metadata");
        assert_eq!(metadata.kind, FileKind::File);
        assert_eq!(metadata.size, 10);
    }

    #[test]
    fn read_after_close_denied() {
        let mut fs = fs_with_files();
        let handle = fs.open(b"/docs/notes.txt").expect("open notes");
        fs.close(handle).expect("close succeeds");

        let mut buffer = [0u8; 8];
        assert_eq!(
            fs.read(handle, &mut buffer).unwrap_err(),
            FsError::BadHandle,
            "a read on a closed handle must be refused"
        );
        assert_eq!(fs.write(handle, b"x").unwrap_err(), FsError::BadHandle);
        assert_eq!(fs.close(handle).unwrap_err(), FsError::BadHandle);
    }

    #[test]
    fn a_stale_handle_does_not_address_a_reused_slot() {
        // The ABA case: close, reopen (the slot is reused), then use the old
        // handle. It must be refused, not silently act on the new file.
        let mut fs = fs_with_files();
        let old = fs.open(b"/docs/notes.txt").expect("open notes");
        fs.close(old).expect("close");
        let _new = fs.open(b"/etc/passwd").expect("open passwd");

        let mut buffer = [0u8; 8];
        assert_eq!(
            fs.read(old, &mut buffer).unwrap_err(),
            FsError::BadHandle,
            "the old handle must not address the new file"
        );
    }

    #[test]
    fn open_close_10000_leaks_no_handles() {
        // The table is fixed size, so a leak would show up as the table
        // filling up and `open` starting to fail.
        let mut fs = fs_with_files();
        for index in 0..10_000 {
            let handle = fs
                .open(b"/docs/notes.txt")
                .expect("open must not exhaust the table");
            fs.close(handle).expect("close must succeed");
            assert_eq!(fs.open_count(), 0, "iteration {index} left a handle open");
        }
        assert_eq!(fs.open_count(), 0);
        assert_eq!(fs.inode_count(), 5, "no inodes leaked");
    }

    #[test]
    fn the_open_table_is_bounded() {
        let mut fs = fs_with_files();
        let mut handles = [None; MAX_OPEN_FILES];
        for slot in handles.iter_mut() {
            *slot = Some(fs.open(b"/docs/notes.txt").expect("open"));
        }
        assert_eq!(fs.open_count(), MAX_OPEN_FILES);
        assert_eq!(
            fs.open(b"/docs/notes.txt").unwrap_err(),
            FsError::Quota,
            "the table must be bounded, not growable"
        );

        // Closing one frees exactly one slot.
        fs.close(handles[0].expect("handle")).expect("close");
        assert_eq!(fs.open_count(), MAX_OPEN_FILES - 1);
        assert!(fs.open(b"/docs/notes.txt").is_ok());
    }

    #[test]
    fn ramfs_size_bounded() {
        let mut fs = RamFs::new();
        fs.create_file(b"/big").expect("create");
        let handle = fs.open(b"/big").expect("open");

        // A single file cannot exceed its own bound.
        let chunk = [0xABu8; 256];
        let mut written = 0usize;
        loop {
            let count = fs.write(handle, &chunk).expect("write");
            if count == 0 {
                break;
            }
            written += count;
            if written > MAX_FILE_BYTES * 2 {
                break;
            }
        }
        assert!(
            written <= MAX_FILE_BYTES,
            "a single file must not exceed {MAX_FILE_BYTES} bytes, wrote {written}"
        );

        // The filesystem as a whole cannot exceed its own bound either.
        assert!(fs.total_bytes() <= MAX_TOTAL_BYTES);
        fs.create_file(b"/big2").expect("create second");
        let handle2 = fs.open(b"/big2").expect("open second");
        let mut filled = fs.total_bytes();
        loop {
            let count = fs.write(handle2, &chunk).expect("write");
            if count == 0 {
                break;
            }
            filled += count;
            if filled > MAX_TOTAL_BYTES * 2 {
                break;
            }
        }
        assert!(
            fs.total_bytes() <= MAX_TOTAL_BYTES,
            "the filesystem must not exceed {MAX_TOTAL_BYTES} bytes, holds {}",
            fs.total_bytes()
        );
    }

    #[test]
    fn a_full_filesystem_refuses_rather_than_overflowing() {
        let mut fs = RamFs::new();
        let chunk = [0u8; 1024];
        // Each file holds at most MAX_FILE_BYTES, so the filesystem's total
        // bound needs more files than that to be reached. The files live in
        // subdirectories because a single directory holds only MAX_CHILDREN
        // entries.
        let mut handles = [None; 24];
        let mut index = 0usize;
        for dir in 0..4 {
            let dir_name = [b'/', b'd', b'0' + dir as u8];
            fs.create_dir(&dir_name).expect("create dir");
            for file in 0..5 {
                let name = [b'/', b'd', b'0' + dir as u8, b'/', b'f', b'0' + file as u8];
                fs.create_file(&name).expect("create");
                handles[index] = Some(fs.open(&name).expect("open"));
                index += 1;
            }
        }
        // Fill the filesystem to its total bound.
        let mut exhausted = false;
        for handle in handles.iter().flatten() {
            loop {
                match fs.write(*handle, &chunk) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(FsError::Quota) => {
                        exhausted = true;
                        break;
                    }
                    Err(other) => panic!("unexpected error {other}"),
                }
            }
        }
        assert!(
            exhausted,
            "the filesystem must refuse writes past its total bound"
        );
        assert!(fs.total_bytes() <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn file_read_without_cap_denied() {
        let mut fs = fs_with_files();
        let mut grants = ScopeSet::<4>::new();
        grants.insert(FileScope::new(
            fs.canonical(b"/docs").expect("canonical"),
            Rights::READ,
        ));

        // In scope: allowed.
        assert!(
            fs.open_scoped(&grants, b"/docs/notes.txt", Rights::READ)
                .is_ok()
        );

        // Out of scope: denied, even though the file exists.
        assert_eq!(
            fs.open_scoped(&grants, b"/etc/passwd", Rights::READ)
                .unwrap_err(),
            FsError::Denied
        );

        // Right not held: denied.
        assert_eq!(
            fs.open_scoped(&grants, b"/docs/notes.txt", Rights::WRITE)
                .unwrap_err(),
            FsError::Denied
        );
    }

    #[test]
    fn a_denial_does_not_disclose_whether_the_file_exists() {
        // Both an absent path and an out-of-scope path must fail the same way,
        // or the denial becomes an existence oracle.
        let mut fs = fs_with_files();
        let grants = ScopeSet::<4>::new();
        assert_eq!(
            fs.open_scoped(&grants, b"/etc/passwd", Rights::READ)
                .unwrap_err(),
            FsError::Denied
        );
        assert_eq!(
            fs.open_scoped(&grants, b"/nonexistent", Rights::READ)
                .unwrap_err(),
            FsError::Denied
        );
    }

    #[test]
    fn a_scoped_capability_cannot_escape_its_subtree() {
        let mut fs = fs_with_files();
        let mut grants = ScopeSet::<4>::new();
        grants.insert(FileScope::new(
            fs.canonical(b"/docs").expect("canonical"),
            Rights::READ,
        ));

        // `..` is removed by canonicalisation before the scope check, so it
        // cannot widen the grant.
        assert_eq!(
            fs.open_scoped(&grants, b"/docs/../etc/passwd", Rights::READ)
                .unwrap_err(),
            FsError::Denied
        );
        assert_eq!(
            fs.open_scoped(&grants, b"/docs/../../etc/passwd", Rights::READ)
                .unwrap_err(),
            FsError::Denied
        );
    }

    #[test]
    fn listing_a_directory_yields_its_children() {
        let fs = fs_with_files();
        let mut entries = [DirEntry {
            name: [0; 64],
            name_len: 0,
            kind: FileKind::File,
            size: 0,
        }; 8];
        let count = fs.list_dir(b"/docs", &mut entries).expect("list");
        assert_eq!(count, 1);
        assert_eq!(entries[0].name_str(), Some("notes.txt"));
        assert_eq!(entries[0].kind, FileKind::File);

        assert_eq!(fs.child_count(b"/docs").expect("count"), 1);
        assert_eq!(fs.child_count(b"/").expect("count"), 2);
        assert_eq!(
            fs.list_dir(b"/docs/notes.txt", &mut entries).unwrap_err(),
            FsError::NotADirectory
        );
    }

    #[test]
    fn path_resolution_reports_the_right_failures() {
        let mut fs = fs_with_files();
        assert_eq!(fs.open(b"/nonexistent").unwrap_err(), FsError::NotFound);
        assert_eq!(
            fs.open(b"/docs/notes.txt/inner").unwrap_err(),
            FsError::NotADirectory,
            "a file used as a directory component"
        );
        assert_eq!(
            fs.open(b"/docs").unwrap_err(),
            FsError::IsADirectory,
            "a directory opened as a file"
        );
        assert_eq!(
            fs.create_file(b"/docs/notes.txt").unwrap_err(),
            FsError::AlreadyExists
        );
        assert_eq!(
            fs.create_dir(b"/nope/child").unwrap_err(),
            FsError::NotFound
        );
    }

    #[test]
    fn a_dot_component_naming_a_file_is_resolved_not_rejected() {
        // `.` names the file itself, so `/docs/notes.txt/.` is the file. The
        // canonical form drops the `.`, and the resolution then succeeds.
        let mut fs = fs_with_files();
        let handle = fs.open(b"/docs/notes.txt/.").expect("open with a dot");
        fs.close(handle).expect("close");
    }

    #[test]
    fn writes_are_clipped_to_the_file_bound() {
        let mut fs = RamFs::new();
        fs.create_file(b"/f").expect("create");
        let handle = fs.open(b"/f").expect("open");
        let data = [0x5Au8; 64];
        let mut written = 0usize;
        loop {
            let count = fs.write(handle, &data).expect("write");
            if count == 0 {
                break;
            }
            written += count;
            assert!(written <= MAX_FILE_BYTES);
        }
        assert_eq!(written, MAX_FILE_BYTES);
        assert_eq!(
            fs.metadata(b"/f").expect("metadata").size,
            MAX_FILE_BYTES as u32
        );
    }
}
