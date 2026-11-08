//! A sector-backed filesystem for mounted disks (the M5 mount).
//!
//! # What this is
//!
//! `ramfs` is a store that lives in kernel memory. This module is the store
//! that lives *on a block device*: the layout the kernel reads from a disk it
//! did not write, which is what "mount" means in VFS terms. The kernel
//! supplies the device ([`BlockDevice`]); the layout, the bounds, and the
//! refusal rules live here so the host test suite can exercise them without a
//! machine.
//!
//! # The image layout, in full
//!
//! Every number below is little-endian, and every region is measured in
//! 512-byte sectors.
//!
//! ```text
//! sector 0        superblock
//! dir_start       directory: slot_count * 64 bytes, zero-padded to sectors
//! data_start      slot 0's data: slot_sectors sectors
//!                 slot 1's data: slot_sectors sectors
//!                 ...
//! scratch_start   reserved test area, [scratch_start, total_sectors)
//! total_sectors   end of device as the image sees it
//! ```
//!
//! Superblock, byte offsets within sector 0:
//!
//! | offset | size | field |
//! | --- | --- | --- |
//! | 0 | 8 | magic `NOVADSK\0` |
//! | 8 | 4 | format version |
//! | 12 | 4 | slot count (1..=MAX_SLOTS) |
//! | 16 | 4 | sectors per slot (the file capacity) |
//! | 20 | 4 | first directory sector |
//! | 24 | 4 | directory sectors |
//! | 28 | 4 | first data sector |
//! | 32 | 4 | first scratch (test) sector |
//! | 36 | 4 | total sectors |
//!
//! Directory entry, 64 bytes: name (32, NUL-terminated), length (4), capacity
//! (4), start sector (8), reserved (16). An entry whose name begins with NUL
//! is a **free slot**: the format has no allocator, so "free" is exactly
//! "unclaimed", and creating a file claims the lowest unclaimed slot.
//!
//! # Why there is no free-space bitmap
//!
//! Files occupy one fixed extent chosen at create time, so there is nothing to
//! coalesce or fragment: a slot is either claimed or it is not. A bitmap would
//! be a second source of truth about the same fact, and a second source of
//! truth about allocation is how a filesystem hands out the same sector twice.
//!
//! # The image is untrusted input
//!
//! [`DiskFs::mount`] validates the superblock *and every live directory entry*
//! before it serves a single byte: magic, version, geometry (checked
//! arithmetic all the way down), extent alignment, duplicate names, and a
//! length that fits its capacity. An image that fails any of that is refused
//! as a whole — a half-trusted filesystem would resolve paths against entries
//! the kernel already knows are inconsistent.
//!
//! # Write ordering
//!
//! Writes are **write-through**: the device acknowledges each sector as it is
//! written, and a later read observes it. Durability across a power loss is a
//! different promise and this format does not make it: a caller that needs it
//! calls [`DiskFs::flush`], which reaches the device's flush only after every
//! prior write has been submitted. That distinction is the one
//! `reports/NovaOS.md` insists on — "a write returned success" is not "the
//! write is on the platter".

use crate::path::PathError;
use crate::ramfs::FsError;

/// Bytes in one sector.
pub const SECTOR_BYTES: usize = 512;

/// The magic that identifies a NovaOS disk image.
///
/// Distinct from the initramfs archive magic on purpose: the two formats are
/// unrelated, and a shared magic would let an initramfs archive be mounted as a
/// disk (or the reverse) right up until the first field that disagreed.
pub const MAGIC: [u8; 8] = *b"NOVADSK\0";

/// The format version this code reads.
pub const FORMAT_VERSION: u32 = 1;

/// The sector the superblock occupies.
pub const SUPERBLOCK_SECTOR: u64 = 0;

/// File slots in an image. 8 slots of 64 bytes is exactly one sector.
pub const MAX_SLOTS: usize = 8;

/// Longest file name, in bytes.
///
/// 31 rather than 32 because the on-disk field is NUL-terminated: a name that
/// filled the field would have no terminator, and the directory would then be
/// ambiguous about where the name ends.
pub const MAX_FILE_NAME: usize = 31;

/// Bytes in one directory entry.
pub const DIR_ENTRY_BYTES: usize = 64;

/// Bytes in the name field of a directory entry.
pub const NAME_FIELD_BYTES: usize = 32;

/// Bytes of directory this code reads — one sector.
///
/// [`MAX_SLOTS`] entries of [`DIR_ENTRY_BYTES`] is 512, so one sector holds
/// the whole directory and that is the bound the mount validates against
/// rather than a window sized for a format this is not.
pub const DIR_CACHE_BYTES: usize = SECTOR_BYTES;

/// Byte offsets of the superblock's fields.
mod superblock_offsets {
    pub const VERSION: usize = 8;
    pub const SLOT_COUNT: usize = 12;
    pub const SLOT_SECTORS: usize = 16;
    pub const DIR_START: usize = 20;
    pub const DIR_SECTORS: usize = 24;
    pub const DATA_START: usize = 28;
    pub const SCRATCH_START: usize = 32;
    pub const TOTAL_SECTORS: usize = 36;
}

/// Byte offsets of a directory entry's fields.
mod entry_offsets {
    pub const LENGTH: usize = 32;
    pub const CAPACITY: usize = 36;
    pub const START: usize = 40;
}

/// A failure to reach the block device beneath the filesystem.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceError {
    /// The device is read-only and a write was attempted.
    ReadOnly,
    /// The device did not complete the request before the driver's deadline.
    Timeout,
    /// The device is absent, reset, or refused to acknowledge the driver.
    Unavailable,
    /// The device answered, but not in a way the driver could accept — a
    /// bogus descriptor id, a short response, a status byte out of range.
    Fault,
    /// The request named a sector the device does not have.
    OutOfRange,
}

impl DeviceError {
    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            DeviceError::ReadOnly => "D_READ_ONLY",
            DeviceError::Timeout => "D_TIMEOUT",
            DeviceError::Unavailable => "D_UNAVAILABLE",
            DeviceError::Fault => "D_FAULT",
            DeviceError::OutOfRange => "D_OUT_OF_RANGE",
        }
    }
}

impl core::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// The sector device the filesystem sits on.
///
/// One sector per call is deliberate. The filesystem bounds every transfer to a
/// sector, so a device that could serve megabytes in one call would only make
/// the *caller's* length arithmetic the thing that has to be right — and that
/// arithmetic is exactly where an out-of-bounds write would come from.
pub trait BlockDevice {
    /// Total sectors the device holds.
    fn sector_count(&self) -> u64;

