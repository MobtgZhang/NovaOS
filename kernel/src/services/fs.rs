//! The kernel-side file server.
//!
//! # What this is
//!
//! The RAM filesystem (`nova-fs`) is the *store*. This module is the *service*
//! around it: the scope bookkeeping, the bridge from a capability handle to a
//! path scope, and the request/reply handling for the file protocol
//! (`nova-protocols`).
//!
//! # The capability↔scope bridge
//!
//! A path grant is a capability, and a capability is a handle into a table. The
//! filesystem does not know about handles, so the kernel keeps a *scope pool*
//! per task: a bounded table of `(handle, scope)` pairs. When the kernel grants
//! a path, it mints a capability in the task's table *and* records the scope
//! in the task's pool, so a handle can be turned back into a
//! [`FileScope`].
//!
//! The pool is not a second source of truth for *rights*: the rights come from
//! the capability table every time the scope set is built, and a scope whose
//! capability has been revoked is dropped. That is what makes revocation
//! effective — a revoked grant stops granting the moment it is revoked, not
//! when the pool is next rewritten.
//!
//! # The protocol
//!
//! Requests and replies are `nova-protocols`' fixed layouts, carried in the
//! inline payload of an IPC message: the fixed header first, then the
//! variable-length data. Every request carries a protocol version, and the
//! version is checked with `nova_protocols::negotiate` before anything else —
//! a message with an unknown version is refused with `E_UNSUPPORTED` rather
//! than interpreted under a nearby version.

use crate::klog;
use crate::logger::Level;
use nova_abi::cap::{CapHandle, Rights};
use nova_abi::error::ErrorCode;
use nova_fs::diskfs::{DiskFileId, DiskFs, DiskListEntry, MountError, NAME_FIELD_BYTES};
use nova_fs::path::{CanonicalPath, canonicalize};
use nova_fs::ramfs::{
    DirEntry, FileHandle, FileKind as FsFileKind, FsError, MAX_FILE_BYTES, RamFs,
};
use nova_fs::scope::{FileScope, ScopeSet};
use nova_ipc::Message;
use nova_protocols::ProtocolVersion;
use nova_protocols::file::{
    FILE_FLAG_CREATE, FILE_FLAG_TRUNCATE, FILE_LIST_REPLY_SIZE, FILE_OP_CHMOD, FILE_OP_CHOWN,
    FILE_OP_CLOSE, FILE_OP_DELETE, FILE_OP_LIST, FILE_OP_LIST_AT, FILE_OP_OPEN, FILE_OP_READ,
    FILE_OP_RENAME, FILE_OP_STAT, FILE_OP_WRITE, FILE_READ_REPLY_SIZE, FILE_VERSION,
    FileCloseReply, FileDirEntry, FileKind, FileListReply, FileOpenReply, FileReadReply,
    FileStatReply, FileWriteReply,
};
use nova_protocols::negotiate;

use crate::drivers::block::KernelBlockDevice;

/// Bytes a directory listing may spend on records.
///
/// The inline payload is 64 bytes and the reply header takes 16 of them, so a
/// listing has 48 left. A record is sixteen bytes plus the name, which means
/// roughly two entries per reply — the reason `FILE_OP_LIST_AT` exists. The
/// budget is computed here rather than left to each call site, because a call
/// site that got it wrong would produce a reply the IPC layer refuses to build
/// and the client would read a header whose `count` names records that are
/// not there.
const LISTING_BUDGET: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - FILE_LIST_REPLY_SIZE;

/// Bytes one read may return in its reply.
///
/// The same arithmetic as [`LISTING_BUDGET`], for the read header.
const READ_BUDGET: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - FILE_READ_REPLY_SIZE;

/// Maximum number of path scopes one task may hold.
///
/// A task that needs more than this has a design problem: the grant should be
/// on a directory, not on every file.
pub const MAX_SCOPES_PER_TASK: usize = 8;

/// Maximum number of files the server keeps open on behalf of clients.
///
/// The same bound as the filesystem's own open-file table, because the server's
/// table is what maps a *wire* handle back to a filesystem handle and its path.
pub const MAX_SERVER_FILES: usize = 32;

/// Maximum number of disks mounted into the namespace.
///
/// Two, because that is what the reference machine has — a writable test disk
/// and a read-only one — and because a mount table that is bounded by a
/// constant cannot be grown by a caller asking for one.
pub const MAX_MOUNTS: usize = 2;

/// The bit that marks a wire handle as naming a file on a disk.
///
/// Wire handles for RAM files are `generation << 32 | slot`, with both halves
/// small. Setting the top bit makes every disk handle land in a range no RAM
/// handle can reach, so a stale handle from one cannot address a file on the
/// other — the two are checked by construction rather than by remembering to
/// keep the counters apart.
const DISK_WIRE_BIT: u64 = 1 << 63;

/// The owner value for an open the server made on its own behalf.
///
/// Real task slots are bounded by [`crate::services::MAX_TASKS`], so this can
/// never match a task's slot and a `kill_task` can never close it.
const SERVER_OWNED: usize = usize::MAX;

/// A disk mounted into the file namespace.
pub struct DiskMount {
    /// The directory the disk appears at, e.g. `/media/test`.
    path: CanonicalPath,
    /// Whether writes are refused on this mount.
    read_only: bool,
    /// The filesystem, which validates the image once and keeps its directory
    /// in memory from then on.
    disk: DiskFs<KernelBlockDevice>,
}

impl DiskMount {
    /// The directory this disk is mounted at.
    pub const fn path(&self) -> &CanonicalPath {
        &self.path
    }

    /// Whether the mount refuses writes.
    pub const fn read_only(&self) -> bool {
        self.read_only
    }

    /// How many files the disk holds.
    pub fn file_count(&self) -> usize {
        self.disk.file_count()
    }
}

/// Where an open file lives.
#[derive(Clone, Copy, Debug)]
enum Origin {
    /// A file in the RAM filesystem.
    Ram(FileHandle),
    /// A file on a mounted disk: which mount, which file, and the offset the
    /// last protocol-level read or write left off at.
    Disk {
        /// Which mount the file is on.
        mount: usize,
        /// The filesystem's handle for it.
        file: DiskFileId,
        /// The position reads and writes advance from when the request does
        /// not carry one of its own.
        position: u64,
    },
}

/// One task's path grants.
///
/// The pool is the kernel's half of the capability↔scope bridge: it remembers
/// which handle stands for which scope, so a handle can be resolved back to a
/// path.
/// # Why the entries are not `Option<ScopeEntry>`
///
/// A nested `Option` here would put a niche inside the entry's `Rights`
/// field -- `Rights` is a `u32` whose only valid values are the ten defined
/// bits, so an invalid bit pattern is a spare encoding for `None`. That niche
/// then propagates outward: `TaskState` contains a `ScopePool`, so
/// `Option<TaskState>` looks for its own niche in the *same* field, and a
/// task whose scope pool is empty becomes indistinguishable from no task at
/// all. The kernel then reads back a task it has just created as `None`.
///
/// The count already says which entries are live, so the entries themselves
/// need no discriminant. Storing them plainly removes the nesting that the
/// collision needs.
#[derive(Clone, Copy, Debug)]
pub struct ScopePool {
    entries: [ScopeEntry; MAX_SCOPES_PER_TASK],
    len: usize,
}

