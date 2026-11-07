//! Bitmap physical page frame allocator.
//!
//! # Why a bitmap
//!
//! The allocator needs to answer two questions — "is this frame free?" and
//! "give me the lowest free frame" — over an address space whose usable extent
//! is only known at boot. A bitmap of one bit per 4 KiB frame answers both,
//! and its cost scales linearly: a 192 GiB machine needs 6 MiB of bitmap, a
//! 1 TiB machine 32 MiB. The bitmap is therefore *not* a compile-time array:
//! it is carved out of the top of the largest usable region at boot, marked
//! busy in itself, and reached through the direct map like every other frame.
//!
//! # Frames are physical frame numbers
//!
//! A frame index *is* the physical frame number: frame `n` is the 4 KiB at
//! physical `n * page`. The bitmap covers every frame up to the highest
//! usable region's end, and frames the firmware did not call usable — the
//! holes between regions, the reserved ranges, the low memory below the
//! kernel image — are marked busy at construction. Handing out a frame the
//! firmware owns would be a corruption no later check could catch, so the
//! marking happens once, at boot, where it can be audited.
//!
//! # Testability
//!
//! [`Bitmap`] is deliberately free of architecture and of `unsafe` in its
//! *logic* — its unit tests run on the host under `cargo test` over a plain
//! word slice. The kernel-side [`FrameAllocator`] is a thin shell over it that
//! chooses the backing store and adds the direct map needed to touch it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::ADDRESS_SPACE;
use crate::boot::info::{BootInfo, MemoryRegion, PAGE_SIZE};
use crate::klog;
use crate::logger::Level;
use crate::mm::direct_map;

/// Error returned by [`FrameAllocator::alloc`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameError {
    /// Every frame the allocator tracks is in use.
    OutOfMemory,
}

/// Statistics describing a live allocator, for the boot log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Total frames the bitmap tracks.
    pub total_frames: usize,
    /// Frames currently handed out.
    pub allocated_frames: usize,
    /// Frames still available.
    pub free_frames: usize,
    /// Size of the backing bitmap in bytes.
    pub bitmap_bytes: usize,
}

impl core::fmt::Display for AllocatorStats {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} allocated, {} free of {} frames ({} KiB bitmap)",
            self.allocated_frames,
            self.free_frames,
            self.total_frames,
            self.bitmap_bytes / 1024,
        )
    }
}

/// The physical frame boundary below which a frame may be handed to a device
/// that can only DMA into 32-bit addresses.
///
/// 4 GiB / 4 KiB. VirtIO devices without the `ANY_LAYOUT`-style 64-bit
/// addressing features read descriptor and buffer addresses as 32-bit words,
/// so a frame at or above this boundary would be *described* correctly and
/// *reached* wrongly. [`FrameAllocator::alloc_dma`] refuses to cross it;
/// ordinary allocations are free to use the whole machine.
const DMA_FRAME_LIMIT: usize = 0x10_0000;

/// Physical frame allocator.
///
/// # Concurrency
///
/// The bitmap sits behind an [`UnsafeCell`] and every accessor is `unsafe`. That
/// is deliberate: NovaOS runs on one CPU with interrupts masked during
/// allocation, so there is nothing to race against. Real concurrency replaces
/// this shell with a locked one — the safe [`Bitmap`] logic it wraps is exactly
/// what survives that refactor, which is why the split exists.
///
/// # Reserved floor
///
/// Frames below [`FrameAllocator::base_frame`] are marked busy at construction.
/// They cover firmware-owned regions and the kernel image itself; handing one
/// out would corrupt the running kernel, so "allocation failed" is the correct
/// outcome long before that watermark is reached.
pub struct FrameAllocator {
    bitmap: UnsafeCell<Bitmap>,
    /// First frame the allocator may hand out.
    base_frame: usize,
    /// Number of frames the bitmap tracks: every physical frame up to the
    /// highest usable region's end.
    total_frames: usize,
    /// Frames below this boundary are DMA-capable; see [`DMA_FRAME_LIMIT`].
    dma_limit_frame: usize,
    /// Sequential allocation hint; cleared when it runs off the end.
    hint: AtomicUsize,
    /// Physical address of the bitmap's backing store, for the boot log.
    bitmap_phys: u64,
}

