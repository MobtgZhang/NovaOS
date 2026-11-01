//! The M5 suites: PCI enumeration, the VirtIO block driver, the block cache,
//! and read/write mounts into the file namespace.
//!
//! # Why these tests exist at all
//!
//! The logic behind the disk format — the superblock's fields, the directory
//! layout, the slot geometry — is host-tested in `nova-fs`'s `diskfs` module
//! against an in-memory array. What that cannot test is the thing M5 is
//! actually about: that a *real* emulated device, reached through real PCI
//! configuration space and a real virtqueue, returns the bytes the format
//! says it should, and that a device which misbehaves is caught rather than
//! trusted.
//!
//! So these cases drive the same `KernelBlockDevice` the filesystem mounts,
//! over the same `with_disk` path, against the disks the bootloader actually
//! attached. A passing case here means the round trip through the machine
//! worked, not just that the parsing was self-consistent.
//!
//! # The bar these tests must clear
//!
//! Every case names the failure it catches. The three failure classes worth
//! stating up front:
//!
//! - **A driver that believes a device.** The device is untrusted hardware.
//!   It can answer late, answer wrongly, or never answer. `block_timeout` and
//!   `driver_crash_mid_io` arm the fault hooks to make it do exactly those
//!   things, because a recovery path that has never run is a recovery path
//!   that does not work.
//! - **A cache that verifies its own writes.** `virtio_block_write_and_verify`
//!   asserts on the device counters, not on the bytes alone, so a
//!   write-then-read that was served entirely from the cache is caught.
//! - **A mount that escapes the capability system.** `read_only_mount` proves
//!   a disk reachable only through a path scope, and read-only for a caller
//!   whose grant says so.
//!
//! # The fault hooks are always armed through a guard
//!
//! A hook that is armed and not cleared poisons every later case. Each fault
//! case therefore arms through a `Drop` guard, so the hooks are cleared on
//! every path out of the case — including the early `?` returns, which are
//! exactly the paths where a forgotten cleanup would be written.

use core::sync::atomic::Ordering;

use nova_abi::cap::Rights;
use nova_fs::diskfs::{BlockDevice, DeviceError, MAGIC, SECTOR_BYTES, parse_superblock};
use nova_fs::ramfs::FsError;

use super::harness::TestSuite;
use crate::drivers::block::KernelBlockDevice;
use crate::drivers::{self, block, pci, virtio};
use crate::services::KernelState;

/// The M5 suites, in registration order.
///
/// The names are the milestone's exit gate (`reports/NovaOS.md` §20.7) and must
/// not be renamed: the shell harness asserts on exactly these strings.
pub static M5_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "pci_enumeration",
        cases: &[
            (
                "finds_a_virtio_block_device_with_a_modern_transport",
                finds_a_virtio_block_device_with_a_modern_transport,
            ),
            (
                "bar_probe_does_not_move_the_bar",
                bar_probe_does_not_move_the_bar,
            ),
            (
                "advertises_every_transport_structure",
                advertises_every_transport_structure,
            ),
        ],
    },
    TestSuite {
        name: "virtio_block_read",
        cases: &[
            (
                "sector_zero_holds_the_filesystem_magic",
                sector_zero_holds_the_filesystem_magic,
            ),
            (
                "the_second_read_is_served_from_the_cache",
                the_second_read_is_served_from_the_cache,
            ),
        ],
    },
    TestSuite {
        name: "virtio_block_write_and_verify",
        cases: &[
            (
                "a_written_sector_reads_back_from_the_medium",
                a_written_sector_reads_back_from_the_medium,
            ),
            (
                "a_file_written_through_the_vfs_reads_back",
                a_file_written_through_the_vfs_reads_back,
            ),
        ],
    },
    TestSuite {
        name: "block_timeout",
        cases: &[
            (
                "a_silent_device_times_out_and_recovers",
                a_silent_device_times_out_and_recovers,
            ),
            (
                "a_completion_the_driver_rejects_is_not_used",
                a_completion_the_driver_rejects_is_not_used,
            ),
        ],
    },
    TestSuite {
        name: "driver_crash_mid_io",
        cases: &[
            (
                "an_abandoned_request_is_cancelled_cleanly",
                an_abandoned_request_is_cancelled_cleanly,
            ),
            (
                "the_disk_comes_back_after_the_driver_is_reopened",
                the_disk_comes_back_after_the_driver_is_reopened,
            ),
        ],
    },
    TestSuite {
        name: "read_only_mount",
        cases: &[
            (
                "the_read_only_device_refuses_writes",
                the_read_only_device_refuses_writes,
            ),
            (
                "a_read_only_mount_refuses_a_vfs_write",
                a_read_only_mount_refuses_a_vfs_write,
            ),
            (
                "a_mount_is_reachable_only_through_a_scope",
                a_mount_is_reachable_only_through_a_scope,
            ),
        ],
    },
];

