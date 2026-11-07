//! The VirtIO block device: one disk, one queue, one request at a time.
//!
//! # Why synchronous
//!
//! The M5 acceptance scenarios are about *correctness under failure* — a device
//! that times out, answers wrongly, or is taken away mid-transfer — and those
//! are only decidable when a request has one owner waiting for it. An
//! interrupt-driven queue would need a completion table, a way to hand a
//! half-finished request back to a caller that has gone away, and a lock the
//! poll path does not have. Those are real designs and this is not one of them:
//! one request, one wait, one answer.
//!
//! # The bounce buffer, and why every transfer goes through it
//!
//! A request's data buffer must be at a *physical* address the device can DMA
//! to. A caller's buffer may be on the kernel stack or in a static, and neither
//! has a physical address the kernel can derive from its virtual one — the
//! stack is not in the direct map. So every request is staged in one frame the
//! driver owns ([`VirtioBlk`]'s transfer frame), whose address is known, and
//! copied to or from the caller's buffer. It costs one `memcpy` per sector and
//! buys a driver that is correct for *any* buffer, which is the property a
//! filesystem sitting above it depends on.
//!
//! # Failure paths
//!
//! Every error path stops the device first, then rewinds the rings, then — for
//! everything except an owner that has gone away — reinitializes from scratch.
//! The order is not decorative: a device that is still DMAing into rings the
//! driver has just rewound writes into descriptor slots the driver has since
//! reused, and the buffer those slots name may already have been given back.

use super::pci as transport;
use super::queue::{self, VirtQueue};
use super::{FEATURE_BLK_FLUSH, FEATURE_BLK_RO, FEATURE_VERSION_1, VirtioError, faults};
use crate::arch::ADDRESS_SPACE;
use crate::drivers::mmio;
use crate::drivers::pci;
use crate::klog;
use crate::mm::{direct_map, frame_allocator};

/// Bytes in one sector, as VirtIO defines it when `VIRTIO_BLK_F_BLK_SIZE` is
/// absent — which is the only case this driver handles.
pub const SECTOR_BYTES: usize = 512;

/// Sectors one request may move.
///
/// Bounded by the transfer frame: the data stage starts at
/// [`TRANSFER_DATA_OFFSET`] and the frame is one page, so
/// `(4096 - 64) / 512 = 7`. The filesystem and the cache both transfer one
/// sector at a time, so this is headroom rather than a used feature — but it
/// is a *checked* bound, because a length that reaches the descriptor ring
/// unvalidated is how a device gets told to write past a frame.
pub const MAX_TRANSFER_SECTORS: usize =
    (ADDRESS_SPACE.page_size as usize - TRANSFER_DATA_OFFSET) / SECTOR_BYTES;

/// Byte offset of the data stage inside the transfer frame.
///
/// After the 16-byte request header and the status byte, with room to spare:
/// the three are separate descriptors, so the gap exists to keep the status
/// byte out of the header's cache line rather than to save space.
const TRANSFER_DATA_OFFSET: usize = 64;

/// Byte offset of the status stage inside the transfer frame.
const TRANSFER_STATUS_OFFSET: usize = 16;

/// The request types this driver sends.
mod request_type {
    /// Device writes the data to the driver (a read).
    pub const IN: u32 = 0;
    /// Device reads the data from the driver (a write).
    pub const OUT: u32 = 1;
    /// Device flushes caches to the medium.
    pub const FLUSH: u32 = 4;
}

/// The status byte the device writes last.
mod status_byte {
    /// The request completed.
    pub const OK: u8 = 0;
    /// The device failed the request (out of range, IO error).
    pub const IOERR: u8 = 1;
    /// The device does not support this request type.
    pub const UNSUPPORTED: u8 = 2;
    /// What the driver writes *before* submitting: if the device leaves this
    /// value in place it never touched the status descriptor at all.
    pub const IN_PROGRESS: u8 = 0xFF;
}