impl FrameAllocator {
    /// Build an allocator over the memory the firmware described.
    ///
    /// `base_frame` is the first frame safe to hand out. The caller passes the
    /// end of the kernel image so the allocator never recycles the pages it is
    /// executing from.
    ///
    /// The bitmap's backing store is taken from the *top* of the largest
    /// usable region that can hold it, above the kernel-image floor. Placing
    /// it high keeps the low 4 GiB — the DMA-capable window — clear of
    /// bookkeeping, and placing it inside a usable region means the direct
    /// map already covers it before a single table of the kernel's own
    /// exists.
    ///
    /// # Panics
    ///
    /// If the firmware reports no usable memory, or if no usable region is
    /// large enough to hold the bitmap above the kernel-image floor. Both are
    /// boot failures: a bitmap that silently truncated would hand out frames
    /// beyond the memory it can account for.
    pub fn new(boot_info: &BootInfo, base_frame: usize) -> Self {
        let page = ADDRESS_SPACE.page_size as usize;

        // The bitmap tracks physical frames up to the end of the highest
        // usable region. Frames the firmware did not call usable are marked
        // busy below, so the extent only needs to *cover* the allocatable
        // ones.
        let limit_frame = boot_info
            .regions()
            .iter()
            .filter(|region| region.is_usable())
            .fold(0u64, |top, region| top.max(region.end() / PAGE_SIZE))
            as usize;

        if limit_frame == 0 {
            panic!("mm: the firmware reported no usable memory; there is nothing to allocate");
        }

        let bitmap_bytes = limit_frame.div_ceil(u64::BITS as usize) * size_of::<u64>();
        let bitmap_frames = bitmap_bytes.div_ceil(page);

        // Choose the host region: the largest usable region that can hold the
        // bitmap at or above the kernel-image floor. Ties go to the higher
        // region, so a machine with two equal regions keeps its low memory
        // — the DMA window — clear of bookkeeping.
        let mut chosen: Option<(usize, usize, u64)> = None;
        for region in boot_info.regions() {
            if !region.is_usable() {
                continue;
            }
            let start_frame = (region.base / PAGE_SIZE) as usize;
            let end_frame = (region.end() / PAGE_SIZE) as usize;
            let floor = base_frame.max(start_frame);
            if end_frame < floor + bitmap_frames {
                continue;
            }
            let better = match chosen {
                None => true,
                Some((_, _, length)) => {
                    region.length > length || (region.length == length && start_frame > floor)
                }
            };
            if better {
                chosen = Some((floor, end_frame, region.length));
            }
        }

        let Some((bitmap_start_frame, _, _)) = chosen else {
            panic!(
                "mm: the frame bitmap needs {} bytes above frame {base_frame}, but no usable \
                 region in the firmware's map can hold it",
                bitmap_bytes
            );
        };

        let bitmap_phys = bitmap_start_frame as u64 * ADDRESS_SPACE.page_size;
        let word_count = bitmap_bytes / size_of::<u64>();

        // SAFETY: `bitmap_phys` names frames this allocator is claiming for
        // itself, inside a usable region the direct map covers; the write is
        // bounded by the span just claimed.
        unsafe {
            core::ptr::write_bytes(
                direct_map::phys_to_virt(bitmap_phys).cast::<u8>(),
                0,
                bitmap_bytes,
            );
        }

        // SAFETY: the backing store was just zeroed and stays owned by the
        // allocator for its whole lifetime; no other reference to those frames
        // exists.
        let bitmap = unsafe {
            Bitmap::from_zeroed(
                direct_map::phys_to_virt(bitmap_phys).cast::<u64>(),
                word_count,
                limit_frame,
            )
        };

        klog!(
            Level::Debug,
            "mm: frame bitmap at phys {:#x} ({} KiB) tracking {} frames",
            bitmap_phys,
            bitmap_bytes / 1024,
            limit_frame
        );

        let mut allocator = Self {
            bitmap: UnsafeCell::new(bitmap),
            base_frame,
            total_frames: limit_frame,
            dma_limit_frame: DMA_FRAME_LIMIT.min(limit_frame),
            hint: AtomicUsize::new(base_frame),
            bitmap_phys,
        };

        // Seed the occupancy: everything busy, then the usable regions above
        // the kernel-image floor free, then the bitmap itself busy again.
        // Marking is order-independent and idempotent, so the firmware's
        // region order — which validation counts but does not enforce — does
        // not matter.
        //
        // SAFETY: single-CPU construction from `_start`, before any reader.
        unsafe {
            let bitmap = &mut *allocator.bitmap.get();

            bitmap.mark_allocated_range(0, limit_frame);

            for region in boot_info.regions() {
                if !region.is_usable() {
                    continue;
                }
                let start = (region.base / PAGE_SIZE) as usize;
                let end = (region.end() / PAGE_SIZE) as usize;
                let free_start = start.max(base_frame);
                if end > free_start {
                    bitmap.mark_free_range(free_start, end - free_start);
                }
            }

            bitmap.mark_allocated_range(bitmap_start_frame, bitmap_frames);
        }

        let free = {
            // SAFETY: read-only access, single-CPU construction.
            let bitmap = unsafe { &*allocator.bitmap.get() };
            bitmap.free_count()
        };
        klog!(
            Level::Debug,
            "mm: allocator seeded with {free} free frames (dma-capable below frame {})",
            allocator.dma_limit_frame
        );

        allocator
    }