// ---------------------------------------------------------------------------
// pci_enumeration
// ---------------------------------------------------------------------------

/// The bus walk must find the block devices the boot configuration attached.
///
/// The walk follows PCI-to-PCI bridges and is bounded; a device that only
/// exists behind an unfollowed bridge is invisible, which is the failure this
/// catches. The transport is confirmed by the *capability list* rather than the
/// device ID, because a transitional device keeps the legacy ID while offering
/// the modern structures — the ID alone cannot tell the two apart.
fn finds_a_virtio_block_device_with_a_modern_transport() -> Result<(), &'static str> {
    let devices = pci::enumerate();
    if devices.is_empty() {
        return Err("the bus walk found no functions at all");
    }

    let mut blocks = 0usize;
    let mut modern = 0usize;
    for device in devices.iter() {
        if !device.is_virtio_block() {
            continue;
        }
        blocks += 1;
        if let Some(capabilities) = pci::virtio_capabilities(device)
            && capabilities.find(pci::VIRTIO_CAP_COMMON).is_some()
        {
            modern += 1;
        }
    }

    if blocks == 0 {
        return Err("no virtio block function was enumerated");
    }
    if modern == 0 {
        return Err("every block device speaks only the legacy transport");
    }
    Ok(())
}

/// Probing a BAR must leave the device's address exactly as it found it.
///
/// The probe writes all-ones into the register and reads the size back; the
/// *base* has to come from the value that was there before the write, and the
/// original has to be restored afterwards. If either half is wrong the second
/// probe returns a different address, and — far worse — the device is left
/// decoded at the probe pattern, which is a BAR at `0xFFFF_FFF0` and a machine
/// whose disk has vanished. The double probe is what catches it.
fn bar_probe_does_not_move_the_bar() -> Result<(), &'static str> {
    let devices = pci::enumerate();
    let device = devices
        .find(|candidate| candidate.is_virtio_block())
        .ok_or("no virtio block device was enumerated")?;
    let capabilities = pci::virtio_capabilities(device).ok_or("no capability list")?;
    let common = capabilities
        .find(pci::VIRTIO_CAP_COMMON)
        .ok_or("no common structure is advertised")?;

    let first =
        pci::probe_bar(device.location, common.bar).ok_or("the common BAR could not be probed")?;
    let second = pci::probe_bar(device.location, common.bar).ok_or("the second probe failed")?;

    if first.io {
        return Err("the common structure is on an IO BAR, not memory");
    }
    if first.address != second.address || first.size != second.size {
        return Err("probing a BAR moved it");
    }
    // The structure the driver will map has to fit inside the region the probe
    // reported, or the mapping reaches past the BAR into whatever follows it.
    if common.offset as u64 + 0x38 > first.size {
        return Err("the common structure does not fit inside its BAR");
    }
    Ok(())
}