    /// Read one sector.
    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR_BYTES]) -> Result<(), DeviceError>;

    /// Write one sector.
    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR_BYTES]) -> Result<(), DeviceError>;

    /// Make prior writes durable, then report whether they are.
    fn flush(&mut self) -> Result<(), DeviceError>;
}

/// Why a mount was refused.
///
/// A refusal, not a degraded mount: an image whose geometry does not add up is
/// an image whose file extents cannot be trusted, and serving it partially
/// would be a read of memory the format never described.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountError {
    /// The device could not be read.
    Device(DeviceError),
    /// The magic was not [`MAGIC`].
    BadMagic,
    /// The format version is not [`FORMAT_VERSION`].
    UnsupportedVersion,
    /// The geometry fields did not describe a self-consistent image.
    BadGeometry,
    /// A live directory entry disagreed with the geometry.
    BadEntry,
    /// Two entries claimed the same name.
    DuplicateName,
}

impl MountError {
    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            MountError::Device(error) => error.name(),
            MountError::BadMagic => "M_BAD_MAGIC",
            MountError::UnsupportedVersion => "M_UNSUPPORTED_VERSION",
            MountError::BadGeometry => "M_BAD_GEOMETRY",
            MountError::BadEntry => "M_BAD_ENTRY",
            MountError::DuplicateName => "M_DUPLICATE_NAME",
        }
    }
}

impl core::fmt::Display for MountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MountError::Device(error) => write!(f, "mount device: {error}"),
            other => f.write_str(other.name()),
        }
    }
}

/// The validated geometry of an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Superblock {
    /// Sectors per file slot; the capacity of every file is this times 512.
    pub slot_sectors: usize,
    /// File slots.
    pub slot_count: usize,
    /// First directory sector.
    pub dir_start: u64,
    /// First data sector — slot 0's first byte.
    pub data_start: u64,
    /// First sector of the reserved test area.
    pub scratch_start: u64,
    /// Total sectors the image claims.
    pub total_sectors: u64,
}

impl Superblock {
    /// First sector past every slot's data.
    ///
    /// # Overflow
    ///
    /// Never at runtime: [`parse_superblock`] refuses any geometry whose
    /// products do not fit, so this add cannot wrap. It is written as a
    /// checked add anyway, because a `Superblock` that reached this point
    /// through some other path would otherwise wrap silently.
    pub const fn data_end(&self) -> u64 {
        let slots = self.slot_count.saturating_mul(self.slot_sectors);
        self.data_start.saturating_add(slots as u64)
    }

    /// The capacity of one file, in bytes.
    pub const fn capacity(&self) -> usize {
        self.slot_sectors.saturating_mul(SECTOR_BYTES)
    }

    /// The reserved test area, as a half-open sector range.
    ///
    /// `None` when the range is empty: an image with no scratch area has no
    /// place a *test* may write, and saying so is better than returning an
    /// empty range that reads as "writable, zero length".
    pub const fn scratch_range(&self) -> Option<(u64, u64)> {
        if self.scratch_start < self.total_sectors {
            Some((self.scratch_start, self.total_sectors))
        } else {
            None
        }
    }
}

/// Parse and validate the superblock in sector 0.
///
/// Every field is checked against every other before it is returned: a
/// superblock whose numbers are individually fine and jointly impossible is
/// still a refusal.
pub fn parse_superblock(bytes: &[u8; SECTOR_BYTES]) -> Result<Superblock, MountError> {
    if bytes[..8] != MAGIC {
        return Err(MountError::BadMagic);
    }

    let version = read_u32(bytes, superblock_offsets::VERSION);
    if version != FORMAT_VERSION {
        return Err(MountError::UnsupportedVersion);
    }

    let slot_count = read_u32(bytes, superblock_offsets::SLOT_COUNT) as usize;
    let slot_sectors = read_u32(bytes, superblock_offsets::SLOT_SECTORS) as usize;
    let dir_start = read_u32(bytes, superblock_offsets::DIR_START) as u64;
    let dir_sectors = read_u32(bytes, superblock_offsets::DIR_SECTORS) as usize;
    let data_start = read_u32(bytes, superblock_offsets::DATA_START) as u64;
    let scratch_start = read_u32(bytes, superblock_offsets::SCRATCH_START) as u64;
    let total_sectors = read_u32(bytes, superblock_offsets::TOTAL_SECTORS) as u64;

    if slot_count == 0 || slot_count > MAX_SLOTS || slot_sectors == 0 {
        return Err(MountError::BadGeometry);
    }

    // The directory must be big enough for every slot and small enough to fit
    // the cache this code reads it into. Both bounds are checked rather than
    // assumed, because both are read from the disk.
    let entries_bytes = slot_count
        .checked_mul(DIR_ENTRY_BYTES)
        .ok_or(MountError::BadGeometry)?;
    let required_dir_sectors = entries_bytes
        .checked_add(SECTOR_BYTES - 1)
        .ok_or(MountError::BadGeometry)?
        / SECTOR_BYTES;
    if dir_sectors < required_dir_sectors || dir_sectors * SECTOR_BYTES > DIR_CACHE_BYTES {
        return Err(MountError::BadGeometry);
    }

    if dir_start < 1 {
        return Err(MountError::BadGeometry);
    }
    let dir_end = dir_start
        .checked_add(dir_sectors as u64)
        .ok_or(MountError::BadGeometry)?;
    if dir_end > data_start {
        return Err(MountError::BadGeometry);
    }

    let data_bytes = slot_count
        .checked_mul(slot_sectors)
        .ok_or(MountError::BadGeometry)?;
    let data_end = data_start
        .checked_add(data_bytes as u64)
        .ok_or(MountError::BadGeometry)?;
    if data_end > scratch_start || scratch_start > total_sectors {
        return Err(MountError::BadGeometry);
    }

    Ok(Superblock {
        slot_sectors,
        slot_count,
        dir_start,
        data_start,
        scratch_start,
        total_sectors,
    })
}

/// One directory entry, as the filesystem holds it in memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    /// Whether this slot is claimed by a file.
    used: bool,
    /// The name, NUL-padded, of a used slot.
    name: [u8; NAME_FIELD_BYTES],
    /// The name's length.
    name_len: usize,
    /// Bytes currently in the file.
    length: usize,
    /// The slot's byte capacity.
    capacity: usize,
    /// First sector of the slot's extent.
    start: u64,
}