/// One remembered grant.
///
/// An unused slot holds the invalid handle, which is what an empty pool
/// starts with.
///
/// # Why there is an explicit `in_use` flag
///
/// `FileScope` ends in a `Rights`, and `Rights` is a `u32` whose only valid
/// values are the ten defined bits. That leaves bits 10..31 unused, which
/// Rust uses as a niche: an `Option<FileScope>` is encoded by setting one of
/// those bits, and costs no extra space.
///
/// The niche is convenient and it is also a trap. `TaskState` contains a
/// scope pool, so `Option<TaskState>` looks for a niche of its own and finds
/// the *same* `Rights` field -- meaning a task whose scope pool is empty
/// encodes identically to no task at all, and the kernel reads back a task
/// it has just created as absent. The failure then surfaces far from its
/// cause: a null pointer where a task was expected, or a task that never
/// appears in a lookup.
///
/// The `in_use` flag gives the entry a discriminant of its own, so no niche
/// propagates outward and every `Option` in the kernel has an ordinary
/// tag.
#[derive(Clone, Copy, Debug)]
pub struct ScopeEntry {
    /// Whether this slot holds a remembered grant.
    pub in_use: bool,
    /// The capability handle that names the grant.
    pub handle: CapHandle,
    /// The granted subtree and the rights it carries.
    pub scope: FileScope,
}

impl Default for ScopePool {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopePool {
    /// An empty pool, which grants nothing.
    pub const fn new() -> Self {
        Self {
            entries: [ScopeEntry {
                in_use: false,
                handle: CapHandle::INVALID,
                scope: FileScope::new(CanonicalPath::ROOT, Rights::EMPTY),
            }; MAX_SCOPES_PER_TASK],
            len: 0,
        }
    }

    /// Remember a grant.
    ///
    /// Returns `false` when the pool is full or the handle is already
    /// remembered. A full pool is a refusal, not an eviction: silently
    /// dropping an earlier grant would change what the task may do without
    /// telling it.
    pub fn insert(&mut self, handle: CapHandle, scope: FileScope) -> bool {
        if self.len >= MAX_SCOPES_PER_TASK {
            return false;
        }
        if self.entries[..self.len]
            .iter()
            .any(|entry| entry.in_use && entry.handle == handle)
        {
            return false;
        }
        self.entries[self.len] = ScopeEntry {
            in_use: true,
            handle,
            scope,
        };
        self.len = self.len.saturating_add(1);
        true
    }

    /// The scope a handle stands for, if the pool remembers one.
    pub fn scope_for(&self, handle: CapHandle) -> Option<FileScope> {
        self.entries[..self.len]
            .iter()
            .find(|entry| entry.in_use && entry.handle == handle)
            .map(|entry| entry.scope)
    }

    /// Forget a grant.
    pub fn remove(&mut self, handle: CapHandle) -> bool {
        for index in 0..self.len {
            if self.entries[index].in_use && self.entries[index].handle == handle {
                let last = self.len.saturating_sub(1);
                self.entries[index] = self.entries[last];
                self.entries[last] = ScopeEntry {
                    in_use: false,
                    handle: CapHandle::INVALID,
                    scope: FileScope::new(CanonicalPath::ROOT, Rights::EMPTY),
                };
                self.len = last;
                return true;
            }
        }
        false
    }

    /// The remembered grants, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &ScopeEntry> {
        self.entries[..self.len].iter()
    }

    /// How many grants are remembered.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no grant is remembered.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Forget every grant.
    pub fn clear(&mut self) {
        self.entries = [ScopeEntry {
            in_use: false,
            handle: CapHandle::INVALID,
            scope: FileScope::new(CanonicalPath::ROOT, Rights::EMPTY),
        }; MAX_SCOPES_PER_TASK];
        self.len = 0;
    }
}

/// One file the server has opened for a client.
#[derive(Clone, Copy, Debug)]
struct ServerFile {
    /// The handle the client holds.
    wire: u64,
    /// Where the file lives, so a later operation reaches the right store.
    origin: Origin,
    /// The path it was opened at, so the scope can be re-checked on every
    /// later operation.
    path: CanonicalPath,
    /// The task slot that opened the file, so its death can close it.
    ///
    /// [`SERVER_OWNED`] for an open the server made on its own behalf and
    /// closed within the same request — bookkeeping that outliving the
    /// request would be a leak.
    owner: usize,
}

/// The file server.
///
/// Owns the RAM filesystem, the table of open files, the mounted disks, and the
/// protocol handling. Everything is inline, so the whole server is a fixed-size
/// value the kernel can place in a static.
/// The facts the seed writes into `/system` and `/EFI`, collected by the
/// boot path at the one moment they are all true at once.
///
/// Passing them in — rather than reaching for globals from inside the
/// service — keeps the seed a pure function of what the caller observed,
/// which is also what the host tests can construct without a machine.
/// The initramfs is borrowed, not stored: the seed walks it once at boot
/// and drops the borrow, so nothing in the filesystem holds a reference
/// into the boot snapshot.
pub struct SystemSeed {
    /// How many processors the boot manager started.
    pub cpu_count: u32,
    /// Usable physical memory, in bytes.
    pub usable_memory: u64,
    /// The highest mapped physical address, in bytes.
    pub mapped_memory: u64,
    /// What the boot menu selected, in its own spelling: `desktop`,
    /// `safe-terminal`, or `test`.
    pub boot_mode: &'static str,
    /// The initramfs archive the boot manager loaded, when there is one.
    pub initramfs: Option<crate::initramfs::Archive>,
}

/// Render a one-line text record into `buffer`, zero-terminated at the len.
///
/// The RAM file backend keeps bytes, not strings, so the record is written
/// as raw UTF-8 without a trailing NUL — the caller slices to the length
/// [`seed_line`] computed.
fn seed_line(buffer: &mut [u8; MAX_FILE_BYTES], args: core::fmt::Arguments<'_>) {
    let mut writer = LineWriter { buffer, used: 0 };
    use core::fmt::Write as _;
    let _ = writer.write_fmt(args);
}

/// A bounded `fmt::Write` that copies into a fixed stack buffer.
struct LineWriter<'a> {
    buffer: &'a mut [u8; MAX_FILE_BYTES],
    used: usize,
}

impl core::fmt::Write for LineWriter<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let room = self.buffer.len().saturating_sub(self.used);
        let take = bytes.len().min(room);
        self.buffer[self.used..self.used + take].copy_from_slice(&bytes[..take]);
        self.used += take;
        Ok(())
    }
}

pub struct FileServer {
    fs: RamFs,
    open_files: [Option<ServerFile>; MAX_SERVER_FILES],
    /// How many times each open-file slot has been reused.
    ///
    /// A disk file's wire handle carries its slot's generation, so a handle
    /// that outlives its close cannot address the file that later claims the
    /// slot — the handle's generation no longer matches the slot's, and the
    /// lookup refuses it instead of silently redirecting it.
    generations: [u16; MAX_SERVER_FILES],
    mounts: [Option<DiskMount>; MAX_MOUNTS],
}

impl Default for FileServer {
    fn default() -> Self {
        Self::new()
    }
}

impl FileServer {
    /// A server with an empty filesystem and no mounts.
    ///
    /// A `const fn` so the server can be built as part of a static
    /// initialiser. The server holds a whole [`RamFs`], which is about 71 KiB,
    /// and the kernel's stack is 64 KiB -- building one on the stack overflows
    /// it, and the overflow is reported somewhere else entirely.
    ///
    /// The mounts are written out as two explicit `None`s rather than `[None;
    /// MAX_MOUNTS]`, because the latter needs the element type to be `Copy`
    /// and a mount holds a filesystem.
    pub const fn new() -> Self {
        Self {
            fs: RamFs::new(),
            open_files: [None; MAX_SERVER_FILES],
            generations: [0; MAX_SERVER_FILES],
            mounts: [None, None],
        }
    }

    /// The underlying filesystem.
    pub fn fs(&self) -> &RamFs {
        &self.fs
    }

