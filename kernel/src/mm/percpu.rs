//! Per-CPU frame allocation cache.
//!
//! # Why this exists
//!
//! [`FrameAllocator`] serialises every allocation behind one global
//! [`SpinLock<Bitmap>`](crate::mm::frame::Bitmap). On a single-CPU boot that is
//! fine, but once user threads run on application processors the lock becomes
//! a hot spot: every page table page, every stack, every surface buffer takes
//! the same lock.
//!
//! This module adds a per-CPU LIFO cache of recently freed frames above the
//! global allocator. The fast path — allocate from or release to the local
//! cache — never takes the global lock. Only when a CPU's cache is empty does
//! it refill in a batch from the global allocator, and only when it is full
//! does it spill one frame back. Because [`crate::arch::current_cpu`] indexes
//! a statically allocated array and each CPU reads and writes only its own
//! slot, two processors never alias the same memory and no lock is needed on
//! the fast path.
//!
//! # Layering
//!
//! The global [`FrameAllocator`] stays the single owner of physical memory and
//! the source of truth for the bitmap. This module is a cache *above* it:
//! refills and spills go through `alloc`/`dealloc`, so the global allocator's
//! accounting (free count, hint, DMA limit) is always consistent.
//!
//! `alloc_frame_fast` / `dealloc_frame_fast` are the fast-path entry points.
//! The existing `FrameAllocator::alloc` / `FrameAllocator::dealloc` remain the
//! slow path and are unchanged; callers that need bulk or contiguous frames
//! (memory objects, DMA buffers) keep using them directly.

use crate::arch::current_cpu;
use crate::boot::info::MAX_CPUS;
use crate::mm::frame::FrameError;

/// How many frames one CPU's local cache holds at most.
///
/// Small enough that the cache never hoards memory a sibling CPU could use,
/// large enough that a burst of allocations (a page table walk, a surface
/// buffer) hits the cache most of the time.
const LOCAL_CACHE_SIZE: usize = 16;

/// How many frames are taken from or returned to the global allocator in one
/// locked batch when the local cache empties or fills.
const REFILL_BATCH: usize = 8;

/// One CPU's private frame cache: a LIFO stack of frame indices.
///
/// Cache-line aligned so two CPUs' slots never share a line: every slot is
/// written by its own CPU and read by nobody else, and a shared line would
/// make the writes ping-pong across the interconnect.
#[repr(C, align(64))]
pub struct PerCpuFrameCache {
    /// The cached frames, indexed `0..len`; `frames[len]` is the top of the
    /// stack.
    frames: [usize; LOCAL_CACHE_SIZE],
    /// Number of valid entries in `frames`.
    len: usize,
}

impl PerCpuFrameCache {
    const fn new() -> Self {
        Self {
            frames: [0; LOCAL_CACHE_SIZE],
            len: 0,
        }
    }

    /// Pop one frame, or `None` if the cache is empty.
    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.frames[self.len])
    }

    /// Push one frame, returning `true` if it was accepted.
    ///
    /// Returns `false` when the cache is full; the caller must spill one frame
    /// to the global allocator first.
    fn push(&mut self, frame: usize) -> bool {
        if self.len >= LOCAL_CACHE_SIZE {
            return false;
        }
        self.frames[self.len] = frame;
        self.len += 1;
        true
    }
}

/// One cache per kernel CPU slot, indexed by [`crate::arch::current_cpu`].
///
/// Zero-initialised at boot; every slot starts empty and is filled by refills
/// from the global allocator.
static mut PER_CPU_FRAME_CACHES: [PerCpuFrameCache; MAX_CPUS] =
    [const { PerCpuFrameCache::new() }; MAX_CPUS];

/// The local cache for this CPU.
///
/// # Safety
///
/// Only this CPU may read or write its own slot; no other CPU aliases this
/// slice element, so there is no data race.
fn local_cache() -> &'static mut PerCpuFrameCache {
    let cpu = current_cpu();
    debug_assert!(cpu < MAX_CPUS);
    // SAFETY: `cpu < MAX_CPUS`, and each CPU accesses only its own index.
    unsafe { &mut PER_CPU_FRAME_CACHES[cpu] }
}