/// The four control structures a modern device must advertise.
///
/// A device missing any of them cannot be driven by this transport: the common
/// structure is the whole control interface, notify is how a request is
/// announced, the ISR is mapped (though unused by a polled driver) because its
/// absence signals a device that predates the layout, and the device
/// configuration is where the capacity lives.
fn advertises_every_transport_structure() -> Result<(), &'static str> {
    let devices = pci::enumerate();
    let device = devices
        .find(|candidate| candidate.is_virtio_block())
        .ok_or("no virtio block device was enumerated")?;
    let capabilities = pci::virtio_capabilities(device).ok_or("no capability list")?;

    if capabilities.find(pci::VIRTIO_CAP_COMMON).is_none() {
        return Err("the common structure is not advertised");
    }
    let notify = capabilities
        .find(pci::VIRTIO_CAP_NOTIFY)
        .ok_or("the notify structure is not advertised")?;
    if notify.notify_off_multiplier % 4 != 0 {
        return Err("the notify multiplier is not dword aligned");
    }
    if capabilities.find(pci::VIRTIO_CAP_ISR).is_none() {
        return Err("the ISR structure is not advertised");
    }
    let config = capabilities
        .find(pci::VIRTIO_CAP_DEVICE)
        .ok_or("the device configuration is not advertised")?;
    if config.length < 8 {
        return Err("the device configuration is too small to hold a capacity");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// virtio_block_read
// ---------------------------------------------------------------------------

/// Sector zero of the writable disk must carry the disk filesystem's magic.
///
/// This is the whole read path in one assertion: the driver submitted a
/// request, the device DMA'd the sector into the transfer frame, the frame was
/// copied out, and the bytes that arrived describe a filesystem the *format*
/// (host-tested) would accept. A driver that mixed up the descriptor flags, the
/// bounce offset, or the used-ring length shows up here as a wrong magic.
fn sector_zero_holds_the_filesystem_magic() -> Result<(), &'static str> {
    if block::disk_count() == 0 {
        return Err("no disk was started at boot");
    }

    let mut sector = [0u8; SECTOR_BYTES];
    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;

    if sector[..8] != MAGIC {
        return Err("sector zero does not carry the disk filesystem's magic");
    }

    let superblock = parse_superblock(&sector).map_err(|error| error.name())?;
    if superblock.total_sectors == 0 {
        return Err("the superblock reports a zero-length disk");
    }
    if device.sector_count() < superblock.total_sectors {
        return Err("the device is smaller than its own superblock claims");
    }
    Ok(())
}