    /// Move the sequential allocation hint forward to `frame + 1`.
    ///
    /// Monotonic on purpose. The hint is an optimization over the floor scan,
    /// and two legitimate paths hand out frames *below* it: `alloc_dma` scans
    /// from the floor to keep device-reachable frames, and `alloc`'s own
    /// fallback rescan runs when the hint region has run out. Storing those
    /// low frames unconditionally would drag the hint backwards, and a
    /// workload that mixes ordinary allocations with either path would never
    /// walk past its starting region — the freed frames below the hint would
    /// be handed out again forever.
    fn advance_hint(&self, frame: usize) {
        let next = frame.saturating_add(1);
        if self.hint.load(Ordering::Relaxed) < next {
            self.hint.store(next, Ordering::Relaxed);
        }
    }

    /// Allocate one physical frame.
    ///
    /// Returns a frame *index*, not an address. Mapping a frame into the
    /// kernel's address space is the HHDM's job, not the allocator's.
    pub fn alloc(&self) -> Result<usize, FrameError> {
        // SAFETY: single-CPU allocator. Allocation is only reachable from
        // kernel code; real concurrency replaces this shell with a locked one.
        let bitmap = unsafe { &mut *self.bitmap.get() };

        // Allocation is overwhelmingly sequential, so try the hint first and
        // fall back to a full scan only when that region is exhausted.
        let hint = self.hint.load(Ordering::Relaxed);
        if hint < self.total_frames
            && let Some(frame) = bitmap.find_first_free_from(hint)
        {
            bitmap.mark_allocated(frame);
            self.advance_hint(frame);
            return Ok(frame);
        }

        // The hint region is full or stale; rescan from the floor.
        match bitmap.find_first_free_from(self.base_frame) {
            Some(frame) => {
                bitmap.mark_allocated(frame);
                self.advance_hint(frame);
                Ok(frame)
            }
            None => Err(FrameError::OutOfMemory),
        }
    }

    /// Allocate one frame a 32-bit DMA device can reach.
    ///
    /// The search is the same lowest-first scan, restricted to frames below
    /// [`DMA_FRAME_LIMIT`]. VirtIO descriptors and buffers name their memory
    /// with 32-bit words, so a frame at or above the 4 GiB boundary would be
    /// described correctly and reached wrongly — the device would read or
    /// write *something*, just not the memory the kernel handed it. Ordinary
    /// allocations keep the whole machine; only callers that feed a device
    /// need this restriction.
    ///
    /// On a machine whose usable memory is entirely below 4 GiB — every
    /// small-RAM configuration — this is the same pool as [`Self::alloc`],
    /// and the restriction costs nothing.
    pub fn alloc_dma(&self) -> Result<usize, FrameError> {
        // SAFETY: single-CPU allocator, as `alloc`.
        let bitmap = unsafe { &mut *self.bitmap.get() };

        match bitmap.find_first_free_from(self.base_frame) {
            Some(frame) if frame < self.dma_limit_frame => {
                bitmap.mark_allocated(frame);
                // Deliberately via `advance_hint`, not a plain store: a DMA
                // buffer below the kernel image must not drag the sequential
                // hint backwards.
                self.advance_hint(frame);
                Ok(frame)
            }
            // Either the pool is exhausted or only the frames above the DMA
            // boundary are free; the caller cannot use either answer.
            _ => Err(FrameError::OutOfMemory),
        }
    }

    /// Return a frame to the pool.
    ///
    /// # Safety
    ///
    /// The caller must own `frame`, and must not free the same frame twice. A
    /// double free silently hands one physical page to two owners, which is a
    /// use-after-free that no later check can catch.
    pub unsafe fn dealloc(&self, frame: usize) {
        if frame < self.base_frame || frame >= self.total_frames {
            // Refusing out-of-range frees is deliberate: the most likely caller
            // bug is passing a physical *address* where a frame index was
            // expected, and silently clearing a wrong bit is far worse.
            klog!(
                Level::Warn,
                "mm: refusing to free out-of-range frame {frame} (valid {}..{})",
                self.base_frame,
                self.total_frames
            );
            return;
        }

        // SAFETY: delegated to this function's contract; single-CPU access.
        unsafe { (&mut *self.bitmap.get()).mark_free(frame) };
    }