impl Entry {
    /// A free slot.
    const fn free() -> Self {
        Self {
            used: false,
            name: [0; NAME_FIELD_BYTES],
            name_len: 0,
            length: 0,
            capacity: 0,
            start: 0,
        }
    }

    /// The name as bytes.
    fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// A handle to a file on a mounted disk.
///
/// The slot index *is* the identity: slots are not reused while a file holds
/// the name, and deletion is not part of this format, so unlike a
/// RAM-filesystem handle there is no generation to carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiskFileId {
    slot: usize,
}

/// What [`DiskFs::stat`] reports about one file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiskStat {
    /// Bytes currently in the file.
    pub size: usize,
    /// Bytes the file may grow to.
    pub capacity: usize,
}

/// One file in a [`DiskFs::list`] result.
#[derive(Clone, Copy, Debug)]
pub struct DiskListEntry {
    /// The name, NUL-padded.
    pub name: [u8; NAME_FIELD_BYTES],
    /// The name's length.
    pub name_len: usize,
    /// Bytes in the file.
    pub size: usize,
}

impl DiskListEntry {
    /// The name as bytes.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// A canonical path, split for a flat filesystem.
#[derive(PartialEq, Eq, Debug)]
enum Rooted<'a> {
    /// The disk root itself — the mount point, which lists rather than opens.
    Root,
    /// A single component: `/name`.
    Leaf(&'a [u8]),
    /// Two or more components: `/first/rest`, where `first` decides which
    /// error applies.
    Nested(&'a [u8]),
}

/// A mounted, sector-backed filesystem.
///
/// Generic over the device so the host tests can drive it from an array and
/// the kernel can drive it from VirtIO Block, with every rule below shared
/// between the two.
pub struct DiskFs<D: BlockDevice> {
    device: D,
    geometry: Superblock,
    /// The directory, cached and authoritative in memory.
    entries: [Entry; MAX_SLOTS],
    /// Whether writes are refused at the filesystem's own door.
    read_only: bool,
}

impl<D: BlockDevice> DiskFs<D> {
    /// Mount `device`, validating the image before serving anything from it.
    ///
    /// `read_only` is a *mount* flag, independent of the device: an image on a
    /// writable device can be mounted read-only, which is what makes the
    /// read-only guarantee testable without unplugging the disk.
    pub fn mount(mut device: D, read_only: bool) -> Result<Self, MountError> {
        let mut sector = [0u8; SECTOR_BYTES];
        device
            .read_sector(SUPERBLOCK_SECTOR, &mut sector)
            .map_err(MountError::Device)?;
        let geometry = parse_superblock(&sector)?;

        if device.sector_count() < geometry.total_sectors {
            return Err(MountError::BadGeometry);
        }

        device
            .read_sector(geometry.dir_start, &mut sector)
            .map_err(MountError::Device)?;

        let mut entries = [Entry::free(); MAX_SLOTS];
        for (slot, entry) in entries.iter_mut().enumerate().take(geometry.slot_count) {
            let offset = slot
                .checked_mul(DIR_ENTRY_BYTES)
                .ok_or(MountError::BadGeometry)?;
            let end = offset
                .checked_add(DIR_ENTRY_BYTES)
                .ok_or(MountError::BadGeometry)?;
            if end > DIR_CACHE_BYTES {
                return Err(MountError::BadGeometry);
            }

            let mut record = [0u8; DIR_ENTRY_BYTES];
            record.copy_from_slice(&sector[offset..end]);
            *entry = parse_entry(&record, &geometry, slot)?;
        }

        // Duplicate names are ambiguous, not merely untidy: the first match
        // would win a lookup the image never intended to be decidable.
        for (slot, entry) in entries.iter().enumerate() {
            if !entry.used {
                continue;
            }
            for other in entries.iter().skip(slot.saturating_add(1)) {
                if other.used && other.name_bytes() == entry.name_bytes() {
                    return Err(MountError::DuplicateName);
                }
            }
        }

        Ok(Self {
            device,
            geometry,
            entries,
            read_only,
        })
    }

    /// Whether this mount refuses writes.
    pub const fn read_only(&self) -> bool {
        self.read_only
    }

    /// The image's validated geometry.
    pub const fn geometry(&self) -> &Superblock {
        &self.geometry
    }

    /// The device's total sectors, as the filesystem sees them.
    pub fn sector_count(&self) -> u64 {
        self.device.sector_count()
    }

    /// Resolve a canonical absolute path to a file on this disk.
    ///
    /// The disk is flat: `/name` resolves, `/` is the root (and is not a
    /// file), and anything deeper reports [`FsError::NotADirectory`] when the
    /// first component names a file and [`FsError::NotFound`] when it names
    /// nothing — the two differences a caller can act on.
    pub fn lookup(&self, path: &[u8]) -> Result<DiskFileId, FsError> {
        match split_rooted(path)? {
            Rooted::Root => Err(FsError::IsADirectory),
            Rooted::Leaf(name) => {
                let index = self.find_name(name).ok_or(FsError::NotFound)?;
                Ok(DiskFileId { slot: index })
            }
            Rooted::Nested(first) => match self.find_name(first) {
                Some(_) => Err(FsError::NotADirectory),
                None => Err(FsError::NotFound),
            },
        }
    }

    /// The number of files currently present.
    pub fn file_count(&self) -> usize {
        self.entries[..self.geometry.slot_count]
            .iter()
            .filter(|entry| entry.used)
            .count()
    }

    /// What one file holds.
    pub fn stat(&self, id: DiskFileId) -> Result<DiskStat, FsError> {
        let entry = self.entry(id)?;
        Ok(DiskStat {
            size: entry.length,
            capacity: entry.capacity,
        })
    }

    /// Fill `out` with the files present, returning how many were written.
    ///
    /// The count stops at `out.len()`: a listing that ran past its buffer to
    /// report the true total would be the buffer overrun this bound exists to
    /// prevent.
    pub fn list(&self, out: &mut [DiskListEntry]) -> usize {
        let mut count = 0usize;
        for entry in self.entries[..self.geometry.slot_count].iter() {
            if !entry.used || count == out.len() {
                continue;
            }
            out[count] = DiskListEntry {
                name: entry.name,
                name_len: entry.name_len,
                size: entry.length,
            };
            count = count.saturating_add(1);
        }
        count
    }

    /// Read from an open file, returning how many bytes were produced.
    ///
    /// A read at or past the end returns `Ok(0)` — end of file, not an error —
    /// and a read that starts inside and runs past the end is truncated to the
    /// file's length. Both are checked against the *validated* length, so the
    /// sector arithmetic below can never name a sector outside the slot's
    /// extent.
    pub fn read(
        &mut self,
        id: DiskFileId,
        offset: usize,
        out: &mut [u8],
    ) -> Result<usize, FsError> {
        let entry = *self.entry(id)?;
        if offset >= entry.length {
            return Ok(0);
        }
        let wanted = out.len().min(entry.length.saturating_sub(offset));

        let mut done = 0usize;
        while done < wanted {
            let position = offset.checked_add(done).ok_or(FsError::InvalidArgument)?;
            let sector = entry.start.saturating_add((position / SECTOR_BYTES) as u64);
            let within = position % SECTOR_BYTES;
            let take = SECTOR_BYTES
                .saturating_sub(within)
                .min(wanted.saturating_sub(done));

            let mut block = [0u8; SECTOR_BYTES];
            self.device
                .read_sector(sector, &mut block)
                .map_err(map_device_error)?;
            out[done..done.saturating_add(take)]
                .copy_from_slice(&block[within..within.saturating_add(take)]);
            done = done.saturating_add(take);
        }

        Ok(done)
    }

    /// Read a whole file by path into `out`.
    ///
    /// For callers — the mount tests, and stat-then-read paths — that want
    /// the file's bytes without resolving a handle and tracking an offset
    /// themselves.
    pub fn read_path(&mut self, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
        let id = self.lookup(path)?;
        self.read(id, 0, out)
    }

    /// Write into an open file, returning how many bytes were stored.
    ///
    /// The write may extend the file up to its slot capacity; beyond that it
    /// is [`FsError::Quota`], because a slot's extent is fixed at create time
    /// and writing past it would land in the next slot's data — the exact
    /// out-of-bounds write the format's validation exists to rule out.
    pub fn write(&mut self, id: DiskFileId, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }

        let mut entry = *self.entry(id)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(FsError::InvalidArgument)?;
        if end > entry.capacity {
            return Err(FsError::Quota);
        }

        let mut done = 0usize;
        while done < data.len() {
            let position = offset.checked_add(done).ok_or(FsError::InvalidArgument)?;
            let sector = entry.start.saturating_add((position / SECTOR_BYTES) as u64);
            let within = position % SECTOR_BYTES;
            let take = SECTOR_BYTES
                .saturating_sub(within)
                .min(data.len().saturating_sub(done));

            // A partial sector has to go back the way it came: writing only
            // `data` over the whole sector would zero the bytes on either
            // side of the write.
            let mut block = [0u8; SECTOR_BYTES];
            if within != 0 || take != SECTOR_BYTES {
                self.device
                    .read_sector(sector, &mut block)
                    .map_err(map_device_error)?;
            }
            block[within..within.saturating_add(take)]
                .copy_from_slice(&data[done..done.saturating_add(take)]);
            self.device
                .write_sector(sector, &block)
                .map_err(map_device_error)?;
            done = done.saturating_add(take);
        }

        if end > entry.length {
            entry.length = end;
            self.entries[id.slot] = entry;
            self.write_back_directory()?;
        }

        Ok(data.len())
    }

    /// Create a file at `path`, returning its handle.
    ///
    /// The name and the path are validated *before* a slot is claimed, so a
    /// refused create leaves the directory exactly as it was.
    pub fn create(&mut self, path: &[u8]) -> Result<DiskFileId, FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }

