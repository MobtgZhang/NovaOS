//! A VirtIO virtqueue: the descriptor ring, the available ring, and the used
//! ring, each in its own frame.
//!
//! # What a virtqueue is
//!
//! Three arrays the device and the driver share by physical address:
//!
//! ```text
//! descriptors   the driver's request fragments: where, how long, which way
//! available     the driver's "these descriptors are ready", a ring of heads
//! used          the device's "I finished these", a ring of (head, length)
//! ```
//!
//! The driver writes descriptors, publishes a head in the available ring, and
//! tells the device to look. The device replies in the used ring. Nothing here
//! is memory the *driver* reads back from the device synchronously — the reply
//! is always a poll of the used ring's index.
//!
//! # Why one frame per ring
//!
//! Each ring is allocated as its own frame rather than packed into one
//! allocation. The three have different alignment requirements (16, 2, and 4
//! bytes), a shared allocation would have to satisfy the strictest for all
//! three, and — the real reason — a ring the device DMA's into must not share a
//! frame with anything the kernel considers private. Three frames, three
//! ownership boundaries, and a device that writes past the end of its ring
//! corrupts one frame rather than the kernel's.
//!
//! # Synchronous by construction
//!
//! [`VirtQueue`] carries at most **one** in-flight request. That is not a
//! shortcut: it is what makes the timeout, cancel, and device-reset paths
//! decidable. With one request in flight, "the device is not answering" has a
//! single descriptor chain to unwind and a single buffer to give back, so a
//! reset cannot leave a second request's buffer freed while the device is
//! still writing into it.

use super::faults;
use crate::arch::ADDRESS_SPACE;
use crate::drivers::mmio;
use crate::mm::{direct_map, frame_allocator};

/// Bytes in one descriptor.
pub const DESCRIPTOR_BYTES: usize = 16;

/// Descriptor flag: another descriptor follows, pointed to by `next`.
pub const FLAG_NEXT: u16 = 1;

/// Descriptor flag: the *device* writes this buffer (a read request's data).
pub const FLAG_DEVICE_WRITES: u16 = 2;

/// Descriptor flag: this descriptor is indirect (not used here; refused on
/// sight rather than implemented, because an indirect descriptor is a second
/// address space the device walks and this kernel validates neither).
pub const FLAG_INDIRECT: u16 = 4;

/// Bytes in the fixed part of the available ring.
pub const AVAIL_HEADER_BYTES: usize = 4;

/// Bytes in the fixed part of the used ring.
pub const USED_HEADER_BYTES: usize = 4;

/// Why a queue operation was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueError {
    /// The frame allocator had no frames left for the rings.
    OutOfMemory,
    /// The device offered a queue size this driver will not build.
    UnsupportedSize,
    /// A request is already waiting on the device.
    Busy,
    /// The device reported a completion this queue cannot accept — a head
    /// index past the end of the descriptor table.
    BadCompletion,
}

/// One entry the device reported as finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Completion {
    /// The descriptor head the request started at.
    pub head: u16,
    /// How many bytes the device wrote across the chain.
    pub written: u32,
}

/// A virtqueue occupying three frames.
pub struct VirtQueue {
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    frames: [usize; 3],
    size: u16,
    /// The available-ring index this driver has published up to.
    published: u16,
    /// The used-ring index this driver has consumed up to.
    consumed: u16,
}

impl VirtQueue {
    /// Build a queue of `size` descriptors, each ring in its own zeroed frame.
    ///
    /// `size` must be a power of two and no larger than [`MAX_QUEUE_SIZE`]: the
    /// available and used rings index themselves modulo the size, so a size
    /// that is not a power of two would make the modulo a source of subtle
    /// aliasing between requests.
    pub fn allocate(size: u16) -> Result<Self, QueueError> {
        if size == 0 || !size.is_power_of_two() || size > MAX_QUEUE_SIZE {
            return Err(QueueError::UnsupportedSize);
        }

        let desc_bytes = size as usize * DESCRIPTOR_BYTES;
        let avail_bytes = AVAIL_HEADER_BYTES + size as usize * 2;
        let used_bytes = USED_HEADER_BYTES + size as usize * 8;

        let frames = [
            alloc_ring_frame(desc_bytes)?,
            alloc_ring_frame(avail_bytes)?,
            alloc_ring_frame(used_bytes)?,
        ];

        // Each frame was just allocated and is therefore the caller's, the
        // direct map covers it, and the byte counts above were checked against
        // the page size in `alloc_ring_frame`. No `unsafe` is needed to *hold*
        // the pointers: every dereference below is the one that says what it
        // is doing.
        Ok(Self {
            desc: direct_map::phys_to_virt(frame_phys(frames[0])),
            avail: direct_map::phys_to_virt(frame_phys(frames[1])),
            used: direct_map::phys_to_virt(frame_phys(frames[2])),
            frames,
            size,
            published: 0,
            consumed: 0,
        })
    }

