//! Bitmap physical page frame allocator.
//!
//! # Why a bitmap
//!
//! The allocator needs to answer two questions — "is this frame free?" and
//! "give me the lowest free frame" — over an address space whose usable extent
//! is only known at boot. A bitmap of one bit per 4 KiB frame handles 256 MiB
//! with 64 KiB of kernel memory, which is a good trade before M2 introduces real
//! virtual memory and the bookkeeping that comes with it.
//!
//! # Testability
//!
//! [`Bitmap`] is deliberately free of architecture and of `unsafe`, and its unit
//! tests run on the host under `cargo test`. The kernel-side
//! [`FrameAllocator`] is a thin shell over it that adds the HHDM mapping needed
//! to touch a frame's contents.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::ADDRESS_SPACE;
use crate::boot::info::{BootInfo, MemoryRegion, PAGE_SIZE};
use crate::klog;
use crate::logger::Level;

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

/// Physical frame allocator.
///
/// # Concurrency
///
/// The bitmap sits behind an [`UnsafeCell`] and every accessor is `unsafe`. That
/// is deliberate: NovaOS v0.1.0 runs on one CPU with interrupts masked during
/// allocation, so there is nothing to race against. M2 introduces real
/// concurrency and replaces this type wholesale — the safe [`Bitmap`] logic it
/// wraps is exactly what survives that refactor, which is why the split exists.
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
    /// Number of frames the bitmap tracks.
    total_frames: usize,
    /// Sequential allocation hint; cleared when it runs off the end.
    hint: AtomicUsize,
}

impl FrameAllocator {
    /// Build an allocator over the usable regions described by `boot_info`.
    ///
    /// `base_frame` is the first frame safe to hand out. The caller passes the
    /// end of the kernel image so the allocator never recycles the pages it is
    /// executing from.
    ///
    /// # Panics
    ///
    /// If the machine reports more allocatable memory than
    /// [`MAX_FRAMES`] can track. A bitmap that silently truncated would hand out
    /// frames beyond the memory it can account for, so this fails loudly at boot
    /// instead: raise `MAX_BITMAP_WORDS` and re-measure the `.bss` cost first.
    pub fn new(boot_info: &BootInfo, base_frame: usize) -> Self {
        // Count only frames that lie inside a usable region, so the bitmap can
        // never report free a frame the firmware said was not RAM.
        //
        // The page size comes from the address space rather than a constant, so
        // this stays correct on a target with a 16 KiB or 64 KiB granule.
        let page = ADDRESS_SPACE.page_size;
        let usable = boot_info
            .regions()
            .iter()
            .filter(|region| region.is_usable() && region.length > 0)
            .fold(0u64, |total, region| total.saturating_add(region.length));
        let total_frames = (usable / page) as usize;

        let mut bitmap = Bitmap::with_capacity(total_frames).unwrap_or_else(|| {
            panic!(
                "mm: machine reports {} usable frames, but the bitmap pool tracks at most {} \
                 ({:.0} MiB). Raise MAX_BITMAP_WORDS in kernel/src/mm/frame.rs.",
                total_frames,
                MAX_FRAMES,
                (MAX_FRAMES as u64 * page) / (1024 * 1024)
            )
        });

        // Reserve the floor. Doing it here rather than from the caller means no
        // window exists in which `alloc` could observe an unreserved frame.
        let floor = base_frame.min(total_frames);
        for frame in 0..floor {
            bitmap.mark_allocated(frame);
        }

        Self {
            bitmap: UnsafeCell::new(bitmap),
            base_frame,
            total_frames,
            hint: AtomicUsize::new(floor),
        }
    }