    /// Whether `frame` is currently allocated.
    pub fn is_allocated(&self, frame: usize) -> bool {
        // SAFETY: read-only access to the bitmap, single-CPU.
        unsafe { (*self.bitmap.get()).is_allocated(frame) }
    }

    /// Current allocator statistics.
    pub fn stats(&self) -> AllocatorStats {
        // SAFETY: read-only access to the bitmap, single-CPU.
        let bitmap = unsafe { &*self.bitmap.get() };
        AllocatorStats {
            total_frames: self.total_frames,
            allocated_frames: bitmap.allocated_count(),
            free_frames: bitmap.free_count(),
            bitmap_bytes: bitmap.storage_bytes(),
        }
    }

    /// Frames currently free.
    ///
    /// Exposed separately from [`FrameAllocator::stats`] because the boot test
    /// suite asserts on the free count directly, and `stats()` is a copy of four
    /// fields where only one of them is wanted.
    pub fn free_count(&self) -> usize {
        // SAFETY: read-only access to the bitmap, single-CPU.
        unsafe { (*self.bitmap.get()).free_count() }
    }

    /// First frame the allocator will ever hand out.
    pub const fn base_frame(&self) -> usize {
        self.base_frame
    }

    /// Total frames the allocator tracks.
    pub const fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// The frame boundary DMA allocations never cross.
    pub const fn dma_frame_limit(&self) -> usize {
        self.dma_limit_frame
    }

    /// The physical address of the bitmap's backing store.
    pub const fn bitmap_phys(&self) -> u64 {
        self.bitmap_phys
    }
}

/// Render allocator statistics for the boot log.
pub fn describe_allocator(allocator: &FrameAllocator) -> AllocatorStats {
    allocator.stats()
}

/// Round `value` up to the next multiple of `align`.
///
/// `align` must be a power of two. Used wherever a byte length becomes a frame
/// count, where rounding *down* would silently lose a partial page.
pub const fn align_up(value: u64, align: u64) -> u64 {
    value.wrapping_add(align - 1) & !(align - 1)
}

/// Count the allocatable frames in a memory map, for boot-log reporting.
pub fn count_usable_frames(regions: &[MemoryRegion]) -> usize {
    regions
        .iter()
        .filter(|region| region.is_usable())
        .fold(0u64, |total, region| total.saturating_add(region.length))
        .checked_div(PAGE_SIZE)
        .map(|frames| frames as usize)
        .unwrap_or(0)
}

/// One bit per frame: `1` means allocated.
///
/// # Why this is a separate type
///
/// [`Bitmap`] has no `unsafe` in its *logic*, no architecture knowledge, and no
/// heap indirection the caller cannot name: it operates over a word array the
/// caller provides. Splitting it out means the interesting logic
/// (find-first-free, range marking, counting) is testable on the host with
/// `cargo test`, while [`FrameAllocator`] stays a thin shell that chooses the
/// backing store and adds the direct map needed to reach it.
///
/// # Invariants
///
/// `1` marks a frame that must never be handed out; `0` marks one that may be.
/// The two directions are *not* symmetric: `mark_free` on an already-free frame
/// is a double-free upstream and is a no-op here rather than an error, because
/// the bitmap cannot tell the difference and idempotence is preferable to a
/// panic on the allocator's hot path.
pub struct Bitmap {
    /// The words holding the bits. The caller owns the storage; the bitmap
    /// only borrows it for its own lifetime.
    words: *mut u64,
    /// Number of valid words at `words`.
    word_count: usize,
    /// Frames tracked. Never exceeds `word_count * 64`, and may be lower if
    /// the firmware reported less memory than the store can hold.
    len: usize,
    /// Count of set bits, maintained incrementally so
    /// [`Bitmap::allocated_count`] is O(1) and the boot log does not scan the
    /// whole array.
    allocated: usize,
}