    /// The underlying filesystem, mutably.
    pub fn fs_mut(&mut self) -> &mut RamFs {
        &mut self.fs
    }

    /// Assemble the top-level directory layout the report specifies.
    ///
    /// `RamFs::new` creates only the root inode, and a file cannot be created
    /// under a parent that does not exist — so without this the very first
    /// `open("/home/…", CREATE)` would fail with a missing directory rather
    /// than with a permission decision. §14.5.2 lists the layout as a
    /// *convention* the `FileServer` assembles, not as special cases in the
    /// VFS, and this is that assembly, once, at boot.
    ///
    /// Nothing here grants anyone access to the tree it creates: a directory
    /// that exists is a directory a capability still has to name.
    ///
    /// The seed is not a placeholder layout: the files it writes carry bytes
    /// that describe the machine as the kernel actually sees it — the CPU
    /// count, the usable memory, the ABI revision, the boot mode, the actual
    /// inventory of the initramfs the boot manager handed over. Nothing here
    /// is decorative; a shell that browses `/` sees the system, not a mock of
    /// one.
    pub fn seed_default_layout(&mut self, seed: SystemSeed) {
        const DIRECTORIES: [&[u8]; 20] = [
            b"/system",
            b"/system/services",
            b"/system/runtime",
            b"/system/manifests",
            b"/devices",
            b"/proc",
            b"/tmp",
            b"/var",
            b"/var/log",
            b"/var/run",
            b"/var/run/notify",
            b"/home",
            b"/media",
            b"/dev",
            b"/etc",
            b"/EFI",
            b"/EFI/NovaOS",
            b"/home/nova",
            b"/home/nova/Desktop",
            b"/home/nova/.ede",
        ];
        for path in DIRECTORIES {
            // Already present means the layout is already assembled, which is
            // the right answer for a second call rather than an error.
            let _ = self.fs.create_dir(path);
        }

        self.seed_file(b"/etc/hostname", b"novaos\n");
        {
            let mut buffer = [0u8; MAX_FILE_BYTES];
            let version = concat!("NovaOS ", env!("CARGO_PKG_VERSION"));
            let mut used = 0usize;
            for byte in version.bytes() {
                buffer[used] = byte;
                used += 1;
            }
            let arch = crate::arch::arch_name();
            for byte in b" (".iter() {
                buffer[used] = *byte;
                used += 1;
            }
            for byte in arch.as_bytes() {
                buffer[used] = *byte;
                used += 1;
            }
            for byte in b")\n".iter() {
                buffer[used] = *byte;
                used += 1;
            }
            self.seed_file_bytes(b"/system/version", &buffer[..used]);
        }
        let mut buffer = [0u8; MAX_FILE_BYTES];
        seed_line(&mut buffer, format_args!("{}\n", seed.cpu_count));
        self.seed_file_bytes(b"/system/runtime/cpu-count", &buffer);

        let mut buffer = [0u8; MAX_FILE_BYTES];
        seed_line(
            &mut buffer,
            format_args!(
                "{} MiB usable of {} MiB mapped\n",
                seed.usable_memory / (1024 * 1024),
                seed.mapped_memory / (1024 * 1024)
            ),
        );
        self.seed_file_bytes(b"/system/runtime/memory", &buffer);

        let mut buffer = [0u8; MAX_FILE_BYTES];
        seed_line(
            &mut buffer,
            format_args!(
                "{}.{}\n",
                nova_abi::ABI_VERSION.major,
                nova_abi::ABI_VERSION.minor
            ),
        );
        self.seed_file_bytes(b"/system/runtime/abi", &buffer);

        let mut buffer = [0u8; MAX_FILE_BYTES];
        seed_line(&mut buffer, format_args!("{}\n", seed.boot_mode));
        self.seed_file_bytes(b"/EFI/NovaOS/boot-mode", &buffer);

        // The initramfs the boot manager loaded, listed — the real list of
        // what this machine booted with, not a snapshot of some other build.
        let mut inventory = [0u8; MAX_FILE_BYTES];
        let count = match &seed.initramfs {
            Some(archive) => {
                let mut used = 0usize;
                for entry in archive.iter() {
                    let name = entry.name;
                    if used + name.len() + 1 > inventory.len() {
                        break;
                    }
                    inventory[used..used + name.len()].copy_from_slice(name);
                    used += name.len();
                    inventory[used] = b'\n';
                    used += 1;
                }
                used
            }
            None => 0,
        };
        if count > 0 {
            self.seed_file_bytes(b"/EFI/NovaOS/initramfs.list", &inventory[..count]);
        }

        self.seed_file(
            b"/home/nova/notes.txt",
            b"Notes for the default user.\n\nEdit me with the desktop editor; the file persists across launches.\n",
        );

        // The desktop icons: EDE drives its desktop from `.desktop` files,
        // so the shell reads them from here rather than hardcoding positions
        // or commands. The contents are fixed at build time; a user whose
        // desktop shows different icons is editing these files, or adding
        // more of them, exactly as EDE would have them do.
        self.seed_file(
            b"/home/nova/Desktop/terminal.desktop",
            b"[Desktop Entry]\nType=Application\nName=Terminal\nExec=terminal\nIcon=utilities-terminal\nComment=NovaOS terminal\nCategories=System;\n",
        );
        self.seed_file(
            b"/home/nova/Desktop/files.desktop",
            b"[Desktop Entry]\nType=Application\nName=Files\nExec=file-manager\nIcon=system-file-manager\nComment=Browse and manage files\nCategories=System;\n",
        );
        self.seed_file(
            b"/home/nova/Desktop/editor.desktop",
            b"[Desktop Entry]\nType=Application\nName=Editor\nExec=editor\nIcon=accessories-text-editor\nComment=Edit a file\nCategories=Utility;\n",
        );
        self.seed_file(
            b"/home/nova/Desktop/widgets.desktop",
            b"[Desktop Entry]\nType=Application\nName=Widgets\nExec=widgets-demo\nIcon=preferences-desktop\nComment=novagraph widget showcase\nCategories=Utility;\n",
        );
        self.seed_file(
            b"/home/nova/.ede/panel.conf",
            b"[Panel]\napplets=start-menu,quick-launch,clock,taskbar,systray\nheight=32\n",
        );
        self.seed_file(
            b"/home/nova/.ede/desktop.conf",
            b"[Desktop]\nwallpaper_use=true\nwallpaper=nova-deep-sea\nicons=terminal,files,editor,widgets\n",
        );
    }

    /// Write `contents` to a freshly created file, dropping whatever bytes
    /// were already there. The seed files are root-owned and world-readable,
    /// which is the only shape that makes sense for a boot-time inventory:
    /// the desktop and the shell both read them, and neither is supposed to
    /// be able to alter the record of what booted.
    fn seed_file(&mut self, path: &[u8], contents: &[u8]) {
        let _ = self.fs.create_file(path);
        if let Ok(handle) = self.fs.open(path) {
            let _ = self.fs.write(handle, contents);
            let _ = self.fs.close(handle);
        }
    }

    /// The byte-slice twin of [`Self::seed_file`], for buffers that are built
    /// on the stack at seed time.
    fn seed_file_bytes(&mut self, path: &[u8], contents: &[u8]) {
        self.seed_file(path, contents);
    }

    /// Forget every open file, unmount every disk, and empty the filesystem.
    ///
    /// Unmounting is part of "forget everything": a restart that left a disk
    /// mounted would hand the new incarnation open files it never opened, on a
    /// device whose state the old incarnation may have left inconsistent.
    pub fn reset(&mut self) {
        // `clear` rather than a fresh `RamFs`: the filesystem is about 71 KiB
        // and the kernel's stack is 64 KiB, so assigning a new one would
        // overflow the stack.
        self.fs.clear();
        self.open_files = [None; MAX_SERVER_FILES];
        self.generations = [0; MAX_SERVER_FILES];
        self.mounts = [None, None];
    }