/// Allocate one physical frame from this CPU's local cache.
///
/// The fast path pops from the local cache without taking any lock. When the
/// cache is empty, up to [`REFILL_BATCH`] frames are taken from the global
/// allocator in one locked batch, and the top one is returned.
///
/// Returns [`FrameError::OutOfMemory`] when the global allocator has no frames
/// left.
///
/// # Example
///
/// ```ignore
/// let frame = mm::percpu::alloc_frame_fast()?;
/// // use the frame ...
/// mm::percpu::dealloc_frame_fast(frame);
/// ```
pub fn alloc_frame_fast() -> Result<usize, FrameError> {
    let cache = local_cache();
    if let Some(frame) = cache.pop() {
        return Ok(frame);
    }

    // The cache is empty: refill from the global allocator.
    // SAFETY: `mm::init` has already run before any thread exists; the
    // allocator is published and read-only after that point.
    let allocator = unsafe { crate::mm::frame_allocator() };
    let mut refilled = 0usize;
    while refilled < REFILL_BATCH {
        match allocator.alloc() {
            Ok(frame) => {
                if !cache.push(frame) {
                    // Should not happen: the cache was empty, so there is
                    // room for at least `REFILL_BATCH` entries.
                    break;
                }
                refilled += 1;
            }
            Err(FrameError::OutOfMemory) => break,
        }
    }

    if refilled == 0 {
        return Err(FrameError::OutOfMemory);
    }

    debug_assert!(cache.len > 0);
    cache.pop().ok_or(FrameError::OutOfMemory)
}

/// Return one physical frame to this CPU's local cache.
///
/// The fast path pushes onto the local cache without taking any lock. When the
/// cache is full, the oldest cached frame is spilled to the global allocator
/// first, then the freed frame is cached.
///
/// # Safety
///
/// The caller must own `frame` and must not free the same frame twice. A
/// double free hands one physical page to two owners — a use-after-free no
/// later check can catch.
#[allow(dead_code)]
pub unsafe fn dealloc_frame_fast(frame: usize) {
    let cache = local_cache();
    if cache.push(frame) {
        return;
    }

    // The cache is full: spill the oldest frame to the global allocator, then
    // cache the freed one. `frames[0]` is the oldest because pushes append at
    // `len` and pops take from `len - 1`.
    let spilled = cache.frames[0];
    for i in 1..cache.len {
        cache.frames[i - 1] = cache.frames[i];
    }
    cache.len -= 1;
    cache.frames[cache.len] = frame;
    cache.len += 1;

    // SAFETY: `spilled` was previously handed out by this allocator and has
    // not been freed since; the caller's `frame` is likewise a live owner.
    let allocator = unsafe { crate::mm::frame_allocator() };
    // SAFETY: `spilled` was handed out by this allocator and not yet freed.
    unsafe { allocator.dealloc(spilled) };
}

/// Flush every CPU's local cache back to the global allocator.
///
/// Used when a CPU is being taken offline and by the test suite to force a
/// clean state between cases.
#[allow(dead_code)]
pub fn flush_all_caches() {
    // SAFETY: `mm::init` has already run before any thread exists.
    let allocator = unsafe { crate::mm::frame_allocator() };
    // SAFETY: flush runs before any thread is scheduled on any CPU, so no
    // concurrent access to any cache slot exists.
    let caches = unsafe { &mut *core::ptr::addr_of_mut!(PER_CPU_FRAME_CACHES) };
    for cache in caches.iter_mut() {
        while cache.len > 0 {
            cache.len -= 1;
            // SAFETY: every cached frame was handed out by this allocator and
            // has not been freed since.
            unsafe { allocator.dealloc(cache.frames[cache.len]) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_from_empty_cache_returns_none() {
        // `PerCpuFrameCache::pop` on a fresh cache yields None.
        let mut cache = PerCpuFrameCache::new();
        assert_eq!(cache.pop(), None);
    }

    #[test]
    fn push_pop_is_lifo() {
        let mut cache = PerCpuFrameCache::new();
        assert!(cache.push(10));
        assert!(cache.push(20));
        assert_eq!(cache.pop(), Some(20));
        assert_eq!(cache.pop(), Some(10));
        assert_eq!(cache.pop(), None);
    }

    #[test]
    fn push_when_full_is_refused() {
        let mut cache = PerCpuFrameCache::new();
        for i in 0..LOCAL_CACHE_SIZE {
            assert!(cache.push(i));
        }
        assert!(!cache.push(999));
    }

    #[test]
    fn cache_size_matches_constant() {
        assert_eq!(LOCAL_CACHE_SIZE, 16);
        assert_eq!(REFILL_BATCH, 8);
    }
}
