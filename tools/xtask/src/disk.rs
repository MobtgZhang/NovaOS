//! Generating the two test disks the M5 suites read and write.
//!
//! # Why the disks are generated, not committed
//!
//! A committed binary image would go stale the moment the on-disk format
//! changed, and the failure it produces is the worst kind to debug: the host
//! tests, which generate and mount the same format, stay green while the kernel
//! refuses to mount an image that no longer matches. The disagreement lives in
//! whichever copy drifted, and nothing points at it.
//!
//! So the disks are built here, from the *same* `nova_fs::diskfs::DiskFs` the
//! kernel mounts. The formatter and the mounter are one piece of code; a change
//! to the format moves both at once, and a boot that cannot mount is a bug in
//! the change rather than a stale artifact.
//!
//! # The two disks are byte-identical on purpose
//!
//! `testdisk.raw` is the writable disk. `testdisk-ro.raw` is the read-only one
//! — but its bytes are the *same image*. The read-only property comes from
//! QEMU, not from the file: attaching the drive with `readonly=on` makes the
//! emulated device set `VIRTIO_BLK_F_RO`, and that is the flag the M5 suites
//! assert on. Keeping the images identical means the only difference between
//! the two disks under test is the device configuration, which is exactly the
//! thing being tested.
//!
//! # Why the superblock is written by hand
//!
//! `DiskFs` has no `format` entry point, and adding one would put the format's
//! definition in two places — here and in `parse_superblock` — rather than one.
//! What `DiskFs` contributes is everything *after* the superblock: the
//! directory, the slot allocation, and the file contents, all through the code
//! the kernel also runs. The superblock is the format's one fixed preamble, and
//! it is written out below against the parser's own field offsets.

use std::path::PathBuf;

use nova_fs::diskfs::{
    BlockDevice, DeviceError, DiskFs, FORMAT_VERSION, MAGIC, MAX_SLOTS, SECTOR_BYTES,
};

/// Sectors in one test disk: 1 MiB.
const DEVICE_SECTORS: usize = 2048;

/// Sectors one file slot occupies, so each file gets 4 KiB.
const SLOT_SECTORS: u32 = 8;

/// The single directory sector's index.
const DIR_START: u32 = 1;

/// The directory's length in sectors.
const DIR_SECTORS: u32 = 1;

/// Data begins immediately after the directory.
const DATA_START: u32 = 2;

/// The scratch area begins after the last slot.
///
/// `DATA_START + MAX_SLOTS * SLOT_SECTORS` = 2 + 64 = 66. This is the region
/// the M5 write test scribbles on, because it is the one part of the image the
/// filesystem never uses — so a test that writes here cannot corrupt a mounted
/// filesystem, and a test that reads it back is reading the *medium*.
const SCRATCH_START: u32 = DATA_START + (MAX_SLOTS as u32) * SLOT_SECTORS;

/// The file every disk carries, so a test always has something to open.
const SEED_FILE: &[u8] = b"/hello.txt";

/// The seed file's contents.
const SEED_CONTENTS: &[u8] = b"hello from the NovaOS test disk\n";

/// The writable test disk's file name.
pub const WRITABLE_NAME: &str = "testdisk.raw";

/// The read-only test disk's file name.
///
/// Byte-identical to [`WRITABLE_NAME`]'s image; the read-only property is
/// QEMU's to apply (`readonly=on`), not something in the bytes.
pub const READ_ONLY_NAME: &str = "testdisk-ro.raw";

/// The two images, in the order they are attached to the machine.
const IMAGES: [&str; 2] = [WRITABLE_NAME, READ_ONLY_NAME];

/// Where the writable test disk is written.
pub fn writable_path() -> PathBuf {
    crate::boot::disk_path(WRITABLE_NAME)
}

/// Where the read-only test disk is written.
pub fn read_only_path() -> PathBuf {
    crate::boot::disk_path(READ_ONLY_NAME)
}

/// A sector device backed by a mutable borrow of a plain byte vector.
///
/// The *borrow* rather than ownership is what lets the caller keep the image.
/// `DiskFs::mount` takes the device by value, so an owning device would carry
/// the bytes away and the only way back would be to re-read them sector by
/// sector through the trait. Borrowing means the vector is simply still there
/// once the filesystem is dropped.
struct VecDevice<'a> {
    sectors: &'a mut Vec<u8>,
}

impl<'a> VecDevice<'a> {
    /// A device over `sectors`, whose length must be a whole number of sectors.
    fn new(sectors: &'a mut Vec<u8>) -> Self {
        Self { sectors }
    }

    /// The byte range one sector occupies, or `None` if it is off the end.
    fn range(&self, lba: u64) -> Option<(usize, usize)> {
        let start = lba.checked_mul(SECTOR_BYTES as u64)? as usize;
        let end = start.checked_add(SECTOR_BYTES)?;
        (end <= self.sectors.len()).then_some((start, end))
    }
}