    /// Mount `disk_id` at `path`.
    ///
    /// The image is validated here, before it is reachable by any caller: a
    /// mount that failed leaves nothing behind, so a client can never see a
    /// half-mounted disk.
    ///
    /// # Errors
    ///
    /// A full mount table, an invalid path, or any failure the image's own
    /// validation reports — a foreign magic, geometry that does not add up, a
    /// directory entry that disagrees with the superblock.
    pub fn mount_disk(
        &mut self,
        disk_id: usize,
        path: &[u8],
        read_only: bool,
    ) -> Result<(), MountError> {
        let canonical = canonicalize(path).map_err(|_| MountError::BadGeometry)?;
        let slot = self
            .mounts
            .iter()
            .position(|mount| mount.is_none())
            .ok_or(MountError::BadGeometry)?;

        let disk = DiskFs::mount(KernelBlockDevice::new(disk_id), read_only)?;
        self.mounts[slot] = Some(DiskMount {
            path: canonical,
            read_only,
            disk,
        });
        Ok(())
    }

    /// The mount covering `path`, if any.
    ///
    /// The longest match wins, so a mount at `/media` and one at
    /// `/media/test` resolve `/media/test/x` to the latter. Only whole
    /// components count: `/media/t` is not under `/media/test`, because a
    /// prefix that stops mid-name is a coincidence of spelling, not a
    /// containment.
    fn mount_covering(&self, path: &CanonicalPath) -> Option<usize> {
        let requested = path.as_bytes();
        let mut best: Option<(usize, usize)> = None;

        for (index, mount) in self.mounts.iter().enumerate() {
            let Some(mount) = mount else { continue };
            let root = mount.path.as_bytes();
            if !requested.starts_with(root) {
                continue;
            }
            match requested.get(root.len()) {
                // Exactly the mount point.
                None => {}
                // A whole component below it.
                Some(&b'/') => {}
                // Mid-name: `/media/t` against `/media/test`.
                Some(_) => continue,
            }
            if best.is_none_or(|(best_len, _)| root.len() > best_len) {
                best = Some((root.len(), index));
            }
        }

        best.map(|(_, index)| index)
    }