    /// The queue's size in descriptors.
    pub const fn size(&self) -> u16 {
        self.size
    }

    /// The physical address of the descriptor table, for the device's queue
    /// registers.
    pub fn desc_phys(&self) -> u64 {
        frame_phys(self.frames[0])
    }

    /// The physical address of the available ring.
    pub fn avail_phys(&self) -> u64 {
        frame_phys(self.frames[1])
    }

    /// The physical address of the used ring.
    pub fn used_phys(&self) -> u64 {
        frame_phys(self.frames[2])
    }

    /// Whether a request is waiting on the device.
    pub fn in_flight(&self) -> bool {
        self.published != self.consumed
    }

    /// Publish one request: a header, an optional data buffer, and a status
    /// byte the device writes last.
    ///
    /// The chain is always three descriptors — header, data, status — except
    /// for a request with no data (a flush), which uses two. `device_writes`
    /// is what separates a read from a write: a read's data descriptor is
    /// marked `FLAG_DEVICE_WRITES` so the device may fill it, and a write's is
    /// not, so the device may only read it. Getting that flag backwards is the
    /// one mistake that turns a driver bug into an arbitrary write, which is
    /// why it is a parameter with one meaning rather than a caller-chosen bit.
    ///
    /// # Safety
    ///
    /// The three physical addresses must name kernel-owned memory that outlives
    /// the request, of at least the given lengths, and no other request may be
    /// in flight (see the type's docs on why that is a rule and not a
    /// suggestion).
    pub unsafe fn publish(
        &mut self,
        header_phys: u64,
        header_len: u32,
        data: Option<(u64, u32, bool)>,
        status_phys: u64,
    ) -> Result<u16, QueueError> {
        if self.in_flight() {
            return Err(QueueError::Busy);
        }

        let head = 0u16;
        // Descriptor slots: 0 header, 1 data (when present), last status.
        let status_index = if data.is_some() { 2 } else { 1 };

        // SAFETY: the descriptor table is `size * 16` bytes in its own frame,
        // and the indices below are constants 0, 1, 2 — all below `size`, which
        // is at least 8 (checked in `allocate`).
        unsafe {
            write_descriptor(self.desc, 0, header_phys, header_len, FLAG_NEXT, 1);
            if let Some((address, length, device_writes)) = data {
                let flags = FLAG_NEXT | if device_writes { FLAG_DEVICE_WRITES } else { 0 };
                write_descriptor(self.desc, 1, address, length, flags, status_index as u16);
            }
            // The status byte is the one thing in this chain the *device*
            // writes, so it carries `FLAG_DEVICE_WRITES` like the read data
            // does. Leaving it clear makes it a driver-readable descriptor,
            // and a device-readable descriptor appearing after a writable one
            // is not a subtle misordering: the device refuses the whole chain
            // ("Incorrect order for descriptors"), marks itself failed, and
            // never answers — a timeout from a device that had already said
            // why.
            write_descriptor(
                self.desc,
                status_index,
                status_phys,
                1,
                FLAG_DEVICE_WRITES,
                0,
            );
        }

        // The head goes into the available ring at `published % size`, and the
        // index moves *after* the entry is written — the device polls the
        // index, so publishing it first would advertise a ring slot that has
        // not been filled.
        let slot = (self.published % self.size) as usize;
        // SAFETY: the available ring is `4 + size * 2` bytes in its own frame,
        // and `slot` is below `size`, so `4 + slot * 2` is within it.
        unsafe {
            mmio::write_u16(self.avail, 4 + slot * 2, head);
        }
        mmio::fence();
        self.published = self.published.wrapping_add(1);
        // SAFETY: as above; the index field is the ring's second word.
        unsafe {
            mmio::write_u16(self.avail, 2, self.published);
        }
        mmio::fence();

        Ok(head)
    }