impl BlockDevice for VecDevice<'_> {
    fn sector_count(&self) -> u64 {
        (self.sectors.len() / SECTOR_BYTES) as u64
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR_BYTES]) -> Result<(), DeviceError> {
        let (start, end) = self.range(lba).ok_or(DeviceError::OutOfRange)?;
        out.copy_from_slice(&self.sectors[start..end]);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR_BYTES]) -> Result<(), DeviceError> {
        let (start, end) = self.range(lba).ok_or(DeviceError::OutOfRange)?;
        self.sectors[start..end].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        // The vector *is* the medium; there is nothing to flush to.
        Ok(())
    }
}

/// Format `image` in place: superblock, then a seeded file through `DiskFs`.
///
/// The `Vec` is threaded through to `VecDevice` by borrow, so the bytes the
/// filesystem wrote are the bytes the caller writes to disk — no copy and no
/// re-read.
fn format_image(image: &mut Vec<u8>) -> Result<(), String> {
    let mut superblock = [0u8; SECTOR_BYTES];
    // Field offsets mirror `parse_superblock`; see the module docs on why this
    // is written by hand rather than through a `format` entry point.
    superblock[0..8].copy_from_slice(&MAGIC);
    superblock[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    superblock[12..16].copy_from_slice(&(MAX_SLOTS as u32).to_le_bytes());
    superblock[16..20].copy_from_slice(&SLOT_SECTORS.to_le_bytes());
    superblock[20..24].copy_from_slice(&DIR_START.to_le_bytes());
    superblock[24..28].copy_from_slice(&DIR_SECTORS.to_le_bytes());
    superblock[28..32].copy_from_slice(&DATA_START.to_le_bytes());
    superblock[32..36].copy_from_slice(&SCRATCH_START.to_le_bytes());
    superblock[36..40].copy_from_slice(&(DEVICE_SECTORS as u32).to_le_bytes());

    let mut device = VecDevice::new(image);
    device
        .write_sector(0, &superblock)
        .map_err(|error| error.name().to_string())?;

    let mut fs = DiskFs::mount(device, false).map_err(|error| error.name().to_string())?;
    let file = fs
        .create(SEED_FILE)
        .map_err(|error| error.name().to_string())?;
    fs.write(file, 0, SEED_CONTENTS)
        .map_err(|error| error.name().to_string())?;
    fs.flush().map_err(|error| error.name().to_string())?;
    Ok(())
}

/// Write both test disks, returning their paths in [`IMAGES`] order.
pub fn generate() -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::with_capacity(IMAGES.len());

    for name in IMAGES {
        let path = crate::boot::disk_path(name);
        let mut image = vec![0u8; DEVICE_SECTORS * SECTOR_BYTES];
        format_image(&mut image)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, &image)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;

        written.push(path);
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch geometry the kernel's write test relies on.
    ///
    /// `test/m5.rs` writes at `scratch_start + 1` and reads it back. If the two
    /// sides ever disagree about where scratch begins — a format change that
    /// moved the data region without moving this constant — the test would
    /// scribble on a live file's slot instead. Pinning the numbers here makes
    /// that a host-side failure rather than a mysterious boot failure.
    #[test]
    fn the_scratch_area_matches_the_kernel_expectation() {
        assert_eq!(SCRATCH_START, 66);
        assert_eq!(DATA_START + (MAX_SLOTS as u32) * SLOT_SECTORS, 66);
        assert!(SCRATCH_START + 4 <= DEVICE_SECTORS as u32);
    }

    /// A generated image must mount and yield its seeded file.
    ///
    /// This is the round trip the whole design rests on: the same `DiskFs`
    /// formats here and mounts in the kernel, so if the host can mount what it
    /// just wrote, the kernel can too — and if it cannot, the failure is caught
    /// in milliseconds here rather than as a boot timeout in QEMU.
    #[test]
    fn a_generated_image_mounts_and_reads_its_seed_file() {
        let mut image = vec![0u8; DEVICE_SECTORS * SECTOR_BYTES];
        format_image(&mut image).expect("the image formats");

        let device = VecDevice::new(&mut image);
        let mut fs = DiskFs::mount(device, false).expect("the fresh image mounts");

        let file = fs.lookup(SEED_FILE).expect("the seed file exists");
        let mut buffer = [0u8; 64];
        let read = fs.read(file, 0, &mut buffer).expect("the seed file reads");
        assert_eq!(&buffer[..read], SEED_CONTENTS);
    }

    /// Both disks must be byte-identical: only QEMU makes one read-only.
    ///
    /// If they ever diverge, the read-only suite would be testing a different
    /// *image* as well as a different device, and a failure could be blamed on
    /// the wrong half.
    #[test]
    fn both_images_are_identical() {
        let mut first = vec![0u8; DEVICE_SECTORS * SECTOR_BYTES];
        let mut second = vec![0u8; DEVICE_SECTORS * SECTOR_BYTES];
        format_image(&mut first).expect("the first image formats");
        format_image(&mut second).expect("the second image formats");
        assert_eq!(first, second);
    }
}