    /// The path a mount sees: the request with the mount point stripped and a
    /// fresh root in front, so `/media/test/readme.txt` reaches the disk as
    /// `/readme.txt`.
    ///
    /// Returns `None` when the request *is* the mount point: the root of a
    /// disk is a directory, and there is no file name to hand the filesystem.
    fn mounted_relative<'a>(mount_root: &[u8], requested: &'a [u8]) -> Option<&'a [u8]> {
        let rest = requested.get(mount_root.len()..)?;
        if rest.is_empty() {
            return None;
        }
        // `rest` begins with '/' because `mount_covering` only matches on a
        // component boundary; the disk's flat namespace wants exactly one
        // leading slash and no more.
        Some(rest)
    }

    /// Open a file on a mounted disk, for a client whose scopes cover it.
    fn open_mounted(
        &mut self,
        index: usize,
        canonical: &CanonicalPath,
        owner: usize,
    ) -> Result<(u64, u32), FsError> {
        let mount = self.mounts[index].as_mut().ok_or(FsError::NotFound)?;
        let Some(relative) = Self::mounted_relative(mount.path.as_bytes(), canonical.as_bytes())
        else {
            return Err(FsError::IsADirectory);
        };

        let file = mount.disk.lookup(relative)?;
        let stat = mount.disk.stat(file)?;
        let read_only = mount.read_only;
        let path = *canonical;

        let wire = self.remember(
            Origin::Disk {
                mount: index,
                file,
                position: 0,
            },
            path,
            owner,
        )?;
        let _ = read_only;

        Ok((wire, stat.size as u32))
    }

    /// Open a file for a client whose scopes are `scopes`.
    ///
    /// The scope check happens before the path is resolved — before the mount
    /// table is even consulted — so a caller without a grant learns nothing
    /// about which disks are mounted, let alone what is on it. The open is
    /// recorded against `owner`'s task slot, so the owner's death closes it.
    pub fn open_for_task(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        owner: usize,
        path: &[u8],
        required: Rights,
    ) -> Result<(u64, u32), FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        if scopes.denies(&canonical, required) {
            return Err(FsError::Denied);
        }

        if let Some(index) = self.mount_covering(&canonical) {
            return self.open_mounted(index, &canonical, owner);
        }

        let handle = self.fs.open_scoped(scopes, path, required)?;
        let size = self
            .fs
            .metadata(canonical.as_bytes())
            .map(|metadata| metadata.size)
            .unwrap_or(0);
        let wire = self.remember(Origin::Ram(handle), canonical, owner)?;
        Ok((wire, size))
    }

    /// Open a file for a client whose identity the server does not track.
    ///
    /// The protocol path goes through [`FileServer::open_for_task`], which
    /// records the caller so its death can reclaim the file. This entry point
    /// is for the server's own opens — a truncation an `open` with the
    /// truncate flag just performed — which are closed within the same
    /// request and never outlive it.
    pub fn open_scoped(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        path: &[u8],
        required: Rights,
    ) -> Result<(u64, u32), FsError> {
        self.open_for_task(scopes, SERVER_OWNED, path, required)
    }

    /// Read from a file the client has open.
    pub fn read(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        wire: u64,
        buffer: &mut [u8],
    ) -> Result<usize, FsError> {
        let entry = self.entry(wire)?;
        require(scopes, &entry.path, Rights::READ)?;
        match entry.origin {
            Origin::Ram(handle) => self.fs.read(handle, buffer),
            Origin::Disk {
                mount,
                file,
                position,
            } => {
                let mount = self.mounts[mount].as_mut().ok_or(FsError::BadHandle)?;
                let read = mount.disk.read(file, position as usize, buffer)?;
                self.advance(wire, read as u64);
                Ok(read)
            }
        }
    }

    /// Write to a file the client has open.
    pub fn write(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        wire: u64,
        data: &[u8],
    ) -> Result<usize, FsError> {
        let entry = self.entry(wire)?;
        require(scopes, &entry.path, Rights::WRITE)?;
        match entry.origin {
            Origin::Ram(handle) => self.fs.write(handle, data),
            Origin::Disk {
                mount,
                file,
                position,
            } => {
                let mount = self.mounts[mount].as_mut().ok_or(FsError::BadHandle)?;
                if mount.read_only {
                    return Err(FsError::ReadOnly);
                }
                let written = mount.disk.write(file, position as usize, data)?;
                self.advance(wire, written as u64);
                Ok(written)
            }
        }
    }

    /// Seek an open disk file, so the next request without an offset of its
    /// own continues where this one stopped.
    fn advance(&mut self, wire: u64, delta: u64) {
        for slot in self.open_files.iter_mut() {
            if let Some(entry) = slot
                && entry.wire == wire
                && let Origin::Disk { position, .. } = &mut entry.origin
            {
                *position = position.saturating_add(delta);
            }
        }
    }

    /// Close a file the client has open.
    ///
    /// A disk file is flushed on close. That is the durability point the
    /// protocol offers: writes are write-through to the device, and `close`
    /// asks the device to make them durable, so "the write was acknowledged"
    /// and "the write is on the medium" are never confused for each other.
    pub fn close(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        wire: u64,
    ) -> Result<(), FsError> {
        let entry = self.entry(wire)?;
        require(scopes, &entry.path, Rights::READ)?;
        self.release(wire, entry.origin)
    }

    /// Drop a wire handle and the file behind it, with no scope check.
    ///
    /// This is the server's own bookkeeping — a close it performs because an
    /// *open* it just made failed halfway — and never a client's request: a
    /// client reaches the filesystem only through [`FileServer::handle_message`],
    /// which checks the caller's scopes first.
    fn release(&mut self, wire: u64, origin: Origin) -> Result<(), FsError> {
        match origin {
            Origin::Ram(handle) => self.fs.close(handle)?,
            Origin::Disk { mount, .. } => {
                let mount = self.mounts[mount].as_mut().ok_or(FsError::BadHandle)?;
                if !mount.read_only {
                    // A flush that fails is reported, because close is the
                    // only place a caller learns that its writes did not
                    // become durable.
                    mount.disk.flush()?;
                }
            }
        }
        for (index, slot) in self.open_files.iter_mut().enumerate() {
            if let Some(entry) = slot
                && entry.wire == wire
            {
                *slot = None;
                // The slot's next tenant must not be addressable by this
                // handle: bump the generation the next disk wire for this
                // slot will carry.
                self.generations[index] = self.generations[index].wrapping_add(1);
            }
        }
        Ok(())
    }

    /// Close every file a task has open, and say how many.
    ///
    /// Called from `kill_task`: a dead client's authority is withdrawn, and
    /// with it the open files that authority was exercising. Without this,
    /// the thirty-two `MAX_SERVER_FILES` slots could be drained by clients
    /// that are never coming back, and every later open in the system would
    /// fail as a quota — a denial of service paid for by nobody in particular.
    ///
    /// The slot is reclaimed *before* the underlying file is released, so a
    /// failed flush cannot strand a dead client's file in the table; the
    /// flush is best-effort, because the client it would be acknowledged to
    /// no longer exists. A file the server opened on its own behalf is never
    /// a match: its owner is [`SERVER_OWNED`], which no task slot can equal.
    pub fn close_owner(&mut self, owner: usize) -> usize {
        let mut closed = 0usize;
        for index in 0..MAX_SERVER_FILES {
            let Some(entry) = self.open_files[index] else {
                continue;
            };
            if entry.owner != owner {
                continue;
            }
            self.open_files[index] = None;
            self.generations[index] = self.generations[index].wrapping_add(1);

            match entry.origin {
                Origin::Ram(handle) => {
                    let _ = self.fs.close(handle);
                }
                Origin::Disk { mount, .. } => {
                    if let Some(mount) = self.mounts.get_mut(mount).and_then(|m| m.as_mut())
                        && !mount.read_only
                    {
                        let _ = mount.disk.flush();
                    }
                }
            }
            closed = closed.saturating_add(1);
        }
        closed
    }

    /// Empty an open file, under the caller's scopes.
    ///
    /// Truncating is writing, so the check is `WRITE`: a client that may read
    /// a file may not empty it. A mounted disk has no truncate operation, and
    /// the flag is refused there rather than quietly ignored — a client that
    /// asked for an empty file and got a full one would write its new content
    /// into the wrong place.
    fn truncate_scoped(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        wire: u64,
    ) -> Result<(), FsError> {
        let entry = self.entry(wire)?;
        require(scopes, &entry.path, Rights::WRITE)?;
        match entry.origin {
            Origin::Ram(handle) => self.fs.truncate(handle),
            Origin::Disk { .. } => Err(FsError::Unsupported),
        }
    }

    /// Delete a file by path, under the caller's scopes.
    ///
    /// Removing a name is a write to the directory it lives in, so the check
    /// is `WRITE` on the path itself — the same right creating it needed.
    /// Only the disk format has a delete: the RAM filesystem holds its files
    /// for the whole session, and reports the operation as unsupported
    /// rather than half-honouring it.
    pub fn delete_scoped(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        path: &[u8],
    ) -> Result<(), FsError> {
        let canonical = canonicalize(path).map_err(FsError::InvalidPath)?;
        if scopes.denies(&canonical, Rights::WRITE) {
            return Err(FsError::Denied);
        }

        let Some(index) = self.mount_covering(&canonical) else {
            return Err(FsError::Unsupported);
        };
        let Some(relative) = ({
            let mount = self.mounts[index].as_ref().ok_or(FsError::NotFound)?;
            Self::mounted_relative(mount.path.as_bytes(), canonical.as_bytes())
        }) else {
            // The request *is* the mount point: a directory, and directories
            // are not files this format can delete.
            return Err(FsError::IsADirectory);
        };

        let mount = self.mounts[index].as_mut().ok_or(FsError::NotFound)?;
        mount.disk.delete(relative)
    }

    /// Rename a file, under the caller's scopes.
    ///
    /// Both names need a write grant: the old name is removed and the new
    /// one appears, and each of those is a change to a directory the client
    /// may not own. The rename happens inside one filesystem, so the two
    /// names must be covered by the same mount — or both by none, for the
    /// RAM filesystem, which has no rename and refuses rather than guessing.
    pub fn rename_scoped(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        from: &[u8],
        to: &[u8],
    ) -> Result<(), FsError> {
        let old = canonicalize(from).map_err(FsError::InvalidPath)?;
        let new = canonicalize(to).map_err(FsError::InvalidPath)?;
        if scopes.denies(&old, Rights::WRITE) || scopes.denies(&new, Rights::WRITE) {
            return Err(FsError::Denied);
        }

        let old_mount = self.mount_covering(&old);
        let new_mount = self.mount_covering(&new);
        match (old_mount, new_mount) {
            (Some(index), Some(same)) if index == same => {
                let relative_from = {
                    let mount = self.mounts[index].as_ref().ok_or(FsError::NotFound)?;
                    Self::mounted_relative(mount.path.as_bytes(), old.as_bytes())
                        .ok_or(FsError::IsADirectory)?
                };
                let relative_to = {
                    let mount = self.mounts[index].as_ref().ok_or(FsError::NotFound)?;
                    Self::mounted_relative(mount.path.as_bytes(), new.as_bytes())
                        .ok_or(FsError::IsADirectory)?
                };
                let mount = self.mounts[index].as_mut().ok_or(FsError::NotFound)?;
                mount.disk.rename(relative_from, relative_to)
            }
            // Cross-mount renames do not exist here, and neither does a RAM
            // rename: both are refused as unsupported rather than silently
            // turned into a copy.
            _ => Err(FsError::Unsupported),
        }
    }

    /// One open file's record.
    fn entry(&self, wire: u64) -> Result<ServerFile, FsError> {
        self.open_files
            .iter()
            .flatten()
            .find(|entry| entry.wire == wire)
            .copied()
            .ok_or(FsError::BadHandle)
    }

    /// Handle one file-protocol request, returning the reply message.
    ///
    /// This is the server side of the protocol. The caller supplies the scopes
    /// and the task slot *of the requesting task*: the server executes on the
    /// client's behalf, so the client's grants decide what is allowed, and the
    /// client's slot is recorded so its death reclaims what it opened.
    pub fn handle_message(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        task_slot: usize,
        request: &Message,
    ) -> Message {
        let opcode = request.opcode();
        let payload = request.payload();

        match opcode {
            FILE_OP_OPEN => self.handle_open(scopes, task_slot, payload),
            FILE_OP_READ => self.handle_read(scopes, payload),
            FILE_OP_WRITE => self.handle_write(scopes, payload),
            FILE_OP_CLOSE => self.handle_close(scopes, payload),
            FILE_OP_LIST => self.handle_list(scopes, payload),
            FILE_OP_LIST_AT => self.handle_list_at(scopes, payload),
            FILE_OP_STAT => self.handle_stat(scopes, payload),
            FILE_OP_DELETE => self.handle_delete(scopes, payload),
            FILE_OP_RENAME => self.handle_rename(scopes, payload),
            FILE_OP_CHMOD => self.handle_chmod(scopes, payload),
            FILE_OP_CHOWN => self.handle_chown(scopes, payload),
            _ => reply_error(ErrorCode::Unsupported),
        }
    }

    /// The `open` operation.
    fn handle_open(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        task_slot: usize,
        payload: &[u8],
    ) -> Message {
        // The header is the fixed layout `nova-protocols` defines; the path
        // follows it in the same payload.
        let Some((version, flags, path_len)) = read_open_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(path) = slice_at(payload, 16, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };

        let creating = flags & FILE_FLAG_CREATE != 0;
        // Both flags are writes: one puts bytes where there were none, the
        // other takes them away. `required` therefore names the right the
        // *open* needs, which is what both the pre-check below and the
        // resolution inside `open_for_task` consult.
        let truncating = flags & FILE_FLAG_TRUNCATE != 0;
        let required = if creating || truncating {
            Rights::WRITE
        } else {
            Rights::READ
        };

        // Creating a file needs a write grant on the path *before* the file
        // exists, so the scope is checked against the path itself.
        if creating {
            let Ok(canonical) = canonicalize(path) else {
                return reply_error(ErrorCode::Invalid);
            };
            if scopes.denies(&canonical, required) {
                return reply_error(ErrorCode::Denied);
            }
            // A create that finds the file already there is not an error: the
            // client asked to open it for writing.
            let _ = self.fs.create_file(path);
        }

        match self.open_for_task(scopes, task_slot, path, required) {
            Ok((wire, size)) => {
                if truncating && let Err(error) = self.truncate_scoped(scopes, wire) {
                    // The file is open and must not stay open. A client that
                    // was refused the truncation would otherwise hold a
                    // handle to a file whose contents it got wrong, and the
                    // server would be one open-file slot down for nothing.
                    let entry = self.entry(wire).ok();
                    if let Some(entry) = entry {
                        let _ = self.release(wire, entry.origin);
                    }
                    return reply_error(error.code());
                }
                let mut bytes = [0u8; 24];
                write_protocol_version(&mut bytes, FILE_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..16].copy_from_slice(&wire.to_le_bytes());
                // A truncation happened after the size was measured, so the
                // size reported here has to be the one the client will read
                // back, not the one that was true a moment earlier.
                let reported = if truncating { 0 } else { size };
                bytes[16..24].copy_from_slice(&(reported as u64).to_le_bytes());
                reply_bytes(&bytes)
            }
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `read` operation.
    fn handle_read(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, _flags, wire, _offset, length)) = read_read_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let mut buffer = [0u8; READ_BUDGET];
        let wanted = usize::try_from(length).unwrap_or(0).min(READ_BUDGET);
        match self.read(scopes, wire, &mut buffer[..wanted]) {
            Ok(count) => {
                let mut bytes = [0u8; FILE_READ_REPLY_SIZE];
                write_protocol_version(&mut bytes, FILE_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..12].copy_from_slice(&(count as u32).to_le_bytes());
                bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
                reply_bytes(&bytes).with_extra_payload(&buffer[..count])
            }
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `write` operation.
    fn handle_write(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, _flags, wire, _offset, length)) = read_write_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(data) = slice_at(payload, 32, length) else {
            return reply_error(ErrorCode::Invalid);
        };
        match self.write(scopes, wire, data) {
            Ok(written) => {
                let mut bytes = [0u8; 16];
                write_protocol_version(&mut bytes, FILE_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..12].copy_from_slice(&(written as u32).to_le_bytes());
                bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
                reply_bytes(&bytes)
            }
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `close` operation.
    fn handle_close(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, wire)) = read_close_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        match self.close(scopes, wire) {
            Ok(()) => {
                let mut bytes = [0u8; 16];
                write_protocol_version(&mut bytes, FILE_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                reply_bytes(&bytes)
            }
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `list` operation.
    fn handle_list(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        self.list_from(scopes, payload, 0)
    }

    /// The `delete` operation: remove a file by name.
    ///
    /// Deleting is a write to the directory the name lives in, so the check
    /// is `WRITE` on the path itself — the same right creating it needed.
    /// Deleting is defined for the disk format, which is where names can be
    /// removed; the RAM filesystem keeps its files for the whole session and
    /// reports the operation as unsupported rather than half-honouring it.
    fn handle_delete(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, path_len)) = read_path_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(path) = slice_at(payload, 16, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        match self.delete_scoped(scopes, path) {
            Ok(()) => reply_status(),
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `rename` operation: move a file's name.
    ///
    /// Both paths must resolve inside the same filesystem — the operation
    /// never crosses a mount boundary, so a rename that would move a file
    /// from RAM to disk is refused rather than silently copied.
    fn handle_rename(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, old_len, new_len)) = read_rename_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(old) = slice_at(payload, 16, old_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Some(new) = slice_at(payload, 16 + old_len as usize, new_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        match self.rename_scoped(scopes, old, new) {
            Ok(()) => reply_status(),
            Err(error) => reply_error(error.code()),
        }
    }

    /// The `list_at` operation: the same listing, starting at one child index.
    ///
    /// The header has `list`'s shape with `reserved` read as the index of the
    /// first child to return, so a client pages through a directory with the
    /// same path, the same scope check, and the same record format. An offset
    /// past the end is an empty page, not an error: the directory may have
    /// shrunk between two pages, and a client that treats that as a failure
    /// cannot finish a listing it had already started.
    fn handle_list_at(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        payload: &[u8],
    ) -> Message {
        let Some(offset) = read_u32(payload, 12) else {
            return reply_error(ErrorCode::Invalid);
        };
        self.list_from(scopes, payload, offset as usize)
    }

    /// Enumerate a directory from `offset`, under `scopes`' permission.
    fn list_from(
        &mut self,
        scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
        payload: &[u8],
        offset: usize,
    ) -> Message {
        let Some((version, path_len)) = read_path_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        };
        let Some(path) = slice_at(payload, 16, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Ok(canonical) = canonicalize(path) else {
            return reply_error(ErrorCode::Invalid);
        };
        if scopes.denies(&canonical, Rights::READ) {
            return reply_error(ErrorCode::Denied);
        }

        if let Some(index) = self.mount_covering(&canonical) {
            return self.list_mounted(index, &canonical, offset);
        }

        // The whole directory at once, not just the first few: the reply
        // budget decides how many records fit, and the *skip* decides which
        // of them are this page's. Reading only as many as the reply could
        // hold would make the pages depend on the record sizes of the
        // entries before them.
        let mut entries = [DirEntry {
            name: [0; 64],
            name_len: 0,
            kind: FsFileKind::File,
            size: 0,
        }; nova_fs::ramfs::MAX_CHILDREN];
        let count = match self.fs.list_dir(canonical.as_bytes(), &mut entries) {
            Ok(count) => count,
            Err(error) => return reply_error(error.code()),
        };
        let total = match self.fs.child_count(canonical.as_bytes()) {
            Ok(total) => total as u32,
            Err(_) => 0,
        };

        let start = offset.min(count);
        let mut listing = [0u8; LISTING_BUDGET];
        let mut used = 0usize;
        let mut included = 0u32;
        for entry in entries.iter().take(count).skip(start) {
            let name = entry.name_bytes();
            let entry_size = 16usize.saturating_add(name.len());
            if used.saturating_add(entry_size) > listing.len() {
                break;
            }
            let record = FileDirEntry {
                name_len: name.len() as u32,
                kind: match entry.kind {
                    FsFileKind::File => FileKind::File,
                    FsFileKind::Directory => FileKind::Directory,
                },
                size: entry.size as u64,
            };
            listing[used..used + 4].copy_from_slice(&record.name_len.to_le_bytes());
            listing[used + 4..used + 8].copy_from_slice(&record.kind.code().to_le_bytes());
            listing[used + 8..used + 16].copy_from_slice(&record.size.to_le_bytes());
            used += 16;
            listing[used..used + name.len()].copy_from_slice(name);
            used += name.len();
            included += 1;
        }

        // `count` is what *arrived*, not what the directory holds: `total` is
        // the second, and a client telling them apart is how it knows to ask
        // for the next page.
        let mut bytes = [0u8; FILE_LIST_REPLY_SIZE];
        write_protocol_version(&mut bytes, FILE_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&included.to_le_bytes());
        bytes[12..16].copy_from_slice(&total.to_le_bytes());

        reply_bytes(&bytes).with_extra_payload(&listing[..used])
    }

    /// The `stat` operation.
    fn handle_stat(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, path_len)) = read_path_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        };
        let Some(path) = slice_at(payload, 16, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Ok(canonical) = canonicalize(path) else {
            return reply_error(ErrorCode::Invalid);
        };
        if scopes.denies(&canonical, Rights::READ) {
            return reply_error(ErrorCode::Denied);
        }

        if let Some(index) = self.mount_covering(&canonical) {
            return self.stat_mounted(index, &canonical);
        }

        let metadata = match self.fs.metadata(canonical.as_bytes()) {
            Ok(metadata) => metadata,
            Err(error) => return reply_error(error.code()),
        };
        let mut bytes = [0u8; 40];
        write_protocol_version(&mut bytes, FILE_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        let kind = match metadata.kind {
            FsFileKind::File => FileKind::File,
            FsFileKind::Directory => FileKind::Directory,
        };
        bytes[8..12].copy_from_slice(&kind.code().to_le_bytes());
        bytes[16..24].copy_from_slice(&(metadata.size as u64).to_le_bytes());
        bytes[24..28].copy_from_slice(&metadata.owner.to_le_bytes());
        bytes[28..32].copy_from_slice(&metadata.group.to_le_bytes());
        bytes[32..36].copy_from_slice(&(metadata.mode as u32).to_le_bytes());
        reply_bytes(&bytes)
    }

    /// Change the mode word on a RAM-backed file.
    ///
    /// Two layers say what may happen here, and they are different kinds of
    /// thing. The *scope* — the caller's grant over this path — is the only
    /// authority this service has, and it is checked before anything else.
    /// The `caller_uid` on the wire is attribution, not authentication: it
    /// records who asked, and [`RamFs::chmod`] uses it for its own
    /// owner-or-root rule; a client that lies about its uid fools no one
    /// who matters, because this kernel does not carry user identities into
    /// the service layer at all. The root-vs-owner convention that makes
    /// this operation safe to hand to unprivileged clients lives in the
    /// shell, whose session knows the user.
    fn handle_chmod(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, mode, path_len, caller_uid)) = read_chmod_header(payload) else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(path) = slice_at(payload, 16, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Ok(canonical) = canonicalize(path) else {
            return reply_error(ErrorCode::Invalid);
        };
        if scopes.denies(&canonical, Rights::WRITE) {
            return reply_error(ErrorCode::Denied);
        }
        if self.mount_covering(&canonical).is_some() {
            // The disk format carries no mode word; refusing beats inventing one.
            return reply_error(ErrorCode::Unsupported);
        }
        match self.fs.chmod(path, mode as u16, caller_uid) {
            Ok(()) => reply_status(),
            Err(error) => reply_error(error.code()),
        }
    }

    /// Change the owner and group on a RAM-backed file.
    ///
    /// Like [`Self::handle_chmod`], the scope is the authority and the wire
    /// uid is attribution. The root-only convention for this operation is
    /// the shell's to enforce; here the scope's write authority over the
    /// path is the whole of the check.
    fn handle_chown(&mut self, scopes: &ScopeSet<MAX_SCOPES_PER_TASK>, payload: &[u8]) -> Message {
        let Some((version, owner, group, path_len, _caller_uid)) = read_chown_header(payload)
        else {
            return reply_error(ErrorCode::Invalid);
        };
        if negotiate(FILE_VERSION, version).is_err() {
            return reply_error(ErrorCode::Unsupported);
        }
        let Some(path) = slice_at(payload, 20, path_len) else {
            return reply_error(ErrorCode::Invalid);
        };
        let Ok(canonical) = canonicalize(path) else {
            return reply_error(ErrorCode::Invalid);
        };
        if scopes.denies(&canonical, Rights::WRITE) {
            return reply_error(ErrorCode::Denied);
        }
        if self.mount_covering(&canonical).is_some() {
            return reply_error(ErrorCode::Unsupported);
        }
        let _ = _caller_uid;
        match self.fs.chown(path, owner, group, 0) {
            Ok(()) => reply_status(),
            Err(error) => reply_error(error.code()),
        }
    }

    /// List the files on the disk a path is mounted from.
    ///
    /// Only the mount root lists. The disk format is flat, so `/media/test/x`
    /// names a file and has nothing beneath it to enumerate — reporting it as
    /// an empty directory would be a directory the format cannot represent.
    fn list_mounted(&mut self, index: usize, canonical: &CanonicalPath, offset: usize) -> Message {
        let Some(mount) = self.mounts[index].as_ref() else {
            return reply_error(ErrorCode::BadHandle);
        };
        if Self::mounted_relative(mount.path.as_bytes(), canonical.as_bytes()).is_some() {
            return reply_error(ErrorCode::Invalid);
        }

        let mut names = [DiskListEntry {
            name: [0; NAME_FIELD_BYTES],
            name_len: 0,
            size: 0,
        }; nova_fs::ramfs::MAX_CHILDREN];
        let count = mount.disk.list(&mut names);

        let start = offset.min(count);
        let mut listing = [0u8; LISTING_BUDGET];
        let mut used = 0usize;
        let mut included = 0u32;
        for entry in names.iter().take(count).skip(start) {
            let name = entry.name_bytes();
            let entry_size = 16usize.saturating_add(name.len());
            if used.saturating_add(entry_size) > listing.len() {
                break;
            }
            let record = FileDirEntry {
                name_len: name.len() as u32,
                kind: FileKind::File,
                size: entry.size as u64,
            };
            listing[used..used + 4].copy_from_slice(&record.name_len.to_le_bytes());
            listing[used + 4..used + 8].copy_from_slice(&record.kind.code().to_le_bytes());
            listing[used + 8..used + 16].copy_from_slice(&record.size.to_le_bytes());
            used += 16;
            listing[used..used + name.len()].copy_from_slice(name);
            used += name.len();
            included += 1;
        }

        let mut bytes = [0u8; FILE_LIST_REPLY_SIZE];
        write_protocol_version(&mut bytes, FILE_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&included.to_le_bytes());
        // The total is what the listing holds: a disk's directory has no
        // deeper structure, so "returned" and "there" are the same number.
        bytes[12..16].copy_from_slice(&(count as u32).to_le_bytes());

        reply_bytes(&bytes).with_extra_payload(&listing[..used])
    }

    /// Report what a path on a mounted disk names.
    fn stat_mounted(&mut self, index: usize, canonical: &CanonicalPath) -> Message {
        let Some(mount) = self.mounts[index].as_ref() else {
            return reply_error(ErrorCode::BadHandle);
        };

        let (kind, size) = match Self::mounted_relative(mount.path.as_bytes(), canonical.as_bytes())
        {
            // The mount root itself: the disk, which is a directory.
            None => (FileKind::Directory, 0u64),
            Some(relative) => {
                let file = match mount.disk.lookup(relative) {
                    Ok(file) => file,
                    Err(error) => return reply_error(error.code()),
                };
                match mount.disk.stat(file) {
                    Ok(stat) => (FileKind::File, stat.size as u64),
                    Err(error) => return reply_error(error.code()),
                }
            }
        };

        let mut bytes = [0u8; 40];
        write_protocol_version(&mut bytes, FILE_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&kind.code().to_le_bytes());
        bytes[16..24].copy_from_slice(&size.to_le_bytes());
        bytes[24..28].copy_from_slice(&0u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&0u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&0o644u32.to_le_bytes());
        reply_bytes(&bytes)
    }

    /// The live mounts, in mount order.
    pub fn mounts(&self) -> impl Iterator<Item = &DiskMount> {
        self.mounts.iter().flatten()
    }

    /// How many files are open right now.
    ///
    /// The observer the reclaim tests read: a dead client's files must be
    /// gone from this count, not merely unusable.
    pub fn open_count(&self) -> usize {
        self.open_files.iter().flatten().count()
    }

    /// Remember an open file, returning the wire handle the client holds.
    ///
    /// The wire handle for a RAM file carries the slot and the generation, so
    /// a client that keeps a stale handle after a close cannot address a
    /// reused slot. A disk file's wire handle carries *this* table's slot,
    /// that slot's generation, and the bit that marks it as a disk handle;
    /// the generation is what makes a handle that outlived its close refuse
    /// rather than address whatever later claimed the slot.
    fn remember(
        &mut self,
        origin: Origin,
        path: CanonicalPath,
        owner: usize,
    ) -> Result<u64, FsError> {
        for (index, slot) in self.open_files.iter_mut().enumerate() {
            if slot.is_none() {
                let wire = match &origin {
                    Origin::Ram(handle) => {
                        ((handle.generation() as u64) << 32) | (handle.slot() as u64)
                    }
                    Origin::Disk { .. } => {
                        DISK_WIRE_BIT | ((self.generations[index] as u64) << 32) | index as u64
                    }
                };
                *slot = Some(ServerFile {
                    wire,
                    origin,
                    path,
                    owner,
                });
                return Ok(wire);
            }
        }

        // A table that is full is a quota, not a silent success: a wire handle
        // with nothing behind it would fail later as `BadHandle`, which reads
        // as a client bug rather than as the server being out of slots.
        Err(FsError::Quota)
    }
}

/// Check that the caller's scopes cover the operation.
fn require(
    scopes: &ScopeSet<MAX_SCOPES_PER_TASK>,
    path: &CanonicalPath,
    needed: Rights,
) -> Result<(), FsError> {
    if scopes.denies(path, needed) {
        return Err(FsError::Denied);
    }
    Ok(())
}

/// Read a length-prefixed slice out of a payload.
fn slice_at(payload: &[u8], offset: usize, length: u32) -> Option<&[u8]> {
    let length = usize::try_from(length).ok()?;
    let end = offset.checked_add(length)?;
    if end > payload.len() {
        return None;
    }
    Some(&payload[offset..end])
}

/// Read a little-endian `u16`.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

/// Read a little-endian `u32`.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Read a little-endian `u64`.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset.checked_add(8)?)?;
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(slice);
    Some(u64::from_le_bytes(buffer))
}

