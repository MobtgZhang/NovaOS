//! The kernel's block layer: the disks that were found, a bounded sector
//! cache, and the counters the M5 suites assert on.
//!
//! # What this layer is for
//!
//! Above it, `nova-fs`'s disk filesystem speaks a sector device trait and knows
//! nothing about VirtIO. Below it, the driver knows nothing about files. This
//! module is the join: it owns the disks (there are two, and they are found at
//! boot rather than assumed), keeps a small cache in front of them, and counts
//! what really reached the device.
//!
//! # The cache, and why a write invalidates rather than updates
//!
//! Eight sectors, direct-mapped by `lba % 8`. A hit is a copy from the cache; a
//! miss is a device read. A **write goes to the device and invalidates** the
//! line rather than storing the new bytes, and the reason is what the write
//! tests are for: a write-then-read must be a *device* read, or the suite is
//! verifying that the cache can read back what the cache wrote. Invalidating
//! makes every verification pass through the medium, which is the only thing
//! that distinguishes "the write happened" from "the write was remembered".
//!
//! The cost is one extra device read per sector written-and-read-back, and the
//! benefit is that `device_reads` / `device_writes` in the counters are the
//! truth about what the hardware was asked to do.

use crate::sync::{SpinLock, SpinLockGuard};

use nova_fs::diskfs::{BlockDevice, DeviceError, SECTOR_BYTES};

use crate::drivers::virtio::blk::{BlockError, VirtioBlk};
use crate::drivers::{pci, virtio};
use crate::klog;
use crate::logger::Level;

/// Disks the kernel keeps. Two is what the reference machine has: one writable
/// test disk and one read-only.
pub const MAX_DISKS: usize = 2;

/// Sector-cache lines per disk.
///
/// Eight is not a performance choice — it is a *bounded* choice: 4 KiB of
/// statics per disk, small enough to be obviously harmless in the kernel image
/// and large enough that the filesystem's metadata reads (superblock, then
/// directory, then file) are not three device round trips.
pub const CACHE_LINES: usize = 8;

/// One cached sector.
#[derive(Clone, Copy)]
struct CacheLine {
    /// The sector this line holds, when `valid`.
    tag: u64,
    /// Whether the line holds anything.
    valid: bool,
    /// The sector's bytes.
    data: [u8; SECTOR_BYTES],
}

impl CacheLine {
    /// An empty line.
    const fn empty() -> Self {
        Self {
            tag: 0,
            valid: false,
            data: [0; SECTOR_BYTES],
        }
    }
}

/// What one disk has been asked to do.
#[derive(Clone, Copy, Default)]
pub struct DiskStats {
    /// Reads served from the cache — no device traffic.
    pub cache_hits: u64,
    /// Reads that had to reach the device.
    pub cache_misses: u64,
    /// Sector reads the device performed.
    pub device_reads: u64,
    /// Sector writes the device performed.
    pub device_writes: u64,
    /// Flushes the device performed.
    pub device_flushes: u64,
    /// Requests that failed for any reason.
    pub errors: u64,
}

/// One disk: the device, its cache, and its counters.
pub struct Disk {
    device: VirtioBlk,
    cache: [CacheLine; CACHE_LINES],
    stats: DiskStats,
}

impl Disk {
    /// The device's capacity in sectors.
    pub fn capacity_sectors(&self) -> u64 {
        self.device.capacity_sectors()
    }