        let name = match split_rooted(path)? {
            Rooted::Root => return Err(FsError::IsADirectory),
            Rooted::Leaf(name) => name,
            Rooted::Nested(first) => {
                return Err(match self.find_name(first) {
                    Some(_) => FsError::NotADirectory,
                    None => FsError::NotFound,
                });
            }
        };
        validate_name(name)?;

        if self.find_name(name).is_some() {
            return Err(FsError::AlreadyExists);
        }

        let slot = self.entries[..self.geometry.slot_count]
            .iter()
            .position(|entry| !entry.used)
            .ok_or(FsError::Quota)?;

        let mut entry = Entry::free();
        entry.used = true;
        entry.name[..name.len()].copy_from_slice(name);
        entry.name_len = name.len();
        entry.length = 0;
        entry.capacity = self.geometry.capacity();
        entry.start = self
            .geometry
            .data_start
            .saturating_add(slot.saturating_mul(self.geometry.slot_sectors) as u64);

        self.entries[slot] = entry;
        self.write_back_directory()?;
        Ok(DiskFileId { slot })
    }

    /// Make every prior write durable.
    pub fn flush(&mut self) -> Result<(), FsError> {
        self.device.flush().map_err(map_device_error)
    }

    /// Delete the file at `path`.
    ///
    /// The slot is freed by zeroing its name and length — the two fields a
    /// free slot must carry — and the directory sector is rewritten once, so
    /// the on-medium image never names a file the memory has forgotten or
    /// the other way around. A crash before the rewrite leaves the file
    /// exactly as it was: the old directory is still consistent with the
    /// old data, so deletion is a step the medium either took whole or not
    /// at all.
    ///
    /// The file's data blocks are not erased: the slot's extent stays owned
    /// by the slot, and the next `create` that claims the slot overwrites
    /// them. What the delete must guarantee is only that no *name* reaches
    /// the stale bytes.
    ///
    /// A handle a caller still holds becomes [`FsError::BadHandle`] on its
    /// next use — the slot it names is no longer claimed. The identity is
    /// checked against the directory rather than remembered, so there is no
    /// window in which a deleted file is still writable.
    pub fn delete(&mut self, path: &[u8]) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }

        let name = match split_rooted(path)? {
            Rooted::Root => return Err(FsError::IsADirectory),
            Rooted::Leaf(name) => name,
            Rooted::Nested(first) => {
                return Err(match self.find_name(first) {
                    Some(_) => FsError::NotADirectory,
                    None => FsError::NotFound,
                });
            }
        };

        let slot = self.find_name(name).ok_or(FsError::NotFound)?;
        self.entries[slot] = Entry::free();
        self.write_back_directory()?;
        Ok(())
    }

    /// Rename the file at `from` to `to`.
    ///
    /// A rename changes one field of one directory entry — the name — and
    /// the slot, and therefore the data extent behind the file, does not
    /// move. The target name is validated and checked for a collision
    /// *before* the entry is touched, so a refused rename leaves the
    /// directory exactly as it was; and the rewrite that publishes the new
    /// name is a single sector write, so a crash between the two leaves the
    /// file under its old name rather than under a half-written one.
    ///
    /// Handles a caller already holds stay valid and now name the file's
    /// new path: the handle's identity is the slot, not the name — the same
    /// semantics a UNIX rename gives an open descriptor.
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }

        let old = match split_rooted(from)? {
            Rooted::Root => return Err(FsError::IsADirectory),
            Rooted::Leaf(name) => name,
            Rooted::Nested(first) => {
                return Err(match self.find_name(first) {
                    Some(_) => FsError::NotADirectory,
                    None => FsError::NotFound,
                });
            }
        };
        let new = match split_rooted(to)? {
            Rooted::Root => return Err(FsError::IsADirectory),
            Rooted::Leaf(name) => name,
            Rooted::Nested(first) => {
                return Err(match self.find_name(first) {
                    Some(_) => FsError::NotADirectory,
                    None => FsError::NotFound,
                });
            }
        };
        validate_name(new)?;

        let slot = self.find_name(old).ok_or(FsError::NotFound)?;
        // `find_name` matches any live entry with the target name — including
        // the source itself, so a rename onto its own name is refused rather
        // than silently rewritten.
        if self.find_name(new).is_some() {
            return Err(FsError::AlreadyExists);
        }

        let mut entry = self.entries[slot];
        entry.name = [0u8; NAME_FIELD_BYTES];
        entry.name[..new.len()].copy_from_slice(new);
        entry.name_len = new.len();
        self.entries[slot] = entry;
        self.write_back_directory()?;
        Ok(())
    }

    /// Borrow the device, for callers that need the raw disk beside the files.
    pub fn device(&mut self) -> &mut D {
        &mut self.device
    }

    /// Take the device back, ending the filesystem.
    ///
    /// For callers that mounted a device they still own — the round-trip
    /// tests, which remount the same image to check that a change reached
    /// the medium rather than only the cache.
    pub fn into_device(self) -> D {
        self.device
    }

    /// The reserved test area as a sector range, if the image has one.
    pub fn scratch_range(&self) -> Option<(u64, u64)> {
        self.geometry.scratch_range()
    }

    /// One live directory entry.
    fn entry(&self, id: DiskFileId) -> Result<&Entry, FsError> {
        self.entries
            .get(id.slot)
            .filter(|entry| entry.used)
            .ok_or(FsError::BadHandle)
    }

    /// The slot index a name occupies, if any.
    fn find_name(&self, name: &[u8]) -> Option<usize> {
        self.entries[..self.geometry.slot_count]
            .iter()
            .position(|entry| entry.used && entry.name_bytes() == name)
    }

    /// Rewrite the directory sector from the in-memory entries.
    ///
    /// The whole sector is rebuilt rather than patched: the entries in memory
    /// *are* the directory, so rebuilding cannot disagree with itself, while a
    /// patch that mixed new bytes into a stale sector could.
    fn write_back_directory(&mut self) -> Result<(), FsError> {
        let mut block = [0u8; SECTOR_BYTES];
        for (index, entry) in self.entries[..self.geometry.slot_count].iter().enumerate() {
            let offset = index.saturating_mul(DIR_ENTRY_BYTES);
            let end = offset.saturating_add(DIR_ENTRY_BYTES);
            if end > DIR_CACHE_BYTES {
                return Err(FsError::Quota);
            }
            block[offset..offset.saturating_add(NAME_FIELD_BYTES)].copy_from_slice(&entry.name);

            let length_at = offset.saturating_add(entry_offsets::LENGTH);
            block[length_at..length_at.saturating_add(4)]
                .copy_from_slice(&(entry.length as u32).to_le_bytes());

            let capacity_at = offset.saturating_add(entry_offsets::CAPACITY);
            block[capacity_at..capacity_at.saturating_add(4)]
                .copy_from_slice(&(entry.capacity as u32).to_le_bytes());

            let start_at = offset.saturating_add(entry_offsets::START);
            block[start_at..start_at.saturating_add(8)].copy_from_slice(&entry.start.to_le_bytes());
        }

        self.device
            .write_sector(self.geometry.dir_start, &block)
            .map_err(map_device_error)
    }
}