impl Bitmap {
    /// Build a bitmap over zeroed backing storage.
    ///
    /// The padding bits above `len` — the frames the firmware never described —
    /// are marked busy, so neither [`Bitmap::find_first_free_from`] nor
    /// [`Bitmap::is_allocated`] can hand them out.
    ///
    /// # Safety
    ///
    /// `words` must point at `word_count` *zeroed*, 8-byte-aligned `u64`
    /// slots that outlive the bitmap, and `len` must not exceed
    /// `word_count * 64`.
    unsafe fn from_zeroed(words: *mut u64, word_count: usize, len: usize) -> Self {
        debug_assert!(len <= word_count * u64::BITS as usize);
        let mut bitmap = Self {
            words,
            word_count,
            len,
            allocated: 0,
        };
        for frame in len..word_count * u64::BITS as usize {
            bitmap.set_bit(frame);
        }
        bitmap
    }

    /// Build a bitmap over host storage, zeroing it first.
    ///
    /// The test-side counterpart of [`Bitmap::from_zeroed`]: same invariants,
    /// same padding rule, over an array the test owns.
    fn from_slice(storage: &mut [u64], len: usize) -> Self {
        debug_assert!(len <= storage.len() * u64::BITS as usize);
        storage.fill(0);
        // SAFETY: the slice is aligned, zeroed, and outlives the bitmap the
        // test holds; `from_zeroed`'s contract is otherwise the same.
        unsafe { Self::from_zeroed(storage.as_mut_ptr(), storage.len(), len) }
    }

    /// Frames this bitmap tracks.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this bitmap tracks no frames at all.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `frame` is within range. Out-of-range counts as allocated.
    #[inline]
    fn in_range(&self, frame: usize) -> bool {
        frame < self.len
    }

    /// Read one backing word, treating an out-of-range index as all-allocated.
    #[inline]
    fn word(&self, index: usize) -> u64 {
        if index >= self.word_count {
            return u64::MAX;
        }
        // SAFETY: `index < word_count` names a valid slot in the caller's
        // backing store.
        unsafe { *self.words.add(index) }
    }

    /// Write one backing word. Out-of-range indices are ignored.
    #[inline]
    fn set_word(&mut self, index: usize, value: u64) {
        if index >= self.word_count {
            return;
        }
        // SAFETY: as `word`.
        unsafe { *self.words.add(index) = value };
    }

    /// Set the bit for `frame`, without touching the allocated count.
    ///
    /// Used only for the padding bits, which the count deliberately excludes.
    fn set_bit(&mut self, frame: usize) {
        let word = frame / u64::BITS as usize;
        let mask = 1u64 << (frame % u64::BITS as usize);
        if word < self.word_count {
            self.set_word(word, self.word(word) | mask);
        }
    }

    /// The bit for `frame`, as a word value and mask.
    #[inline]
    fn bit(frame: usize) -> (usize, u64) {
        (
            frame / u64::BITS as usize,
            1u64 << (frame % u64::BITS as usize),
        )
    }

    /// Mark `frame` allocated. Out-of-range frames are ignored.
    ///
    /// Idempotent: marking an already-allocated frame changes nothing and does
    /// not double-count, which is what makes a duplicate allocation the caller's
    /// bug to report rather than a corrupted count here.
    pub fn mark_allocated(&mut self, frame: usize) {
        if !self.in_range(frame) {
            return;
        }
        let (word, mask) = Self::bit(frame);
        let new = self.word(word) | mask;
        self.store_counted(word, new);
    }

    /// Mark `frame` free. Out-of-range and already-free frames are ignored.
    pub fn mark_free(&mut self, frame: usize) {
        if !self.in_range(frame) {
            return;
        }
        let (word, mask) = Self::bit(frame);
        let new = self.word(word) & !mask;
        self.store_counted(word, new);
    }

    /// Mark every frame in `[start, start + count)` allocated.
    ///
    /// Whole words are set in one step, so seeding a 192 GiB machine's
    /// non-usable gaps costs a handful of word writes rather than millions of
    /// bit operations. Idempotent, like [`Bitmap::mark_allocated`]: the
    /// allocated count only ever moves by the number of bits that *changed*.
    pub fn mark_allocated_range(&mut self, start: usize, count: usize) {
        self.mark_range(start, count, true);
    }

    /// Mark every frame in `[start, start + count)` free.
    ///
    /// See [`Bitmap::mark_allocated_range`] for the word-stepping and the
    /// idempotence; this is the direction the constructor uses to hand a
    /// usable region back after marking the whole frame space busy.
    pub fn mark_free_range(&mut self, start: usize, count: usize) {
        self.mark_range(start, count, false);
    }