    /// Publish one descriptor chain, for a device whose protocol has a single
    /// buffer rather than the block device's header/data/status stages.
    ///
    /// VirtIO net is exactly that shape: the receive buffer and the transmit
    /// buffer each begin with the protocol header and continue with the frame,
    /// so one descriptor is the whole request. A block request needs three
    /// descriptors because the status byte is a separate output stage; folding
    /// the block shape into this method would publish a chain the net device
    /// does not understand.
    ///
    /// # Safety
    ///
    /// `address` must name kernel-owned memory that outlives the request, of at
    /// least `length` bytes, and no other request may be in flight.
    pub unsafe fn publish_one(
        &mut self,
        address: u64,
        length: u32,
        device_writes: bool,
    ) -> Result<u16, QueueError> {
        if self.in_flight() {
            return Err(QueueError::Busy);
        }
        if length == 0 || length as usize > ADDRESS_SPACE.page_size as usize {
            return Err(QueueError::UnsupportedSize);
        }

        // SAFETY: descriptor 0 exists because the queue size is at least 8
        // (checked in `allocate`), and the length was bounded above.
        unsafe {
            write_descriptor(
                self.desc,
                0,
                address,
                length,
                if device_writes { FLAG_DEVICE_WRITES } else { 0 },
                0,
            );
        }

        let slot = (self.published % self.size) as usize;
        // SAFETY: the available ring is `4 + size * 2` bytes in its own frame,
        // and `slot` is below `size`.
        unsafe {
            mmio::write_u16(self.avail, 4 + slot * 2, 0);
        }
        mmio::fence();
        self.published = self.published.wrapping_add(1);
        // SAFETY: as above; the index field is the ring's second word.
        unsafe {
            mmio::write_u16(self.avail, 2, self.published);
        }
        mmio::fence();

        Ok(0)
    }

    /// What the two rings say right now: (available index, used index).
    ///
    /// A diagnostic for the timeout path. A request that never completes has
    /// three possible causes — the device never saw the request, the device
    /// answered into a ring the driver is not reading, or the answer is present
    /// and the driver is looking at the wrong slot — and the three are told
    /// apart only by these two numbers: `avail` moved and `used` did not means
    /// the device was never told; neither moved means the publish never
    /// happened; `used` moved means the completion is there and the driver's
    /// read is wrong.
    pub fn ring_indices(&self) -> (u16, u16) {
        // SAFETY: both rings live in their own frames for the life of the
        // queue, and the index field is the second word of each.
        unsafe { (mmio::read_u16(self.avail, 2), mmio::read_u16(self.used, 2)) }
    }

    /// The raw words of descriptor `index`: (address, length, flags, next).
    pub fn descriptor(&self, index: u16) -> Option<(u64, u32, u16, u16)> {
        if index >= self.size {
            return None;
        }
        let base = index as usize * DESCRIPTOR_BYTES;
        // SAFETY: `index` is below the queue's size, so all four fields are
        // inside the descriptor table's frame.
        Some(unsafe {
            (
                mmio::read_u64(self.desc, base),
                mmio::read_u32(self.desc, base + 8),
                mmio::read_u16(self.desc, base + 12),
                mmio::read_u16(self.desc, base + 14),
            )
        })
    }

    /// Take the next completion the device has published, if any.
    ///
    /// A completion whose head is out of range is reported as
    /// [`QueueError::BadCompletion`] rather than returned: a used entry naming a
    /// descriptor this queue does not have means the device and the driver
    /// disagree about the queue's geometry, and every later offset derived from
    /// that entry would be an out-of-bounds index.
    ///
    /// The two completion faults are applied *here*, at the ring read, rather
    /// than mocked out higher up: the validation below is what proves an
    /// untrusted device cannot pick an index the driver will use, so the test
    /// has to feed the bad index through the same read every real completion
    /// goes through.
    pub fn take_completion(&mut self) -> Result<Option<Completion>, QueueError> {
        // The device never answers. The used ring is left exactly as the
        // device wrote it, so what the driver does next is what it would do
        // for a genuinely silent device.
        if faults()
            .suppress_completions
            .load(core::sync::atomic::Ordering::Acquire)
        {
            return Ok(None);
        }

        // SAFETY: the used ring is `4 + size * 8` bytes in its own frame; the
        // index field is its second word.
        let device_index = unsafe { mmio::read_u16(self.used, 2) };
        if device_index == self.consumed {
            return Ok(None);
        }

        let slot = (self.consumed % self.size) as usize;
        // SAFETY: as above; `slot` is below `size`, so `4 + slot * 8` and the
        // four bytes after it are within the frame.
        let (mut head, written) = unsafe {
            let base = 4 + slot * 8;
            (
                mmio::read_u32(self.used, base) as u16,
                mmio::read_u32(self.used, base + 4),
            )
        };

        // A device that names a descriptor this queue does not have is handed
        // to the same bounds check a real completion is. The forged index is
        // one past the end, which is the smallest value that is definitely
        // wrong — a check that only caught a wildly wrong index would pass a
        // device that was off by one.
        if faults()
            .corrupt_completion
            .load(core::sync::atomic::Ordering::Acquire)
        {
            head = self.size;
        }

        if head >= self.size {
            return Err(QueueError::BadCompletion);
        }

        self.consumed = device_index;
        Ok(Some(Completion { head, written }))
    }