/// Reading the same sector twice must be served from the cache the second time.
///
/// The cache is eight direct-mapped lines; if it were broken — never filling,
/// tagging wrong, or evicting on every access — every read would reach the
/// device and the counters would say so. The bytes alone cannot catch that: a
/// cache that returns the right bytes from the medium every time is correct in
/// effect and wrong in cost, and the counters are what distinguish the two.
fn the_second_read_is_served_from_the_cache() -> Result<(), &'static str> {
    let mut first = [0u8; SECTOR_BYTES];
    let mut second = [0u8; SECTOR_BYTES];

    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut first)
        .map_err(DeviceError::name)?;
    let before = block::with_disk(0, |disk| disk.stats()).map_err(|error| error.name())?;

    device
        .read_sector(0, &mut second)
        .map_err(DeviceError::name)?;
    let after = block::with_disk(0, |disk| disk.stats()).map_err(|error| error.name())?;

    if first != second {
        return Err("the same sector read back two different ways");
    }
    if after.cache_hits <= before.cache_hits {
        return Err("the second read was not served from the cache");
    }
    if after.device_reads != before.device_reads {
        return Err("a cached read still reached the device");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// virtio_block_write_and_verify
// ---------------------------------------------------------------------------

/// A written sector must read back from the medium, not from the cache.
///
/// The write invalidates the cache line rather than updating it, so the
/// verification read is forced to reach the device. The assertions are on the
/// device counters, not just the bytes: a write-then-read that was served
/// entirely from a cache holding the new bytes would *look* like a successful
/// round trip while the medium never changed. The bytes are checked too, of
/// course — a driver that wrote to the wrong offset would pass the counter test.
///
/// The sector chosen is inside the image's scratch area, whose bounds come from
/// the superblock rather than a guess. That is the one region this test may
/// scribble on without corrupting the mounted filesystem's directory or data.
fn a_written_sector_reads_back_from_the_medium() -> Result<(), &'static str> {
    if block::disk_count() == 0 {
        return Err("no disk was started at boot");
    }

    let mut sector = [0u8; SECTOR_BYTES];
    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;
    let superblock = parse_superblock(&sector).map_err(|error| error.name())?;
    let (start, end) = superblock
        .scratch_range()
        .ok_or("the image has no scratch area to test in")?;
    if end.checked_sub(start).unwrap_or(0) < 4 {
        return Err("the scratch area is too small to test in");
    }
    let lba = start + 1;

    let mut pattern = [0u8; SECTOR_BYTES];
    for (index, byte) in pattern.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(0x5A);
    }

    let before = block::with_disk(0, |disk| disk.stats()).map_err(|error| error.name())?;
    device
        .write_sector(lba, &pattern)
        .map_err(DeviceError::name)?;

    let mut readback = [0u8; SECTOR_BYTES];
    device
        .read_sector(lba, &mut readback)
        .map_err(DeviceError::name)?;
    let after = block::with_disk(0, |disk| disk.stats()).map_err(|error| error.name())?;

    if readback != pattern {
        return Err("the sector read back is not the sector that was written");
    }
    if after.device_writes <= before.device_writes {
        return Err("the write never reached the device");
    }
    if after.device_reads <= before.device_reads {
        return Err("the verification read never reached the device");
    }
    Ok(())
}

/// A file written through the VFS must read back through the VFS.
///
/// This is the exit gate in `reports/NovaOS.md` §20.7 — "read and write a test
/// file in a dedicated QEMU virtual disk" — taken through the *file server*
/// rather than the driver, so the mount routing, the scope check, the position
/// tracking, and the flush-on-close are all on the path. Close is the durability
/// point: the write is write-through to the device, and close asks the device to
/// make it durable, so reopening reads what the medium holds.
fn a_file_written_through_the_vfs_reads_back() -> Result<(), &'static str> {
    let state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = state.slot_of_task(task).ok_or("the new task has no slot")?;

    // `reset` unmounts everything, so the disk has to be mounted again before
    // a path on it means anything.
    state
        .files
        .mount_disk(0, drivers::MOUNT_TEST, false)
        .map_err(|error| error.name())?;
    state
        .grant_file_scope(slot, drivers::MOUNT_TEST, Rights::READ | Rights::WRITE)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    // Sixteen bytes, deliberately the whole of the read buffer below: the file
    // is pre-existing and larger, so reading the buffer's length reads exactly
    // the pattern written and no tail of the original contents.
    let file = b"/media/test/hello.txt";
    let pattern = b"vfs round trip!!";

    let (wire, _size) = state
        .files
        .open_scoped(&scopes, file, Rights::WRITE)
        .map_err(|error| error.name())?;
    let written = state
        .files
        .write(&scopes, wire, pattern)
        .map_err(|error| error.name())?;
    if written != pattern.len() {
        return Err("the write reported the wrong byte count");
    }
    state
        .files
        .close(&scopes, wire)
        .map_err(|error| error.name())?;

    // A fresh handle reads from offset zero again; the previous handle's
    // position advanced past the write.
    let (wire, _size) = state
        .files
        .open_scoped(&scopes, file, Rights::READ)
        .map_err(|error| error.name())?;
    let mut buffer = [0u8; 16];
    let read = state
        .files
        .read(&scopes, wire, &mut buffer)
        .map_err(|error| error.name())?;
    state
        .files
        .close(&scopes, wire)
        .map_err(|error| error.name())?;

    if read != pattern.len() {
        return Err("the read returned the wrong byte count");
    }
    if buffer[..read] != pattern[..] {
        return Err("the bytes read back are not the bytes written through the VFS");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// block_timeout
// ---------------------------------------------------------------------------

/// A device that never answers must time out, and the disk must recover.
///
/// The hook suppresses completions, so the used ring never advances and the
/// driver's poll budget runs out. The failure path resets the device, rewinds
/// the rings, and reinitializes from scratch — the same sequence a fresh `open`
/// runs — so the *next* request is served by a device that is fully back. A
/// driver that left the device half-initialized after a timeout would hang the
/// recovery read, which is why the recovery read is part of the case rather
/// than a separate one.
fn a_silent_device_times_out_and_recovers() -> Result<(), &'static str> {
    let outcome = {
        // The guard clears every hook on the way out, including the early
        // return below — a hook left armed would silently break every later
        // case in the same boot.
        let _guard = arm_suppress_completions();
        let mut sector = [0u8; SECTOR_BYTES];
        let mut device = KernelBlockDevice::new(0);
        device.read_sector(0, &mut sector)
    };

    match outcome {
        Err(DeviceError::Timeout) => {}
        Err(_) => return Err("the silent device failed, but not as a timeout"),
        Ok(()) => return Err("a device that never answered was reported as success"),
    }

    let mut sector = [0u8; SECTOR_BYTES];
    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;
    if sector[..8] != MAGIC {
        return Err("the recovered device returned the wrong bytes");
    }
    Ok(())
}