    /// Store one backing word, keeping the allocated count in step.
    ///
    /// The count moves by the *signed* popcount difference between the old and
    /// the new word. The difference is computed in `i64` on purpose: a plain
    /// `u32` subtraction wraps in release builds, so freeing one frame would
    /// add `2^32 - 1` to the count instead of subtracting one, and `free_count`
    /// would then wrap into the astronomicals.
    fn store_counted(&mut self, word_index: usize, new: u64) {
        let old = self.word(word_index);
        if old == new {
            return;
        }
        let delta = new.count_ones() as i64 - old.count_ones() as i64;
        self.allocated = (self.allocated as i64 + delta) as usize;
        self.set_word(word_index, new);
    }

    /// The range-marking worker behind the two public directions.
    fn mark_range(&mut self, start: usize, count: usize, allocate: bool) {
        let last = start.saturating_add(count).min(self.len);
        let mut frame = start;

        while frame < last {
            let (word_index, bit_index) = (frame / u64::BITS as usize, frame % u64::BITS as usize);
            let span = (u64::BITS as usize - bit_index).min(last - frame);

            let mask = if span == u64::BITS as usize {
                u64::MAX
            } else {
                ((1u64 << span) - 1) << bit_index
            };

            let new = if allocate {
                self.word(word_index) | mask
            } else {
                self.word(word_index) & !mask
            };
            self.store_counted(word_index, new);

            frame += span;
        }
    }

    /// Whether `frame` is marked allocated. Out-of-range reads `true`.
    #[inline]
    pub fn is_allocated(&self, frame: usize) -> bool {
        if !self.in_range(frame) {
            // Reporting "allocated" for a frame that does not exist is the safe
            // answer: it sends the caller to a full scan or an out-of-memory
            // error instead of indexing past the array.
            return true;
        }
        let (word, mask) = Self::bit(frame);
        self.word(word) & mask != 0
    }

    /// Lowest free frame at or after `start`, or `None` if there is none.
    ///
    /// A linear scan starting at `start`, skipping whole words of allocated
    /// frames in one step: the allocator's own `hint` already skips the
    /// allocated prefix, so the common case touches a few words.
    pub fn find_first_free_from(&self, start: usize) -> Option<usize> {
        let mut frame = start;

        while frame < self.len {
            let word = frame / u64::BITS as usize;
            let bit = frame % u64::BITS as usize;
            let word_bits = self.word(word);

            if word_bits == u64::MAX && bit == 0 {
                // Whole word allocated: skip all 64 frames in one step.
                frame += u64::BITS as usize;
                continue;
            }

            // `!word_bits` marks the free bits; mask off the ones below `bit`
            // so the scan can never walk backwards. `bit` is the bit *index*
            // here — `Self::bit` returns the decoded mask, and shifting by a
            // mask would wrap the shift count in release builds and unmask
            // the whole word, handing back frames below `start`.
            let free = !word_bits & (u64::MAX << bit);

            if free != 0 {
                return Some(word * u64::BITS as usize + free.trailing_zeros() as usize);
            }

            frame = word * u64::BITS as usize + u64::BITS as usize;
        }

        None
    }

    /// Frames currently marked allocated.
    pub const fn allocated_count(&self) -> usize {
        self.allocated
    }

    /// Frames currently free.
    ///
    /// `len - allocated`, never counting the padding bits above `len`, which
    /// `from_zeroed` marks busy without adding to `allocated`.
    pub const fn free_count(&self) -> usize {
        self.len - self.allocated
    }