    /// Allocate one physical frame.
    ///
    /// Returns a frame *index*, not an address. Mapping a frame into the
    /// kernel's address space is the HHDM's job, not the allocator's.
    pub fn alloc(&self) -> Result<usize, FrameError> {
        // SAFETY: single-CPU allocator. Allocation is only reachable from
        // kernel code, and M2 replaces this type when preemption makes it
        // genuinely concurrent.
        let bitmap = unsafe { &mut *self.bitmap.get() };

        // Allocation is overwhelmingly sequential, so try the hint first and
        // fall back to a full scan only when that region is exhausted.
        let hint = self.hint.load(Ordering::Relaxed);
        if hint < self.total_frames
            && let Some(frame) = bitmap.find_first_free_from(hint)
        {
            bitmap.mark_allocated(frame);
            self.hint.store(frame + 1, Ordering::Relaxed);
            return Ok(frame);
        }

        // The hint region is full or stale; rescan from the floor.
        match bitmap.find_first_free_from(self.base_frame) {
            Some(frame) => {
                bitmap.mark_allocated(frame);
                self.hint.store(frame + 1, Ordering::Relaxed);
                Ok(frame)
            }
            None => Err(FrameError::OutOfMemory),
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

/// Size of the frame allocator's bitmap backing store, in 64-bit words.
///
/// A *static* bound, because the kernel has no heap until M2: the report's rule
/// is "no `Vec`/`Box` before an allocator exists; use a statically bounded pool
/// until then". 1024 words is 8 KiB of `.bss` and tracks 65536 frames, i.e. a
/// 256 MiB machine — comfortably above what v0.1.0 asks QEMU for, and chosen so
/// that a larger `-m` fails loudly at boot instead of silently truncating the
/// pool and handing out frames that do not exist.
pub const MAX_BITMAP_WORDS: usize = 1024;

/// Frames [`Bitmap`] can therefore track.
pub const MAX_FRAMES: usize = MAX_BITMAP_WORDS * u64::BITS as usize;

/// One bit per frame: `1` means allocated.
///
/// # Why this is a separate type
///
/// [`Bitmap`] has no `unsafe`, no architecture knowledge, no heap, and no
/// indirection — it is a bit array in a fixed buffer. Splitting it out means the
/// interesting logic (find-first-free, bounds, counting) is testable on the host
/// with `cargo test`, while [`FrameAllocator`] stays a thin shell that adds the
/// direct map needed to touch a frame's contents. M2 replaces the shell without
/// touching this.
///
/// It is generic over the word count so tests can exercise a two-word bitmap
/// cheaply while the kernel instantiates the full pool.
///
/// # Invariants
///
/// `1` marks a frame that must never be handed out; `0` marks one that may be.
/// The two directions are *not* symmetric: `mark_free` on an already-free frame
/// is a double-free upstream and is a no-op here rather than an error, because
/// the bitmap cannot tell the difference and idempotence is preferable to a
/// panic on the allocator's hot path.
#[derive(Clone)]
pub struct Bitmap<const WORDS: usize = MAX_BITMAP_WORDS> {
    bits: [u64; WORDS],
    /// Frames tracked. Never exceeds `WORDS * 64`, and may be lower if the
    /// firmware reported less memory than the pool can hold.
    len: usize,
    /// Count of set bits, maintained incrementally so [`Bitmap::allocated_count`]
    /// is O(1) and the boot log does not scan the whole array.
    allocated: usize,
}

impl<const WORDS: usize> Default for Bitmap<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> Bitmap<WORDS> {
    /// A bitmap tracking the full capacity of this instantiation, all free.
    pub const fn new() -> Self {
        Self {
            bits: [0; WORDS],
            len: WORDS * 64,
            allocated: 0,
        }
    }

    /// A bitmap tracking `len` frames, all free.
    ///
    /// Returns `None` if `len` exceeds what `WORDS` words can hold, rather than
    /// silently clamping: a machine with more RAM than the pool was sized for
    /// must fail at boot with a diagnostic, not hand out frames outside its own
    /// bookkeeping.
    pub fn with_capacity(len: usize) -> Option<Self> {
        let words = len.div_ceil(u64::BITS as usize);
        if words > WORDS {
            return None;
        }

        let mut bitmap = Self {
            bits: [0; WORDS],
            len,
            allocated: 0,
        };

        // Bits above `len` in the final word describe frames the firmware never
        // reported. Marking them allocated makes them fail `find_first_free_from`
        // as well as `is_allocated`, so neither path can hand them out.
        for frame in len..words * 64 {
            bitmap.bits[frame / 64] |= 1u64 << (frame % 64);
        }

        Some(bitmap)
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

    /// Mark `frame` allocated. Out-of-range frames are ignored.
    ///
    /// Idempotent: marking an already-allocated frame changes nothing and does
    /// not double-count, which is what makes a duplicate allocation the caller's
    /// bug to report rather than a corrupted count here.
    pub fn mark_allocated(&mut self, frame: usize) {
        if !self.in_range(frame) {
            return;
        }
        let word = frame / 64;
        let mask = 1u64 << (frame % 64);
        if self.bits[word] & mask == 0 {
            self.bits[word] |= mask;
            self.allocated += 1;
        }
    }

    /// Mark `frame` free. Out-of-range and already-free frames are ignored.
    pub fn mark_free(&mut self, frame: usize) {
        if !self.in_range(frame) {
            return;
        }
        let word = frame / 64;
        let mask = 1u64 << (frame % 64);
        if self.bits[word] & mask != 0 {
            self.bits[word] &= !mask;
            self.allocated -= 1;
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
        self.bits[frame / 64] & (1u64 << (frame % 64)) != 0
    }

    /// Lowest free frame at or after `start`, or `None` if there is none.
    ///
    /// A linear scan starting at `start`, skipping whole words of allocated
    /// frames in one step: the allocator's own `hint` already skips the
    /// allocated prefix, so the common case touches a few words.
    pub fn find_first_free_from(&self, start: usize) -> Option<usize> {
        let mut frame = start;

        while frame < self.len {
            let word = frame / 64;
            let bit = frame % 64;
            let word_bits = self.bits[word];

            if word_bits == u64::MAX && bit == 0 {
                // Whole word allocated: skip all 64 frames in one step.
                frame += 64;
                continue;
            }

            // `!word_bits` marks the free bits; mask off the ones below `bit` so
            // the scan can never walk backwards.
            let free = !word_bits & (u64::MAX << bit);

            if free != 0 {
                return Some(word * 64 + free.trailing_zeros() as usize);
            }

            frame = word * 64 + 64;
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
    /// `with_capacity` marks busy without adding to `allocated`.
    pub const fn free_count(&self) -> usize {
        self.len - self.allocated
    }

    /// Size of the backing storage in bytes, for the boot log.
    pub const fn storage_bytes(&self) -> usize {
        WORDS * size_of::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::{Bitmap, MAX_FRAMES, MAX_BITMAP_WORDS};

    #[test]
    fn capacity_matches_the_documented_bound() {
        // The boot-time guard compares the reported frame count against this, so
        // a change to one without the other would silently shrink the pool.
        assert_eq!(MAX_FRAMES, MAX_BITMAP_WORDS * 64);
        assert_eq!(MAX_FRAMES, 65_536, "256 MiB of 4 KiB frames");
    }

    #[test]
    fn new_tracks_the_full_capacity() {
        let bitmap = Bitmap::<2>::new();
        assert_eq!(bitmap.len(), 128);
        assert_eq!(bitmap.allocated_count(), 0);
        assert_eq!(bitmap.free_count(), 128);
    }

    #[test]
    fn starts_all_free() {
        let bitmap = Bitmap::<4>::with_capacity(200).expect("200 frames fits in 4 words");
        assert_eq!(bitmap.len(), 200);
        assert_eq!(bitmap.allocated_count(), 0);
        assert_eq!(bitmap.free_count(), 200);
        assert!((0..200).all(|frame| !bitmap.is_allocated(frame)));
    }

    #[test]
    fn allocation_is_visible_and_counted_once() {
        let mut bitmap = Bitmap::<1>::with_capacity(64).expect("64 frames fits in 1 word");
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
        let mut bitmap = Bitmap::<1>::with_capacity(64).expect("64 frames fit in 1 word");
        bitmap.mark_allocated(3);
        bitmap.mark_free(3);
        assert!(!bitmap.is_allocated(3));
        assert_eq!(bitmap.free_count(), 64);
    }

    #[test]
    fn double_free_is_idempotent() {
        let mut bitmap = Bitmap::<1>::with_capacity(64).expect("64 frames fit in 1 word");
        bitmap.mark_allocated(9);
        bitmap.mark_free(9);
        // Freeing again must not underflow the allocated count.
        bitmap.mark_free(9);
        assert_eq!(bitmap.allocated_count(), 0);
    }

    #[test]
    fn out_of_range_operations_are_ignored() {
        let mut bitmap = Bitmap::<1>::with_capacity(64).expect("64 frames fit in 1 word");
        bitmap.mark_allocated(1000);
        bitmap.mark_free(1000);
        assert_eq!(bitmap.allocated_count(), 0);
        // Out-of-range reads report "allocated" so callers cannot index past
        // the array.
        assert!(bitmap.is_allocated(1000));
    }

    #[test]
    fn capacity_beyond_the_pool_is_rejected() {
        // 8 words hold 512 frames; asking for more must fail rather than clamp,
        // because a clamp would hand out frames the pool cannot account for.
        assert!(Bitmap::<8>::with_capacity(513).is_none());
        assert!(Bitmap::<8>::with_capacity(512).is_some());
    }

    #[test]
    fn finds_the_lowest_free_frame() {
        let mut bitmap = Bitmap::<4>::with_capacity(256).expect("256 frames fits in 4 words");
        bitmap.mark_allocated(0);
        bitmap.mark_allocated(1);
        assert_eq!(bitmap.find_first_free_from(0), Some(2));
    }

    #[test]
    fn honours_the_start_hint() {
        let mut bitmap = Bitmap::<4>::with_capacity(256).expect("256 frames fits in 4 words");
        assert_eq!(bitmap.find_first_free_from(100), Some(100));
        bitmap.mark_allocated(100);
        assert_eq!(bitmap.find_first_free_from(100), Some(101));
    }

    #[test]
    fn skips_a_fully_allocated_word() {
        let mut bitmap = Bitmap::<4>::with_capacity(256).expect("256 frames fits in 4 words");
        for frame in 0..64 {
            bitmap.mark_allocated(frame);
        }
        // The scan must cross the whole word in one step and land on 64.
        assert_eq!(bitmap.find_first_free_from(0), Some(64));
    }

    #[test]
    fn reports_exhaustion_rather_than_wrapping() {
        let mut bitmap = Bitmap::<3>::with_capacity(130).expect("130 frames fits in 3 words");
        for frame in 0..130 {
            bitmap.mark_allocated(frame);
        }
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert_eq!(bitmap.free_count(), 0);
    }

    #[test]
    fn unused_tail_bits_are_never_handed_out() {
        // 130 frames needs a third word with only two meaningful bits. A scan
        // must not report frames 130..=191 as free, because the firmware never
        // described that memory.
        let mut bitmap = Bitmap::<3>::with_capacity(130).expect("130 frames fits in 3 words");
        for frame in 0..130 {
            bitmap.mark_allocated(frame);
        }
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert!(bitmap.is_allocated(130));
        assert!(bitmap.is_allocated(191));
        assert_eq!(bitmap.free_count(), 0, "padding is not counted as free");
    }

    #[test]
    fn cross_word_allocation_is_tracked() {
        // 63 and 64 straddle the first word boundary, which is where an
        // off-by-one in the mask arithmetic shows up.
        let mut bitmap = Bitmap::<4>::with_capacity(200).expect("200 frames fits in 4 words");
        bitmap.mark_allocated(63);
        bitmap.mark_allocated(64);
        assert!(bitmap.is_allocated(63));
        assert!(bitmap.is_allocated(64));
        assert_eq!(bitmap.find_first_free_from(0), Some(0));
        assert_eq!(bitmap.find_first_free_from(65), Some(65));
    }

    #[test]
    fn zero_capacity_never_allocates() {
        let bitmap = Bitmap::<1>::with_capacity(0).expect("0 frames fits in any pool");
        assert!(bitmap.is_empty());
        assert_eq!(bitmap.find_first_free_from(0), None);
        assert_eq!(bitmap.free_count(), 0);
    }
}