/// Parse and validate one directory record against `geometry`.
fn parse_entry(
    record: &[u8; DIR_ENTRY_BYTES],
    geometry: &Superblock,
    slot: usize,
) -> Result<Entry, MountError> {
    let name_bytes = &record[..NAME_FIELD_BYTES];
    let Some(terminator) = name_bytes.iter().position(|byte| *byte == 0) else {
        // A name with no NUL does not end at the field, so nothing after it
        // can be interpreted.
        return Err(MountError::BadEntry);
    };

    if terminator == 0 {
        // A free slot. Its length must be zero: a file of any length in an
        // unclaimed slot is a directory that disagrees with itself.
        if read_u32(record, entry_offsets::LENGTH) != 0 {
            return Err(MountError::BadEntry);
        }
        return Ok(Entry::free());
    }

    let length = read_u32(record, entry_offsets::LENGTH) as usize;
    let capacity = read_u32(record, entry_offsets::CAPACITY) as usize;
    let start = read_u64(record, entry_offsets::START);

    if terminator > MAX_FILE_NAME {
        return Err(MountError::BadEntry);
    }
    if name_bytes[..terminator].contains(&b'/') {
        return Err(MountError::BadEntry);
    }
    if capacity != geometry.capacity() || length > capacity {
        return Err(MountError::BadEntry);
    }

    let expected_start = geometry
        .data_start
        .saturating_add(slot.saturating_mul(geometry.slot_sectors) as u64);
    if start != expected_start {
        // An extent that does not begin on its slot's boundary would read
        // another slot's data — or the directory itself.
        return Err(MountError::BadEntry);
    }

    let mut name = [0u8; NAME_FIELD_BYTES];
    name[..terminator].copy_from_slice(&name_bytes[..terminator]);
    Ok(Entry {
        used: true,
        name,
        name_len: terminator,
        length,
        capacity,
        start,
    })
}

/// Split a canonical absolute path for a flat filesystem.
///
/// Canonicalisation is the caller's job — every path reaching the filesystem
/// has already been through [`crate::path::canonicalize`] — so this only
/// classifies what is left: the root, one component, or a depth this format
/// has no directories for.
fn split_rooted(path: &[u8]) -> Result<Rooted<'_>, FsError> {
    if path.first() != Some(&b'/') {
        return Err(FsError::InvalidPath(PathError::NotAbsolute));
    }
    let body = &path[1..];
    if body.is_empty() {
        return Ok(Rooted::Root);
    }

    match body.iter().position(|byte| *byte == b'/') {
        None => Ok(Rooted::Leaf(body)),
        // `//...`: never produced by canonicalisation, so it is a path error
        // rather than an empty first component.
        Some(0) => Err(FsError::InvalidPath(PathError::Empty)),
        Some(slash) => Ok(Rooted::Nested(&body[..slash])),
    }
}