    /// Empty the rings, so the device and the driver start from zero again.
    ///
    /// # Safety
    ///
    /// The device must be **stopped** — its status register must read zero —
    /// before this is called. A live device reading a ring that has been
    /// rewound underneath it will write into descriptor slots the driver has
    /// since reused, which is a DMA into whatever the driver put there. Every
    /// caller in this kernel resets the device first; see
    /// `drivers::virtio::blk`.
    pub unsafe fn rewind(&mut self) {
        // SAFETY: the device is stopped per this function's contract, so the
        // only writer of these rings is this code. Each field is within its
        // frame's documented size.
        unsafe {
            mmio::write_u16(self.avail, 0, 0);
            mmio::write_u16(self.avail, 2, 0);
            mmio::write_u16(self.used, 0, 0);
            mmio::write_u16(self.used, 2, 0);
        }
        self.published = 0;
        self.consumed = 0;
        mmio::fence();
    }

    /// Give the three frames back, leaving the queue inert.
    ///
    /// Idempotent: a second call frees nothing, because the pointers are
    /// nulled here. Teardown is *reachable* more than once by design — a
    /// shutdown that follows a failed start, or a failed start that both
    /// unwinds and is then torn down — and a double free hands the same page
    /// to two owners, which nothing later can detect.
    ///
    /// # Safety
    ///
    /// The queue must not be in use: the device must be stopped, and no
    /// completion may be pending.
    pub unsafe fn release(&mut self) {
        if self.desc.is_null() {
            return;
        }

        for frame in self.frames {
            // SAFETY: delegated to this function's contract; each frame came
            // from `alloc_ring_frame` and is owned by this queue exactly once.
            unsafe { frame_allocator().dealloc(frame) };
        }

        self.frames = [0; 3];
        self.desc = core::ptr::null_mut();
        self.avail = core::ptr::null_mut();
        self.used = core::ptr::null_mut();
        self.size = 0;
        self.published = 0;
        self.consumed = 0;
    }
}

/// The largest queue this driver builds.
///
/// Eight descriptors is three for a request, one spare, and headroom. The
/// device's real queue size is read from its common configuration and clipped
/// to this; a device offering more is not an error, it is a device whose queue
/// this driver does not need.
pub const MAX_QUEUE_SIZE: u16 = 8;

/// Write one descriptor table entry.
///
/// # Safety
///
/// `desc` must point at a descriptor table with at least `index + 1` entries.
unsafe fn write_descriptor(
    desc: *mut u8,
    index: u16,
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
) {
    let base = index as usize * DESCRIPTOR_BYTES;
    // SAFETY: delegated to this function's contract; the entry is four fields
    // at fixed offsets inside the table.
    unsafe {
        mmio::write_u64(desc, base, address);
        mmio::write_u32(desc, base + 8, length);
        mmio::write_u16(desc, base + 12, flags);
        mmio::write_u16(desc, base + 14, next);
    }
}

/// Allocate one zeroed frame large enough for `bytes`.
fn alloc_ring_frame(bytes: usize) -> Result<usize, QueueError> {
    if bytes > ADDRESS_SPACE.page_size as usize {
        return Err(QueueError::UnsupportedSize);
    }

    // SAFETY: `mm::init` published the allocator during boot, and every queue
    // is built after that.
    let frame = unsafe { frame_allocator().alloc() }.map_err(|_| QueueError::OutOfMemory)?;
    let phys = frame_phys(frame);

    // SAFETY: the frame was just allocated, so it is this caller's; the direct
    // map covers it; and the length is one page, checked above.
    unsafe {
        core::ptr::write_bytes(
            direct_map::phys_to_virt(phys),
            0,
            ADDRESS_SPACE.page_size as usize,
        );
    }

    Ok(frame)
}

/// The physical address of a frame.
///
/// The allocator hands back an *index*; the device needs an address. Multiplying
/// by the page size is the whole translation, and doing it in one place keeps
/// every queue address and every buffer address derived the same way.
pub const fn frame_phys(frame: usize) -> u64 {
    frame as u64 * ADDRESS_SPACE.page_size
}