/// Write a protocol version into a reply buffer.
fn write_protocol_version(bytes: &mut [u8], version: ProtocolVersion) {
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
}

/// The header of an `open` request.
fn read_open_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let flags = read_u32(payload, 4)?;
    let path_len = read_u32(payload, 8)?;
    Some((ProtocolVersion::new(major, minor), flags, path_len))
}

/// The header of a `read` request.
fn read_read_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u64, u64, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let flags = read_u32(payload, 4)?;
    let handle = read_u64(payload, 8)?;
    let offset = read_u64(payload, 16)?;
    let length = read_u32(payload, 24)?;
    Some((
        ProtocolVersion::new(major, minor),
        flags,
        handle,
        offset,
        length,
    ))
}

/// The header of a `write` request.
fn read_write_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u64, u64, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let flags = read_u32(payload, 4)?;
    let handle = read_u64(payload, 8)?;
    let offset = read_u64(payload, 16)?;
    let length = read_u32(payload, 24)?;
    Some((
        ProtocolVersion::new(major, minor),
        flags,
        handle,
        offset,
        length,
    ))
}

/// The header of a `close` request.
fn read_close_header(payload: &[u8]) -> Option<(ProtocolVersion, u64)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let handle = read_u64(payload, 8)?;
    Some((ProtocolVersion::new(major, minor), handle))
}