/// A completion naming a descriptor the queue does not have must be rejected.
///
/// The device is untrusted, and the used ring's `id` field is a number it
/// chooses. A driver that used it as an index without checking would compute a
/// descriptor address past the end of the ring — the mechanism by which a
/// malicious or corrupt device turns a read into a kernel memory write. The
/// hook forges the smallest definitely-wrong index (one past the end), so a
/// bounds check that only caught a wildly wrong value would still fail here.
fn a_completion_the_driver_rejects_is_not_used() -> Result<(), &'static str> {
    let outcome = {
        let _guard = arm_corrupt_completion();
        let mut sector = [0u8; SECTOR_BYTES];
        let mut device = KernelBlockDevice::new(0);
        device.read_sector(0, &mut sector)
    };

    match outcome {
        Err(DeviceError::Fault) => {}
        Err(_) => return Err("the forged completion failed, but not as a rejected answer"),
        Ok(()) => return Err("a completion naming a nonexistent descriptor was accepted"),
    }

    // The rejection reset the device; the next request must still be served.
    let mut sector = [0u8; SECTOR_BYTES];
    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;
    if sector[..8] != MAGIC {
        return Err("the recovered device returned the wrong bytes");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// driver_crash_mid_io
// ---------------------------------------------------------------------------

/// A request whose owner vanishes must be cancelled cleanly.
///
/// The request is already submitted, so "cancel" cannot mean "do not send it" —
/// it means stop waiting, stop the device, and give the buffers back without
/// leaving the driver holding something the device is still writing into. The
/// two assertions are that the request reports as cancelled and that nothing is
/// left in flight; the second is the one that matters, because a cancelled
/// request still in flight is a buffer handed back to a caller a device can
/// still DMA into.
fn an_abandoned_request_is_cancelled_cleanly() -> Result<(), &'static str> {
    let outcome = {
        let _guard = arm_owner_exited();
        let mut sector = [0u8; SECTOR_BYTES];
        let mut device = KernelBlockDevice::new(0);
        device.read_sector(0, &mut sector)
    };

    match outcome {
        Err(DeviceError::Unavailable) => {}
        Err(_) => return Err("the abandoned request failed, but not as a cancellation"),
        Ok(()) => return Err("an abandoned request was reported as success"),
    }

    let in_flight =
        block::with_disk(0, |disk| disk.request_in_flight()).map_err(|error| error.name())?;
    if in_flight {
        return Err("a cancelled request was left in flight");
    }
    Ok(())
}