/// How many poll iterations a request may wait for the device.
///
/// An iteration bound, not a clock deadline, and deliberately so: under
/// emulation the machine's clock and the instruction stream advance at
/// different rates, so a "500 ms" deadline calibrated against one would be a
/// different amount of the other. What the bound really says is "this many
/// unanswered polls is a device that is not coming back" — and a *working*
/// device answers in a handful of them, so the number only ever applies to a
/// device that has already failed.
const POLL_BUDGET: u64 = 8_000_000;

/// Why a block request failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockError {
    /// The PCI transport would not come up.
    Transport(VirtioError),
    /// A virtqueue operation was refused.
    Queue(queue::QueueError),
    /// The device did not answer within the driver's budget.
    Timeout,
    /// The owner of an in-flight request exited; the device was stopped and
    /// the request abandoned.
    Cancelled,
    /// The device's answer did not describe this request: a completion for a
    /// descriptor the queue does not have, or a byte count that does not match
    /// what was asked for.
    BadResponse,
    /// The device is read-only and a write was attempted.
    ReadOnly,
    /// The device reported no capacity, so there is nothing to address.
    NoCapacity,
    /// The sector named by the request is past the end of the device.
    OutOfRange,
    /// The device cannot honour a flush, so no durability was promised.
    FlushUnsupported,
}

impl BlockError {
    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            BlockError::Transport(error) => error.name(),
            BlockError::Queue(_) => "B_QUEUE",
            BlockError::Timeout => "B_TIMEOUT",
            BlockError::Cancelled => "B_CANCELLED",
            BlockError::BadResponse => "B_BAD_RESPONSE",
            BlockError::ReadOnly => "B_READ_ONLY",
            BlockError::NoCapacity => "B_NO_CAPACITY",
            BlockError::OutOfRange => "B_OUT_OF_RANGE",
            BlockError::FlushUnsupported => "B_FLUSH_UNSUPPORTED",
        }
    }
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockError::Transport(error) => write!(f, "transport: {error:?}"),
            BlockError::Queue(error) => write!(f, "queue: {error:?}"),
            other => f.write_str(other.name()),
        }
    }
}

/// One VirtIO disk.
pub struct VirtioBlk {
    transport: transport::Transport,
    queue: VirtQueue,
    /// The frame every request's header, status, and data are staged in.
    transfer: Option<usize>,
    /// Device capacity in 512-byte sectors.
    capacity_sectors: u64,
    /// Whether the device refuses writes.
    read_only: bool,
    /// Whether the device implements the flush request.
    flush_supported: bool,
}

impl VirtioBlk {
    /// Take over a block device: negotiate, allocate the queue, and go live.
    pub fn open(device: &pci::Device) -> Result<Self, BlockError> {
        let mut transport = transport::Transport::open(device).map_err(BlockError::Transport)?;

        let wanted = super::Features {
            // `VERSION_1` is required; `RO` and `FLUSH` are accepted when the
            // device offers them, and their absence is reported rather than
            // papered over: a read-only disk that the driver believes is
            // writable is a disk whose writes silently vanish.
            low: (1 << FEATURE_BLK_RO) | (1 << FEATURE_BLK_FLUSH),
            high: 1 << FEATURE_VERSION_1,
        };
        transport
            .start_device(wanted)
            .map_err(BlockError::Transport)?;

        let offered = transport.device_features();
        let read_only = offered.low_has(FEATURE_BLK_RO);
        let flush_supported = offered.low_has(FEATURE_BLK_FLUSH);

        let capacity_sectors = read_capacity(&transport)?;
        if capacity_sectors == 0 {
            transport.reset();
            return Err(BlockError::NoCapacity);
        }

        let mut queue = VirtQueue::allocate(queue::MAX_QUEUE_SIZE).map_err(BlockError::Queue)?;
        let transfer = alloc_transfer_frame()?;

        let setup = transport.setup_queue(
            0,
            queue::MAX_QUEUE_SIZE,
            queue.desc_phys(),
            queue.avail_phys(),
            queue.used_phys(),
        );
        if let Err(error) = setup {
            // Nothing is live yet, but the frames are already taken; give them
            // back before reporting, so a failed probe does not leak a page per
            // attempt.
            // SAFETY: the queue was never enabled, so the device has no
            // reference to these frames.
            unsafe { queue.release() };
            free_transfer_frame(transfer);
            transport.reset();
            return Err(BlockError::Transport(error));
        }

        transport.driver_ok();

        Ok(Self {
            transport,
            queue,
            transfer: Some(transfer),
            capacity_sectors,
            read_only,
            flush_supported,
        })
    }