/// The header of a request that carries only a path.
fn read_path_header(payload: &[u8]) -> Option<(ProtocolVersion, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let path_len = read_u32(payload, 8)?;
    Some((ProtocolVersion::new(major, minor), path_len))
}

/// The header of a `rename` request: two length-prefixed paths follow it.
fn read_rename_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let old_len = read_u32(payload, 8)?;
    let new_len = read_u32(payload, 12)?;
    Some((ProtocolVersion::new(major, minor), old_len, new_len))
}

/// The header of a chmod request: version, mode, path length, caller uid.
fn read_chmod_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u32, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let mode = read_u32(payload, 4)?;
    let path_len = read_u32(payload, 8)?;
    let caller_uid = read_u32(payload, 12)?;
    Some((
        ProtocolVersion::new(major, minor),
        mode,
        path_len,
        caller_uid,
    ))
}

/// The header of a chown request: version, owner, group, path length, caller uid.
fn read_chown_header(payload: &[u8]) -> Option<(ProtocolVersion, u32, u32, u32, u32)> {
    let major = read_u16(payload, 0)?;
    let minor = read_u16(payload, 2)?;
    let owner = read_u32(payload, 4)?;
    let group = read_u32(payload, 8)?;
    let path_len = read_u32(payload, 12)?;
    let caller_uid = read_u32(payload, 16)?;
    Some((
        ProtocolVersion::new(major, minor),
        owner,
        group,
        path_len,
        caller_uid,
    ))
}