/// After the driver is torn down and reopened, the disk must be fully back.
///
/// The cancellation path stops the device and rewinds the rings but does not
/// restart them — the owner is gone and there is nothing to resume. So the disk
/// is left deliberately inert. This case proves the way out of that state:
/// shutting every disk down and re-running the bring-up produces a working disk
/// again, with no leaked frames (a leak would show up as a later allocation
/// failure rather than here, but the frame balance is what makes the reopen
/// possible at all).
fn the_disk_comes_back_after_the_driver_is_reopened() -> Result<(), &'static str> {
    let before = block::disk_count();
    if before == 0 {
        return Err("no disk was started at boot");
    }

    block::shutdown_all();
    if block::disk_count() != 0 {
        return Err("shutdown did not empty the disk table");
    }

    let started = block::init();
    if started != before {
        return Err("reopening started a different number of disks");
    }

    let mut sector = [0u8; SECTOR_BYTES];
    let mut device = KernelBlockDevice::new(0);
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;
    if sector[..8] != MAGIC {
        return Err("the reopened disk returned the wrong bytes");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// read_only_mount
// ---------------------------------------------------------------------------

/// A read-only device must refuse a write before it reaches the medium.
///
/// The reference machine attaches the second disk with `readonly=on`, which
/// makes QEMU set `VIRTIO_BLK_F_RO`. The driver reports that as a refusal rather
/// than submitting a request the device would reject — and the check is that
/// sector zero is *unchanged* afterwards, because a refusal that still let the
/// bytes through would be indistinguishable, from the medium's point of view,
/// from no refusal at all.
fn the_read_only_device_refuses_writes() -> Result<(), &'static str> {
    if block::disk_count() < 2 {
        return Err("the read-only disk was not started");
    }

    let read_only =
        block::with_disk(1, |disk| disk.is_read_only()).map_err(|error| error.name())?;
    if !read_only {
        return Err("disk 1 does not report itself read-only at the device level");
    }

    let mut pattern = [0u8; SECTOR_BYTES];
    pattern[0] = 0xA5;
    let mut device = KernelBlockDevice::new(1);
    match device.write_sector(0, &pattern) {
        Err(DeviceError::ReadOnly) => {}
        Err(_) => return Err("the write failed, but not as a read-only refusal"),
        Ok(()) => return Err("a read-only device accepted a write"),
    }

    let mut sector = [0u8; SECTOR_BYTES];
    device
        .read_sector(0, &mut sector)
        .map_err(DeviceError::name)?;
    if sector[..8] != MAGIC {
        return Err("the refused write reached the medium");
    }
    Ok(())
}

/// A read-only *mount* must refuse a write even for a caller with the right.
///
/// The device flag and the mount flag are two separate mechanisms and both are
/// needed. The device flag stops the bytes; the mount flag is what tells the
/// *caller* why, as an authority refusal rather than a device fault. A caller
/// whose grant says WRITE, writing to a path the administrator mounted read-only,
/// gets `E_READ_ONLY` — not a silent success and not a mysterious I/O error.
/// Reading is, of course, still allowed.
fn a_read_only_mount_refuses_a_vfs_write() -> Result<(), &'static str> {
    let state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = state.slot_of_task(task).ok_or("the new task has no slot")?;

    state
        .files
        .mount_disk(1, drivers::MOUNT_RO, true)
        .map_err(|error| error.name())?;
    let read_only = state
        .files
        .mounts()
        .next()
        .map(|mount| mount.read_only())
        .unwrap_or(false);
    if !read_only {
        return Err("the mount was not recorded as read-only");
    }

    state
        .grant_file_scope(slot, drivers::MOUNT_RO, Rights::READ | Rights::WRITE)
        .map_err(|_| "the grant was refused")?;
    let scopes = state.scope_set_for(slot);

    let file = b"/media/ro/hello.txt";
    let (wire, _size) = state
        .files
        .open_scoped(&scopes, file, Rights::WRITE)
        .map_err(|error| error.name())?;
    match state.files.write(&scopes, wire, b"should be refused") {
        Err(FsError::ReadOnly) => {}
        Err(_) => return Err("the write failed, but not as a read-only refusal"),
        Ok(_) => return Err("a read-only mount accepted a write"),
    }
    state
        .files
        .close(&scopes, wire)
        .map_err(|error| error.name())?;

    // The same file must still be readable: read-only is not no-access.
    let (wire, _size) = state
        .files
        .open_scoped(&scopes, file, Rights::READ)
        .map_err(|error| error.name())?;
    let mut buffer = [0u8; 16];
    let read = state
        .files
        .read(&scopes, wire, &mut buffer)
        .map_err(|error| error.name())?;
    state
        .files
        .close(&scopes, wire)
        .map_err(|error| error.name())?;
    if read == 0 {
        return Err("a read-only mount refused to be read");
    }
    Ok(())
}