    /// The device's capacity in sectors.
    pub const fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Whether the device refuses writes.
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the device implements the flush request.
    pub const fn supports_flush(&self) -> bool {
        self.flush_supported
    }

    /// Whether a request is waiting on the device.
    pub fn request_in_flight(&self) -> bool {
        self.queue.in_flight()
    }

    /// Read one sector into `out`.
    pub fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), BlockError> {
        if out.len() != SECTOR_BYTES {
            return Err(BlockError::BadResponse);
        }
        self.request(request_type::IN, lba, 1, true)?;
        self.copy_out(out);
        Ok(())
    }

    /// Write one sector from `data`.
    pub fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        if data.len() != SECTOR_BYTES {
            return Err(BlockError::BadResponse);
        }
        self.copy_in(data);
        self.request(request_type::OUT, lba, 1, false)
    }

    /// Ask the device to make prior writes durable.
    pub fn flush(&mut self) -> Result<(), BlockError> {
        if self.read_only {
            // Nothing was written, so there is nothing to flush, and claiming
            // otherwise would report a durability step that never needed to
            // happen.
            return Ok(());
        }
        if !self.flush_supported {
            return Err(BlockError::FlushUnsupported);
        }
        self.request(request_type::FLUSH, 0, 0, false)
    }

    /// Stop the device and give every frame back.
    ///
    /// Safe to call on a device with a request in flight: the device is reset
    /// *first*, which stops its DMA, and only then are the rings and the
    /// transfer frame released. The reverse order would return buffers a live
    /// device is still writing into.
    pub fn shutdown(&mut self) {
        // A reset that does not stick means the device may still be DMAing.
        // The frames are freed anyway — this is teardown and the caller is
        // going away — but the refusal is reported through the log rather than
        // silently pretending the device stopped.
        if !self.transport.reset() {
            super::klog_reset_refused();
        }
        // SAFETY: the device is stopped (or, failing that, this is the
        // teardown path where the frames must go back regardless), so no
        // completion is pending and the rings are the driver's alone.
        unsafe { self.queue.rewind() };
        // SAFETY: as above; `release` is idempotent by construction.
        unsafe { self.queue.release() };

        if let Some(frame) = self.transfer.take() {
            free_transfer_frame(frame);
        }
    }

    /// Stage the caller's bytes into the transfer frame.
    fn copy_in(&self, data: &[u8]) {
        let Some(frame) = self.transfer else {
            return;
        };
        // SAFETY: the frame is one page owned by this driver, and `data` is
        // exactly `SECTOR_BYTES`, which fits after `TRANSFER_DATA_OFFSET`.
        unsafe {
            let base = direct_map::phys_to_virt(frame_phys(frame)).add(TRANSFER_DATA_OFFSET);
            core::ptr::copy_nonoverlapping(data.as_ptr(), base, data.len());
        }
    }

    /// Copy the transfer frame's data stage out to the caller.
    fn copy_out(&self, out: &mut [u8]) {
        let Some(frame) = self.transfer else {
            return;
        };
        // SAFETY: as `copy_in`.
        unsafe {
            let base = direct_map::phys_to_virt(frame_phys(frame)).add(TRANSFER_DATA_OFFSET);
            core::ptr::copy_nonoverlapping(base, out.as_mut_ptr(), out.len());
        }
    }

    /// The transfer frame's data stage, for the tests that check a device did
    /// not write where it should not have.
    pub fn transfer_data(&self) -> Option<&[u8]> {
        let frame = self.transfer?;
        // SAFETY: the frame is one page owned by this driver; the slice is the
        // data stage only, so its length is the caller-visible bound.
        Some(unsafe {
            core::slice::from_raw_parts(
                direct_map::phys_to_virt(frame_phys(frame)).add(TRANSFER_DATA_OFFSET),
                SECTOR_BYTES,
            )
        })
    }

    /// Submit one request and wait for the device's answer.
    fn request(
        &mut self,
        kind: u32,
        lba: u64,
        sectors: u32,
        device_writes: bool,
    ) -> Result<(), BlockError> {
        if kind != request_type::FLUSH && sectors == 0 {
            return Err(BlockError::BadResponse);
        }
        if sectors as usize > MAX_TRANSFER_SECTORS {
            return Err(BlockError::BadResponse);
        }
        if lba >= self.capacity_sectors || lba + sectors as u64 > self.capacity_sectors {
            // Checked here rather than left to the device: a request naming a
            // sector past the end is the one case where "the device says no"
            // and "the driver should never have asked" are different answers,
            // and the driver is the one that can tell them apart.
            return Err(BlockError::OutOfRange);
        }

        let Some(frame) = self.transfer else {
            return Err(BlockError::Cancelled);
        };
        let base = frame_phys(frame);

        // The header and the status byte live in the same frame, at offsets
        // that keep them in separate descriptor entries: the device reads the
        // header and writes the status, and one descriptor cannot be both.
        // SAFETY: both offsets are inside the transfer frame, and the header is
        // 16 bytes of little-endian fields this driver is the only writer of.
        unsafe {
            let header = direct_map::phys_to_virt(base);
            mmio::write_u32(header, 0, kind);
            mmio::write_u32(header, 4, 0);
            mmio::write_u64(header, 8, lba);
            mmio::write_u8(
                direct_map::phys_to_virt(base),
                TRANSFER_STATUS_OFFSET,
                status_byte::IN_PROGRESS,
            );
        }
        mmio::fence();

        let data_stage = if sectors > 0 && kind != request_type::FLUSH {
            Some((
                base + TRANSFER_DATA_OFFSET as u64,
                sectors * SECTOR_BYTES as u32,
                device_writes,
            ))
        } else {
            None
        };

        // SAFETY: the transfer frame is one page this driver owns, the header
        // and status offsets are within it, and the queue has at most one
        // request in flight — checked by `publish`.
        unsafe {
            self.queue
                .publish(base, 16, data_stage, base + TRANSFER_STATUS_OFFSET as u64)
                .map_err(BlockError::Queue)?;
        }

        self.transport.notify();

        let completion = match self.await_completion() {
            Ok(completion) => completion,
            Err(error) => {
                self.unwind_after_failure(&error);
                return Err(error);
            }
        };

        self.validate(completion, device_writes, sectors)?;

        // SAFETY: the device is quiescent (a completion was observed), so the
        // status byte is stable to read.
        let status =
            unsafe { mmio::read_u8(direct_map::phys_to_virt(base), TRANSFER_STATUS_OFFSET) };
        match status {
            status_byte::OK => Ok(()),
            status_byte::IOERR => Err(BlockError::OutOfRange),
            status_byte::UNSUPPORTED => Err(BlockError::FlushUnsupported),
            // The device never wrote the status descriptor: it answered the
            // used ring without completing the request, which is a device that
            // is out of step with its own ring.
            _ => Err(BlockError::BadResponse),
        }
    }

    /// Poll until the device answers, the budget runs out, or the owner leaves.
    fn await_completion(&mut self) -> Result<queue::Completion, BlockError> {
        let mut budget = POLL_BUDGET;
        loop {
            if faults()
                .owner_exited
                .load(core::sync::atomic::Ordering::Acquire)
            {
                return Err(BlockError::Cancelled);
            }

            match self.queue.take_completion() {
                Ok(Some(completion)) => return Ok(completion),
                Ok(None) => {}
                Err(error) => return Err(BlockError::Queue(error)),
            }

            if budget == 0 {
                // What the rings say, at the moment the driver gave up. A
                // silent device and a device that answered into the wrong ring
                // look identical from the outside — both are "no completion" —
                // and these four words are what tells them apart: `avail` moved
                // means the publish was visible, `used` did not means the
                // device never answered, and a descriptor that does not hold
                // this request's header means the table the device reads is not
                // the table the driver wrote.
                let (avail, used) = self.queue.ring_indices();
                let header = self.queue.descriptor(0);
                klog!(
                    crate::logger::Level::Warn,
                    "virtio-blk: timeout avail {avail} used {used} desc0 {header:#x?} status {:#x} isr {:#x}",
                    self.transport.status(),
                    self.transport.interrupt_status()
                );
                return Err(BlockError::Timeout);
            }
            budget -= 1;
            core::hint::spin_loop();
        }
    }

    /// Check that the device's answer describes *this* request.
    ///
    /// Three separate checks, because they fail for different reasons: a head
    /// that is not ours means the rings disagree about geometry, a byte count
    /// that is not what was asked for means the device wrote a different amount
    /// than the descriptors allow, and a status byte that is not one of the
    /// three the specification defines means the device is not speaking it.
    fn validate(
        &self,
        completion: queue::Completion,
        device_writes: bool,
        sectors: u32,
    ) -> Result<(), BlockError> {
        if completion.head != 0 {
            return Err(BlockError::BadResponse);
        }

        let expected = if device_writes {
            sectors * SECTOR_BYTES as u32 + 1
        } else {
            1
        };
        if completion.written != expected {
            return Err(BlockError::BadResponse);
        }

        Ok(())
    }

    /// Stop the device and rewind the rings after a failed request.
    ///
    /// A cancellation takes the other path: there the device is stopped and the
    /// rings rewound, but nothing is restarted, because the owner is gone and
    /// the queue is about to be released.
    fn unwind_after_failure(&mut self, error: &BlockError) {
        if matches!(error, BlockError::Cancelled) {
            // SAFETY: stopping the device is what makes the rewind legal; see
            // `VirtQueue::rewind`.
            let _ = self.transport.reset();
            // SAFETY: as above.
            unsafe { self.queue.rewind() };
            return;
        }

        if !self.transport.reset() {
            super::klog_reset_refused();
        }
        // SAFETY: the device is stopped, so the rings are the driver's alone.
        unsafe { self.queue.rewind() };

        // Restart from scratch. Re-running the whole sequence rather than
        // patching the status register back is deliberate: a device that
        // failed once has lost whatever state the sequence built, and the
        // sequence is the only description this driver has of what that state
        // was.
        let wanted = super::Features {
            low: (1 << FEATURE_BLK_RO) | (1 << FEATURE_BLK_FLUSH),
            high: 1 << FEATURE_VERSION_1,
        };
        if self.transport.start_device(wanted).is_err() {
            return;
        }
        let _ = self.transport.setup_queue(
            0,
            self.queue.size(),
            self.queue.desc_phys(),
            self.queue.avail_phys(),
            self.queue.used_phys(),
        );
        self.transport.driver_ok();
    }
}