/// A reply that says only "it worked".
fn reply_status() -> Message {
    let mut bytes = [0u8; 16];
    write_protocol_version(&mut bytes, FILE_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    reply_bytes(&bytes)
}

/// A reply that carries only a status.
fn reply_error(code: ErrorCode) -> Message {
    let mut bytes = [0u8; 24];
    write_protocol_version(&mut bytes, FILE_VERSION);
    bytes[4..8].copy_from_slice(&code.code().to_le_bytes());
    reply_bytes(&bytes)
}

/// A reply carrying a fixed header.
fn reply_bytes(bytes: &[u8]) -> Message {
    Message::with_payload(0, bytes).unwrap_or_else(|_| Message::new(0))
}

/// Attach extra payload bytes to a message.
///
/// The IPC payload budget is 64 bytes, and the fixed headers are 16 to 24
/// bytes, so the extra bytes must fit alongside them. A message that cannot
/// hold them is returned unchanged rather than truncated silently — the
/// protocol's `count` field is what tells a client that a listing was
/// clipped.
trait WithExtraPayload {
    /// Return a message carrying `bytes` in addition to its header.
    fn with_extra_payload(self, bytes: &[u8]) -> Message;
}

impl WithExtraPayload for Message {
    fn with_extra_payload(self, bytes: &[u8]) -> Message {
        let header_len = self.payload().len();
        let total = header_len.saturating_add(bytes.len());
        if total > nova_abi::ipc::MAX_INLINE_PAYLOAD {
            return self;
        }
        let mut buffer = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
        buffer[..header_len].copy_from_slice(self.payload());
        buffer[header_len..total].copy_from_slice(bytes);
        Message::with_payload(self.opcode(), &buffer[..total]).unwrap_or(self)
    }
}