/// Check a candidate file name against the format's rules.
fn validate_name(name: &[u8]) -> Result<(), FsError> {
    if name.is_empty() {
        return Err(FsError::InvalidPath(PathError::Empty));
    }
    if name.len() > MAX_FILE_NAME {
        return Err(FsError::InvalidPath(PathError::ComponentTooLong));
    }
    if name.contains(&0) {
        return Err(FsError::InvalidPath(PathError::InteriorNul));
    }
    // `/` is checked even though `split_rooted` has already split on it: this
    // is the function a future caller (a rename, an import) would reach for,
    // and it must hold the rule itself rather than inherit it.
    if name.contains(&b'/') {
        return Err(FsError::InvalidPath(PathError::InteriorNul));
    }
    Ok(())
}

/// A device failure, as the filesystem reports it.
///
/// The ABI has no `EIO`, so most device faults collapse to
/// [`FsError::Device`]: the distinction survives in `Display`, where a log can
/// see it, and is lost at the syscall boundary — which is the ABI's
/// documented behaviour for every non-distinct code.
fn map_device_error(error: DeviceError) -> FsError {
    match error {
        DeviceError::ReadOnly => FsError::ReadOnly,
        other => FsError::Device(other),
    }
}

/// Read a little-endian `u32`.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let slice = &bytes[offset..offset.saturating_add(4)];
    u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
}