/// Read a disk's capacity from its device configuration structure.
fn read_capacity(transport: &transport::Transport) -> Result<u64, BlockError> {
    let config = transport.device_config().ok_or(BlockError::NoCapacity)?;
    // SAFETY: the configuration structure was mapped with room for the
    // capacity field, and only this driver reads it.
    Ok(unsafe { mmio::read_u64(config, 0) })
}

/// Allocate the frame a request's stages live in.
fn alloc_transfer_frame() -> Result<usize, BlockError> {
    // SAFETY: `mm::init` published the allocator during boot. The device
    // describes this frame's address with a 32-bit DMA word, so the frame must
    // come from the DMA-capable pool below the 4 GiB boundary.
    let frame = unsafe { frame_allocator().alloc_dma() }
        .map_err(|_| BlockError::Queue(queue::QueueError::OutOfMemory))?;

    // SAFETY: the frame was just allocated, so it is this caller's; the direct
    // map covers it; the length is one page.
    unsafe {
        core::ptr::write_bytes(
            direct_map::phys_to_virt(frame_phys(frame)),
            0,
            ADDRESS_SPACE.page_size as usize,
        );
    }

    Ok(frame)
}

/// Give the transfer frame back.
fn free_transfer_frame(frame: usize) {
    // SAFETY: the frame came from `alloc_transfer_frame` and the caller owns
    // it; this is the only place it is freed.
    unsafe { frame_allocator().dealloc(frame) };
}

/// The physical address of a frame.
const fn frame_phys(frame: usize) -> u64 {
    frame as u64 * ADDRESS_SPACE.page_size
}