    /// Whether the device refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.device.is_read_only()
    }

    /// Whether the device implements flush.
    pub fn supports_flush(&self) -> bool {
        self.device.supports_flush()
    }

    /// The counters as they stand.
    pub fn stats(&self) -> DiskStats {
        self.stats
    }

    /// Whether a request is still waiting on the device.
    ///
    /// This is the state the cancellation path has to leave *empty*: a request
    /// that is reported as cancelled but is still in flight would mean the
    /// driver handed a buffer back to the caller while the device could still
    /// DMA into it. The M5 suites assert on it, which is why it is public.
    pub fn request_in_flight(&self) -> bool {
        self.device.request_in_flight()
    }

    /// Read one sector, through the cache.
    ///
    /// The cache is bypassed — and the line it would have served from is
    /// invalidated — while a fault hook is armed. See [`faults_armed`]: a hook
    /// makes the *device* fail, and a cache that answered anyway would report
    /// that failure as a success. Invalidating rather than merely skipping
    /// matters too: the failed read writes into this line's storage on the way
    /// out, so leaving it valid would hand the next reader bytes a device
    /// never produced.
    fn read(&mut self, lba: u64, out: &mut [u8]) -> Result<(), BlockError> {
        let line = (lba as usize) % CACHE_LINES;
        if !super::virtio::faults_armed() && self.cache[line].valid && self.cache[line].tag == lba {
            self.stats.cache_hits += 1;
            out.copy_from_slice(&self.cache[line].data);
            return Ok(());
        }
        if super::virtio::faults_armed() && self.cache[line].tag == lba {
            self.cache[line].valid = false;
        }

        self.stats.cache_misses += 1;
        match self.device.read_sector(lba, &mut self.cache[line].data) {
            Ok(()) => {
                self.stats.device_reads += 1;
                self.cache[line].tag = lba;
                self.cache[line].valid = true;
                out.copy_from_slice(&self.cache[line].data);
                Ok(())
            }
            Err(error) => {
                self.stats.errors += 1;
                Err(error)
            }
        }
    }

    /// Write one sector, straight through to the device.
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        match self.device.write_sector(lba, data) {
            Ok(()) => {
                self.stats.device_writes += 1;
                // Invalidate rather than store. See the module docs: a cached
                // copy of a sector the device has just been told to change is
                // a cache that would verify its own write.
                let line = (lba as usize) % CACHE_LINES;
                if self.cache[line].tag == lba {
                    self.cache[line].valid = false;
                }
                Ok(())
            }
            Err(error) => {
                self.stats.errors += 1;
                Err(error)
            }
        }
    }

    /// Flush the device.
    fn flush(&mut self) -> Result<(), BlockError> {
        match self.device.flush() {
            Ok(()) => {
                self.stats.device_flushes += 1;
                Ok(())
            }
            Err(error) => {
                self.stats.errors += 1;
                Err(error)
            }
        }
    }
}

/// Every disk the kernel found.
pub struct BlockState {
    disks: [Option<Disk>; MAX_DISKS],
    len: usize,
}

impl BlockState {
    /// No disks. A `const fn` so the table can be a static initialiser — the
    /// disks hold frames and raw pointers, none of which can be built in a
    /// const context, so they are installed by [`init`] instead.
    const fn new() -> Self {
        Self {
            disks: [None, None],
            len: 0,
        }
    }
}

/// The block state, behind an `UnsafeCell`.
///
/// Same arrangement as the service state: guarded by the kernel's recursive
/// spin lock, because user threads on application processors can reach
/// `with_disk` while the boot CPU does.
struct SingleBlockState {
    inner: SpinLock<BlockState>,
}

static STATE: SingleBlockState = SingleBlockState {
    inner: SpinLock::new(BlockState::new()),
};

/// The block state.
///
fn state() -> SpinLockGuard<'static, BlockState> {
    STATE.inner.lock()
}

/// Find every VirtIO block device and take it over.
///
/// Returns how many disks are live. A device that fails to initialize is
/// skipped with a log line rather than aborting the boot: a broken disk is a
/// missing filesystem, not a kernel that will not start.
pub fn init() -> usize {
    let devices = pci::enumerate();

    let mut found = 0usize;
    for device in devices.iter() {
        if found == MAX_DISKS {
            break;
        }
        if !device.is_virtio_block() {
            continue;
        }
        // A device ID that matches but with no capability list is legacy-only;
        // `VirtioBlk::open` reports that as `NoModernTransport`, and the log
        // says which device was skipped and why.
        match virtio::blk::VirtioBlk::open(device) {
            Ok(disk_device) => {
                klog!(
                    Level::Info,
                    "virtio-blk: {} at {} capacity {} sectors{}{}",
                    device.device_id,
                    device.location.formatted(),
                    disk_device.capacity_sectors(),
                    if disk_device.is_read_only() {
                        " read-only"
                    } else {
                        ""
                    },
                    if disk_device.supports_flush() {
                        " flush"
                    } else {
                        ""
                    },
                );
                // SAFETY: called once from boot with interrupts masked, before
                // any reader of the table exists.
                let mut state = state();
                let len = state.len;
                state.disks[len] = Some(Disk {
                    device: disk_device,
                    cache: [CacheLine::empty(); CACHE_LINES],
                    stats: DiskStats::default(),
                });
                state.len += 1;
                found += 1;
            }
            Err(error) => {
                klog!(
                    Level::Warn,
                    "virtio-blk: {} at {} refused to start: {}",
                    device.device_id,
                    device.location.formatted(),
                    error,
                );
            }
        }
    }

    if found == 0 {
        klog!(Level::Warn, "virtio-blk: no block device was started");
    }

    found
}