/// Read a little-endian `u64`.
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let slice = &bytes[offset..offset.saturating_add(8)];
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(slice);
    u64::from_le_bytes(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sectors the test device has.
    const DEVICE_SECTORS: usize = 160;

    /// A device backed by a fixed array: no allocator, a sector count the
    /// tests can reason about exactly, and counters that show whether a write
    /// reached the device at all.
    struct ArrayDevice {
        sectors: [u8; SECTOR_BYTES * DEVICE_SECTORS],
        reads: usize,
        writes: usize,
        flushes: usize,
        fail_read: Option<DeviceError>,
    }

    impl ArrayDevice {
        fn new() -> Self {
            Self {
                sectors: [0; SECTOR_BYTES * DEVICE_SECTORS],
                reads: 0,
                writes: 0,
                flushes: 0,
                fail_read: None,
            }
        }
    }

    impl BlockDevice for ArrayDevice {
        fn sector_count(&self) -> u64 {
            DEVICE_SECTORS as u64
        }

        fn read_sector(
            &mut self,
            lba: u64,
            out: &mut [u8; SECTOR_BYTES],
        ) -> Result<(), DeviceError> {
            if let Some(error) = self.fail_read {
                return Err(error);
            }
            if lba as usize >= DEVICE_SECTORS {
                return Err(DeviceError::OutOfRange);
            }
            self.reads = self.reads.saturating_add(1);
            let start = (lba as usize).saturating_mul(SECTOR_BYTES);
            out.copy_from_slice(&self.sectors[start..start.saturating_add(SECTOR_BYTES)]);
            Ok(())
        }

        fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR_BYTES]) -> Result<(), DeviceError> {
            if lba as usize >= DEVICE_SECTORS {
                return Err(DeviceError::OutOfRange);
            }
            self.writes = self.writes.saturating_add(1);
            let start = (lba as usize).saturating_mul(SECTOR_BYTES);
            self.sectors[start..start.saturating_add(SECTOR_BYTES)].copy_from_slice(data);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), DeviceError> {
            self.flushes = self.flushes.saturating_add(1);
            Ok(())
        }
    }

    /// A fresh image: 8 slots of 8 sectors, one directory sector at 1, data
    /// at 2 (ending at 66), scratch 66..160.
    fn image_device() -> ArrayDevice {
        let mut device = ArrayDevice::new();
        let mut block = [0u8; SECTOR_BYTES];
        block[0..8].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        block[12..16].copy_from_slice(&(MAX_SLOTS as u32).to_le_bytes());
        block[16..20].copy_from_slice(&8u32.to_le_bytes());
        block[20..24].copy_from_slice(&1u32.to_le_bytes());
        block[24..28].copy_from_slice(&1u32.to_le_bytes());
        block[28..32].copy_from_slice(&2u32.to_le_bytes());
        block[32..36].copy_from_slice(&66u32.to_le_bytes());
        block[36..40].copy_from_slice(&(DEVICE_SECTORS as u32).to_le_bytes());
        device.sectors[..SECTOR_BYTES].copy_from_slice(&block);
        device
    }

    /// Mount an image, as a statement rather than a repeated expression.
    fn mount(device: ArrayDevice, read_only: bool) -> DiskFs<ArrayDevice> {
        DiskFs::mount(device, read_only).expect("the test image mounts")
    }

    #[test]
    fn a_valid_superblock_parses() {
        let mut block = [0u8; SECTOR_BYTES];
        block[0..8].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        block[12..16].copy_from_slice(&8u32.to_le_bytes());
        block[16..20].copy_from_slice(&8u32.to_le_bytes());
        block[20..24].copy_from_slice(&1u32.to_le_bytes());
        block[24..28].copy_from_slice(&1u32.to_le_bytes());
        block[28..32].copy_from_slice(&2u32.to_le_bytes());
        block[32..36].copy_from_slice(&66u32.to_le_bytes());
        block[36..40].copy_from_slice(&(DEVICE_SECTORS as u32).to_le_bytes());

        let geometry = parse_superblock(&block).expect("a well-formed superblock parses");
        assert_eq!(geometry.slot_count, MAX_SLOTS);
        assert_eq!(geometry.capacity(), 4096);
        assert_eq!(geometry.data_end(), 66);
        assert_eq!(geometry.scratch_range(), Some((66, DEVICE_SECTORS as u64)));
    }

    #[test]
    fn a_foreign_magic_is_refused() {
        let block = [0u8; SECTOR_BYTES];
        assert_eq!(parse_superblock(&block), Err(MountError::BadMagic));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut block = [0u8; SECTOR_BYTES];
        block[0..8].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            parse_superblock(&block),
            Err(MountError::UnsupportedVersion)
        );
    }

    #[test]
    fn geometry_that_cannot_add_up_is_refused() {
        let mut block = [0u8; SECTOR_BYTES];
        block[0..8].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        block[12..16].copy_from_slice(&8u32.to_le_bytes());
        block[16..20].copy_from_slice(&8u32.to_le_bytes());
        // The directory *after* the data it describes.
        block[20..24].copy_from_slice(&100u32.to_le_bytes());
        block[24..28].copy_from_slice(&1u32.to_le_bytes());
        block[28..32].copy_from_slice(&2u32.to_le_bytes());
        block[32..36].copy_from_slice(&66u32.to_le_bytes());
        block[36..40].copy_from_slice(&(DEVICE_SECTORS as u32).to_le_bytes());

        assert_eq!(parse_superblock(&block), Err(MountError::BadGeometry));
    }

    #[test]
    fn a_slot_count_beyond_the_cache_is_refused() {
        let mut block = [0u8; SECTOR_BYTES];
        block[0..8].copy_from_slice(&MAGIC);
        block[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        block[12..16].copy_from_slice(&9u32.to_le_bytes());
        block[16..20].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(parse_superblock(&block), Err(MountError::BadGeometry));
    }

    #[test]
    fn mounting_reads_the_directory() {
        let mut fs = mount(image_device(), false);
        assert_eq!(fs.file_count(), 0);
        assert!(!fs.read_only());
        assert_eq!(fs.scratch_range(), Some((66, DEVICE_SECTORS as u64)));
        assert!(
            fs.device().reads >= 2,
            "a mount that never touched the device validated nothing"
        );
    }

    #[test]
    fn an_image_larger_than_the_device_is_refused() {
        let mut device = image_device();
        // Claim 1000 sectors while the device reports 160: the geometry does
        // not fit the hardware it claims to describe.
        device.sectors[36..40].copy_from_slice(&1000u32.to_le_bytes());

        let outcome: Result<(), MountError> = DiskFs::mount(device, false).map(|_| ());
        assert_eq!(outcome, Err(MountError::BadGeometry));
    }

    #[test]
    fn a_device_failure_is_reported_not_swallowed() {
        let mut device = image_device();
        device.fail_read = Some(DeviceError::Timeout);
        let outcome: Result<(), MountError> = DiskFs::mount(device, false).map(|_| ());
        assert_eq!(outcome, Err(MountError::Device(DeviceError::Timeout)));
    }

    #[test]
    fn an_entry_whose_extent_is_misaligned_is_refused() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/shifted.txt").expect("create");
        let mut device = fs.device;

        // Point slot 0's extent at the directory sector: a mount that accepted
        // it would serve the directory as file data.
        let dir = SECTOR_BYTES;
        let mut record = [0u8; DIR_ENTRY_BYTES];
        record.copy_from_slice(&device.sectors[dir..dir + DIR_ENTRY_BYTES]);
        record[40..48].copy_from_slice(&1u64.to_le_bytes());
        device.sectors[dir..dir + DIR_ENTRY_BYTES].copy_from_slice(&record);

        let outcome: Result<(), MountError> = DiskFs::mount(device, false).map(|_| ());
        assert_eq!(outcome, Err(MountError::BadEntry));
    }

    #[test]
    fn a_file_round_trips_through_create_write_read() {
        let mut fs = mount(image_device(), false);

        let id = fs.create(b"/hello.txt").expect("create");
        assert_eq!(fs.write(id, 0, b"hello disk").expect("write"), 10);

        let mut out = [0u8; 32];
        let count = fs.read(id, 0, &mut out).expect("read");
        assert_eq!(&out[..count], b"hello disk");
        assert_eq!(fs.stat(id).expect("stat").size, 10);
    }

    #[test]
    fn writes_reach_the_device_and_flush_counts_them() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/a.txt").expect("create");

        let writes_before = fs.device().writes;
        fs.write(id, 0, b"payload").expect("write");
        assert!(
            fs.device().writes > writes_before,
            "write-through means the device saw the sector, not just the cache"
        );

        fs.flush().expect("flush");
        assert_eq!(fs.device().flushes, 1);
    }

    #[test]
    fn a_write_extends_the_file_and_persists_the_length() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/log.txt").expect("create");
        fs.write(id, 0, b"first").expect("write");
        fs.write(id, 5, b"-second").expect("write");
        fs.flush().expect("flush");
        let device = fs.device;

        let mut fs = DiskFs::mount(device, false).expect("remount");
        let id = fs.lookup(b"/log.txt").expect("the file survived");
        let mut out = [0u8; 32];
        let count = fs.read(id, 0, &mut out).expect("read");
        assert_eq!(&out[..count], b"first-second");
        assert_eq!(fs.stat(id).expect("stat").size, 12);
    }

    #[test]
    fn a_partial_sector_write_keeps_the_bytes_around_it() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/page.bin").expect("create");
        fs.write(id, 0, &[0xAAu8; SECTOR_BYTES]).expect("fill");
        fs.write(id, 100, &[0x55u8; 8]).expect("patch");

        let mut out = [0u8; SECTOR_BYTES];
        let count = fs.read(id, 0, &mut out).expect("read");
        assert_eq!(count, SECTOR_BYTES);
        assert!(out[..100].iter().all(|byte| *byte == 0xAA));
        assert!(out[108..].iter().all(|byte| *byte == 0xAA));
        assert_eq!(&out[100..108], &[0x55u8; 8]);
    }

    #[test]
    fn a_write_beyond_the_slot_is_refused() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/big.bin").expect("create");

        let capacity = fs.geometry().capacity();
        assert_eq!(
            fs.write(id, capacity - 1, &[0u8; 2]),
            Err(FsError::Quota),
            "one byte past the slot must not reach the next slot's data"
        );
    }

    #[test]
    fn a_read_at_the_end_of_a_file_is_empty_not_an_error() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/short.txt").expect("create");
        fs.write(id, 0, b"abc").expect("write");

        let mut out = [0u8; 8];
        assert_eq!(fs.read(id, 3, &mut out), Ok(0));
        assert_eq!(fs.read(id, 99, &mut out), Ok(0));
        assert_eq!(fs.read(id, 1, &mut out), Ok(2));
    }

    #[test]
    fn a_read_only_mount_refuses_writes_and_creates() {
        let mut fs = mount(image_device(), true);
        assert!(fs.read_only());
        assert_eq!(fs.create(b"/new.txt"), Err(FsError::ReadOnly));
    }

    #[test]
    fn creating_twice_reports_the_existing_file() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/a.txt").expect("first create");
        assert_eq!(fs.create(b"/a.txt"), Err(FsError::AlreadyExists));
    }

    #[test]
    fn creating_past_the_slot_quota_is_refused() {
        let mut fs = mount(image_device(), false);
        for index in 0..MAX_SLOTS {
            let mut path = [0u8; 3];
            path[0] = b'/';
            path[1] = b'f';
            path[2] = b'0' + index as u8;
            fs.create(&path).expect("create fits the quota");
        }
        assert_eq!(fs.create(b"/one-too-many"), Err(FsError::Quota));
    }

    #[test]
    fn a_name_the_format_cannot_hold_is_refused() {
        let mut fs = mount(image_device(), false);
        let mut path = [b'a'; MAX_FILE_NAME + 3];
        path[0] = b'/';
        assert_eq!(
            fs.create(&path),
            Err(FsError::InvalidPath(PathError::ComponentTooLong))
        );
    }

    #[test]
    fn a_missing_file_is_not_found_and_a_file_is_not_a_directory() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/notes.txt").expect("create");

        assert_eq!(fs.lookup(b"/missing.txt"), Err(FsError::NotFound));
        assert_eq!(fs.lookup(b"/notes.txt/deeper"), Err(FsError::NotADirectory));
        assert_eq!(fs.lookup(b"/gone/deeper"), Err(FsError::NotFound));
        assert_eq!(fs.lookup(b"/"), Err(FsError::IsADirectory));
        assert_eq!(
            fs.lookup(b"relative.txt"),
            Err(FsError::InvalidPath(PathError::NotAbsolute))
        );
    }

    #[test]
    fn the_root_lists_every_file() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/one.txt").expect("create");
        fs.create(b"/two.txt").expect("create");

        let mut entries = [DiskListEntry {
            name: [0; NAME_FIELD_BYTES],
            name_len: 0,
            size: 0,
        }; 4];
        let count = fs.list(&mut entries);
        assert_eq!(count, 2);
        assert_eq!(entries[0].name_bytes(), b"one.txt");
        assert_eq!(entries[1].name_bytes(), b"two.txt");
    }

    #[test]
    fn the_listing_never_writes_past_its_buffer() {
        let mut fs = mount(image_device(), false);
        for index in 0..MAX_SLOTS {
            let mut path = [0u8; 3];
            path[0] = b'/';
            path[1] = b'f';
            path[2] = b'0' + index as u8;
            fs.create(&path).expect("create");
        }

        let mut entries = [DiskListEntry {
            name: [0; NAME_FIELD_BYTES],
            name_len: 0,
            size: 0,
        }; 2];
        assert_eq!(fs.list(&mut entries), 2);
    }

    #[test]
    fn a_delete_frees_the_slot_for_a_new_file() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/gone.txt").expect("create");
        fs.delete(b"/gone.txt").expect("delete");

        assert_eq!(fs.file_count(), 0);
        assert_eq!(fs.lookup(b"/gone.txt"), Err(FsError::NotFound));
        // The freed slot takes a new file, and the new file's bytes are not
        // the old file's: the name is what decides, and it is gone.
        let id = fs.create(b"/fresh.txt").expect("create on the freed slot");
        fs.write(id, 0, b"new").expect("write");
        let mut out = [0u8; 8];
        assert_eq!(fs.read_path(b"/fresh.txt", &mut out).expect("read"), 3);
        assert_eq!(&out[..3], b"new");
    }

    #[test]
    fn a_deleted_file_is_unwritable_through_its_old_handle() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/doomed.txt").expect("create");
        fs.delete(b"/doomed.txt").expect("delete");
        // The identity is checked against the directory, so a handle that
        // outlived its file cannot keep writing it.
        assert_eq!(fs.write(id, 0, b"zombie"), Err(FsError::BadHandle));
    }

    #[test]
    fn a_delete_of_a_missing_file_is_a_not_found() {
        let mut fs = mount(image_device(), false);
        assert_eq!(fs.delete(b"/absent.txt"), Err(FsError::NotFound));
        // Nested paths are not files this format can name.
        assert_eq!(fs.delete(b"/dir/absent.txt"), Err(FsError::NotFound));
    }

    #[test]
    fn a_read_only_mount_refuses_delete_and_rename() {
        let mut fs = mount(image_device(), true);
        assert_eq!(fs.delete(b"/whatever.txt"), Err(FsError::ReadOnly));
        assert_eq!(
            fs.rename(b"/whatever.txt", b"/elsewhere.txt"),
            Err(FsError::ReadOnly)
        );
    }

    #[test]
    fn a_rename_moves_the_name_and_keeps_the_bytes() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/before.txt").expect("create");
        fs.write(id, 0, b"payload").expect("write");

        fs.rename(b"/before.txt", b"/after.txt").expect("rename");
        assert_eq!(fs.lookup(b"/before.txt"), Err(FsError::NotFound));
        assert_eq!(fs.file_count(), 1);

        // The handle taken before the rename still reads the file: identity
        // is the slot, not the name.
        let mut out = [0u8; 8];
        assert_eq!(fs.read(id, 0, &mut out).expect("read"), 7);
        assert_eq!(&out[..7], b"payload");
        assert_eq!(fs.read_path(b"/after.txt", &mut out).expect("read"), 7);
    }

    #[test]
    fn a_rename_refuses_a_colliding_target_without_touching_anything() {
        let mut fs = mount(image_device(), false);
        let id = fs.create(b"/first.txt").expect("create first");
        fs.create(b"/second.txt").expect("create second");

        assert_eq!(
            fs.rename(b"/first.txt", b"/second.txt"),
            Err(FsError::AlreadyExists)
        );
        // Both files are exactly where they were.
        assert!(fs.lookup(b"/first.txt").is_ok());
        assert!(fs.lookup(b"/second.txt").is_ok());
        let stat = fs.stat(id).expect("stat");
        assert_eq!(stat.size, 0);
    }

    #[test]
    fn a_rename_onto_its_own_name_is_refused() {
        let mut fs = mount(image_device(), false);
        fs.create(b"/same.txt").expect("create");
        assert_eq!(
            fs.rename(b"/same.txt", b"/same.txt"),
            Err(FsError::AlreadyExists)
        );
    }

    #[test]
    fn a_deleted_slot_round_trips_through_the_medium() {
        // The delete reaches the device, not just the memory: a fresh mount
        // of the same image must agree that the file is gone.
        let mut device = image_device();
        {
            let mut fs = mount(device, false);
            let id = fs.create(b"/temp.txt").expect("create");
            fs.write(id, 0, b"data").expect("write");
            fs.delete(b"/temp.txt").expect("delete");
            fs.flush().expect("flush");
            device = fs.into_device();
        }
        let fs = mount(device, false);
        assert_eq!(fs.file_count(), 0);
        assert_eq!(fs.lookup(b"/temp.txt"), Err(FsError::NotFound));
    }
}