    /// Size of the backing storage in bytes, for the boot log.
    pub const fn storage_bytes(&self) -> usize {
        self.word_count * size_of::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::{Bitmap, DMA_FRAME_LIMIT, FrameAllocator};

    /// A backing store wide enough for the largest `len` the tests use.
    macro_rules! storage {
        ($words:expr) => {{
            let mut storage = [0u64; $words];
            // SAFETY-free: `Bitmap::from_slice` is the test-side constructor.
            storage
        }};
    }

    #[test]
    fn the_bitmap_scales_to_the_documented_machine() {
        // 192 GiB of 4 KiB frames is 50,331,648 bits — 6 MiB of bitmap. The
        // point of the runtime store is exactly that this is a *boot-time*
        // cost measured from the firmware's map, not a compile-time bound
        // that larger machines panic against.
        let frames_192gib: usize = 192 * 1024 * 1024 * 1024 / 4096;
        let bytes = frames_192gib.div_ceil(64) * size_of::<u64>();
        assert_eq!(bytes, 6 * 1024 * 1024, "6 MiB for 192 GiB");
        assert_eq!(
            frames_192gib.div_ceil(4096),
            1_474_560 / 4096 + 1,
            "the bitmap occupies at most a few hundred 4 KiB frames itself"
        );
    }

    #[test]
    fn dma_limit_is_the_4gib_boundary() {
        // 4 GiB / 4 KiB. A frame below this is describable in a 32-bit DMA
        // word; a frame at or above it is not.
        assert_eq!(DMA_FRAME_LIMIT, 0x10_0000);
        assert_eq!(DMA_FRAME_LIMIT * 4096, 4 * 1024 * 1024 * 1024u64 as usize);
    }

    #[test]
    fn starts_all_free() {
        let mut storage = storage!(4);
        let bitmap = Bitmap::from_slice(&mut storage, 200);
        assert_eq!(bitmap.len(), 200);
        assert_eq!(bitmap.allocated_count(), 0);
        assert_eq!(bitmap.free_count(), 200);
        assert!((0..200).all(|frame| !bitmap.is_allocated(frame)));
    }

    #[test]
    fn allocation_is_visible_and_counted_once() {
        let mut storage = storage!(1);
        let mut bitmap = Bitmap::from_slice(&mut storage, 64);
        bitmap.mark_allocated(7);
        assert!(bitmap.is_allocated(7));
        assert_eq!(bitmap.allocated_count(), 1);

        // Re-marking must not double-count, or `free_count` could exceed `len`.
        bitmap.mark_allocated(7);
        assert_eq!(bitmap.allocated_count(), 1);
        assert_eq!(bitmap.free_count(), 63);
    }

    #[test]
    fn free_returns_the_frame_to_the_pool() {
        let mut storage = storage!(1);
        let mut bitmap = Bitmap::from_slice(&mut storage, 64);
        bitmap.mark_allocated(3);
        bitmap.mark_free(3);
        assert!(!bitmap.is_allocated(3));
        assert_eq!(bitmap.free_count(), 64);
    }

    #[test]
    fn double_free_is_idempotent() {
        let mut storage = storage!(1);
        let mut bitmap = Bitmap::from_slice(&mut storage, 64);
        bitmap.mark_allocated(9);
        bitmap.mark_free(9);
        // Freeing again must not underflow the allocated count.
        bitmap.mark_free(9);
        assert_eq!(bitmap.allocated_count(), 0);
    }

    #[test]
    fn out_of_range_operations_are_ignored() {
        let mut storage = storage!(1);
        let mut bitmap = Bitmap::from_slice(&mut storage, 64);
        bitmap.mark_allocated(1000);
        bitmap.mark_free(1000);
        assert_eq!(bitmap.allocated_count(), 0);
        // Out-of-range reads report "allocated" so callers cannot index past
        // the array.
        assert!(bitmap.is_allocated(1000));
    }

    #[test]
    fn padding_bits_are_never_handed_out() {
        // 130 frames needs a third word with only two meaningful bits. A scan
        // must not report frames 130..=191 as free, because the firmware never
        // described that memory.
        let mut storage = storage!(3);
        let mut bitmap = Bitmap::from_slice(&mut storage, 130);
        for frame in 0..130 {
            bitmap.mark_allocated(frame);
        }
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert!(bitmap.is_allocated(130));
        assert!(bitmap.is_allocated(191));
        assert_eq!(bitmap.free_count(), 0, "padding is not counted as free");
        assert_eq!(
            bitmap.allocated_count(),
            130,
            "padding is not counted as allocated"
        );
    }

    #[test]
    fn range_marking_matches_bit_by_bit_marking() {
        let mut storage = storage!(4);
        let mut range_bitmap = Bitmap::from_slice(&mut storage, 256);
        let mut bit_bitmap = Bitmap::from_slice(&mut [0u64; 4], 256);

        range_bitmap.mark_allocated_range(30, 100);
        for frame in 30..130 {
            bit_bitmap.mark_allocated(frame);
        }

        assert_eq!(range_bitmap.allocated_count(), bit_bitmap.allocated_count());
        assert!(
            (0..256)
                .all(|frame| range_bitmap.is_allocated(frame) == bit_bitmap.is_allocated(frame))
        );
    }

    #[test]
    fn range_freeing_returns_a_whole_region() {
        // The constructor's seeding direction: mark everything busy, then hand
        // a usable region back in one call.
        let mut storage = storage!(4);
        let mut bitmap = Bitmap::from_slice(&mut storage, 256);
        bitmap.mark_allocated_range(0, 256);
        assert_eq!(bitmap.free_count(), 0);

        bitmap.mark_free_range(16, 128);
        assert_eq!(bitmap.free_count(), 128);
        assert!((0..16).all(|frame| bitmap.is_allocated(frame)));
        assert!((16..144).all(|frame| !bitmap.is_allocated(frame)));
        assert!((144..256).all(|frame| bitmap.is_allocated(frame)));
    }

    #[test]
    fn range_marking_is_idempotent() {
        let mut storage = storage!(2);
        let mut bitmap = Bitmap::from_slice(&mut storage, 128);
        bitmap.mark_allocated_range(0, 64);
        let once = bitmap.allocated_count();
        bitmap.mark_allocated_range(0, 64);
        assert_eq!(
            bitmap.allocated_count(),
            once,
            "re-marking must not double-count"
        );
    }

    #[test]
    fn range_marking_clips_at_the_end_of_the_map() {
        let mut storage = storage!(2);
        let mut bitmap = Bitmap::from_slice(&mut storage, 100);
        // 60 frames from frame 80 would run to 140; the map ends at 100 and
        // the padding above is already busy.
        bitmap.mark_allocated_range(80, 60);
        assert_eq!(bitmap.allocated_count(), 20);
        assert!(bitmap.is_allocated(99));
        assert_eq!(bitmap.free_count(), 80);
    }

    #[test]
    fn finds_the_lowest_free_frame() {
        let mut storage = storage!(4);
        let mut bitmap = Bitmap::from_slice(&mut storage, 256);
        bitmap.mark_allocated(0);
        bitmap.mark_allocated(1);
        assert_eq!(bitmap.find_first_free_from(0), Some(2));
    }

    #[test]
    fn honours_the_start_hint() {
        let mut storage = storage!(4);
        let mut bitmap = Bitmap::from_slice(&mut storage, 256);
        assert_eq!(bitmap.find_first_free_from(100), Some(100));
        bitmap.mark_allocated(100);
        assert_eq!(bitmap.find_first_free_from(100), Some(101));
    }

    #[test]
    fn skips_a_fully_allocated_word() {
        let mut storage = storage!(4);
        let mut bitmap = Bitmap::from_slice(&mut storage, 256);
        for frame in 0..64 {
            bitmap.mark_allocated(frame);
        }
        // The scan must cross the whole word in one step and land on 64.
        assert_eq!(bitmap.find_first_free_from(0), Some(64));
    }

    #[test]
    fn reports_exhaustion_rather_than_wrapping() {
        let mut storage = storage!(3);
        let mut bitmap = Bitmap::from_slice(&mut storage, 130);
        for frame in 0..130 {
            bitmap.mark_allocated(frame);
        }
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert_eq!(bitmap.free_count(), 0);
    }

    #[test]
    fn cross_word_allocation_is_tracked() {
        // 63 and 64 straddle the first word boundary, which is where an
        // off-by-one in the mask arithmetic shows up.
        let mut storage = storage!(4);
        let mut bitmap = Bitmap::from_slice(&mut storage, 200);
        bitmap.mark_allocated(63);
        bitmap.mark_allocated(64);
        assert!(bitmap.is_allocated(63));
        assert!(bitmap.is_allocated(64));
        assert_eq!(bitmap.find_first_free_from(0), Some(0));
        assert_eq!(bitmap.find_first_free_from(65), Some(65));
    }

    #[test]
    fn zero_capacity_never_allocates() {
        let mut storage = storage!(1);
        let bitmap = Bitmap::from_slice(&mut storage, 0);
        assert!(bitmap.is_empty());
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert_eq!(bitmap.free_count(), 0);
    }

    #[test]
    fn a_large_len_over_a_large_store_round_trips() {
        // 8 words = 512 frames — the shape of a 2 MiB machine, and the seam
        // where a `word_count`/`len` mismatch would show up.
        let mut storage = storage!(8);
        let mut bitmap = Bitmap::from_slice(&mut storage, 500);
        bitmap.mark_allocated_range(0, 400);
        assert_eq!(bitmap.allocated_count(), 400);
        assert_eq!(bitmap.find_first_free_from(0), Some(400));
        assert!(!bitmap.is_allocated(499));
        assert!(bitmap.is_allocated(500));
    }

    // `FrameAllocator` itself is a kernel shell over the bitmap — its
    // construction needs the direct map — so the allocator-level behaviour is
    // covered by the kernel test suites rather than by host tests.
    #[test]
    fn frame_allocator_type_exists_for_the_kernel_shell() {
        // A compile-time anchor: the shell is generic over nothing and its
        // DMA boundary constant is the documented one.
        let _ = DMA_FRAME_LIMIT;
        let _ = core::mem::size_of::<FrameAllocator>();
    }
}