/// How many disks are live.
pub fn disk_count() -> usize {
    state().len
}

/// Run `f` with one disk.
///
/// A closure rather than a long-lived reference: the disk table is one static,
/// and a `&'static mut` handed out twice would be two mutable aliases to it.
/// Every caller therefore says what it needs in one call, and the borrow ends
/// where the call does.
pub fn with_disk<T>(index: usize, f: impl FnOnce(&mut Disk) -> T) -> Result<T, BlockError> {
    let mut state = state();
    let disk = state
        .disks
        .get_mut(index)
        .and_then(|slot| slot.as_mut())
        .ok_or(BlockError::NoCapacity)?;
    Ok(f(disk))
}

/// Stop every disk and give its frames back.
///
/// Nothing in the boot path calls this — the machine is about to be turned off
/// — but the suites do, to prove that a driver shut down cleanly leaves the
/// frame allocator exactly as it found it.
pub fn shutdown_all() {
    // SAFETY: as `disk_count`. Each disk is taken out of the table so a second
    // call cannot shut down a released device.
    let mut state = state();
    for slot in state.disks.iter_mut() {
        if let Some(mut disk) = slot.take() {
            disk.device.shutdown();
        }
    }
    state.len = 0;
}

/// A sector device backed by disk `id`.
///
/// `Copy` and one field wide on purpose: the filesystem holds a *handle* to a
/// disk, not the disk. `DiskFs` is generic over this trait, so the whole disk
/// machinery above stays out of `nova-fs` and the same `DiskFs` code runs
/// against a host-side array in that crate's own tests.
#[derive(Clone, Copy, Debug)]
pub struct KernelBlockDevice {
    id: usize,
}

impl KernelBlockDevice {
    /// A handle to disk `id`.
    pub const fn new(id: usize) -> Self {
        Self { id }
    }

    /// Which disk this handle names.
    pub const fn id(&self) -> usize {
        self.id
    }
}

impl BlockDevice for KernelBlockDevice {
    fn sector_count(&self) -> u64 {
        with_disk(self.id, |disk| disk.capacity_sectors()).unwrap_or(0)
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR_BYTES]) -> Result<(), DeviceError> {
        with_disk(self.id, |disk| disk.read(lba, out))
            .unwrap_or(Err(BlockError::NoCapacity))
            .map_err(to_device_error)
    }

    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR_BYTES]) -> Result<(), DeviceError> {
        with_disk(self.id, |disk| disk.write(lba, data))
            .unwrap_or(Err(BlockError::NoCapacity))
            .map_err(to_device_error)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        with_disk(self.id, |disk| disk.flush())
            .unwrap_or(Err(BlockError::NoCapacity))
            .map_err(to_device_error)
    }
}

/// Translate a driver failure into the filesystem's device error.
///
/// The mapping is lossy and deliberately so: `nova-fs` names five kinds of
/// device trouble, and this driver has nine. What does *not* get lost is the
/// distinction the filesystem's callers can act on — a read-only refusal stays
/// a read-only refusal, a timeout stays a timeout, and everything else becomes
/// "the device could not answer", which is the one thing every remaining case
/// has in common.
pub fn to_device_error(error: BlockError) -> DeviceError {
    match error {
        BlockError::ReadOnly => DeviceError::ReadOnly,
        BlockError::Timeout => DeviceError::Timeout,
        BlockError::OutOfRange => DeviceError::OutOfRange,
        BlockError::BadResponse => DeviceError::Fault,
        BlockError::Queue(_) => DeviceError::Fault,
        BlockError::Transport(_)
        | BlockError::Cancelled
        | BlockError::NoCapacity
        | BlockError::FlushUnsupported => DeviceError::Unavailable,
    }
}