/// A mount must be unreachable without a path scope.
///
/// Mount routing happens *before* the filesystem resolves the path, so it would
/// be easy — and wrong — for it to hand out a file to any caller who guessed the
/// name. This case mounts a disk, grants the task no file scope at all, and
/// asserts the open is refused as a denial. It proves the mount is subject to
/// the same capability discipline as a RAM file, and that a caller without a
/// grant learns nothing about which disks are mounted.
fn a_mount_is_reachable_only_through_a_scope() -> Result<(), &'static str> {
    let state = fresh_state();
    let task = state.create_task().ok_or("task pool is empty")?;
    let slot = state.slot_of_task(task).ok_or("the new task has no slot")?;

    state
        .files
        .mount_disk(1, drivers::MOUNT_RO, true)
        .map_err(|error| error.name())?;

    let scopes = state.scope_set_for(slot);
    match state
        .files
        .open_scoped(&scopes, b"/media/ro/hello.txt", Rights::READ)
    {
        Err(FsError::Denied) => {}
        Err(_) => return Err("the unscoped open failed, but not as a denial"),
        Ok(_) => return Err("a mount was reachable without a scope"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A guard that clears every fault hook when it goes out of scope.
///
/// Armed through the constructors below rather than by setting a hook directly,
/// so the cleanup cannot be forgotten: the hooks are global, and one left armed
/// would make every later case in the same boot fail for the wrong reason.
struct ArmedFault;

impl Drop for ArmedFault {
    fn drop(&mut self) {
        virtio::faults().clear();
    }
}

/// Make the device stop publishing completions, so a request times out.
fn arm_suppress_completions() -> ArmedFault {
    virtio::faults()
        .suppress_completions
        .store(true, Ordering::Release);
    ArmedFault
}

/// Make the device report a completion naming a descriptor that does not exist.
fn arm_corrupt_completion() -> ArmedFault {
    virtio::faults()
        .corrupt_completion
        .store(true, Ordering::Release);
    ArmedFault
}

/// Make the owner of an in-flight request vanish.
fn arm_owner_exited() -> ArmedFault {
    virtio::faults().owner_exited.store(true, Ordering::Release);
    ArmedFault
}

/// The kernel's own service state, reset between cases.
///
/// Same reasoning as `m34::fresh_state`: the state is about 71 KiB (the file
/// server alone) against a 64 KiB stack, so it lives in a static and is reached
/// through the `UnsafeCell` the kernel uses. `reset` unmounts every disk, which
/// is why the mount cases re-mount before reaching for a path.
fn fresh_state() -> &'static mut KernelState {
    // SAFETY: the tests run one at a time, in ring 0, with interrupts masked —
    // the same single-CPU assumption the kernel's own service state relies on.
    unsafe {
        let state = crate::services::test_state();
        state.reset();
        state
    }
}
