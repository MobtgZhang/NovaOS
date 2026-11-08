//! The kernel heap: the kernel's first general-purpose allocator.
//!
//! # What this is
//!
//! Every allocation before this module was a compile-time-sized static array —
//! eight tasks, eight stacks, sixteen channels — so every resource had a hard
//! ceiling and exceeding it was a refusal, not a slowdown (§2.5, §8.2). This
//! module is the escape from that world: a `GlobalAlloc` over the frame
//! allocator, so kernel code can name sizes at run time and `Box`, `Vec`, and
//! the rest of `alloc` work in the kernel.
//!
//! # The shape
//!
//! Memory arrives from the frame allocator in whole frames and is sub-allocated
//! by a first-fit free list kept in **address order**. Address order is the
//! load-bearing choice: a free of a block walks the list anyway, and walking in
//! address order puts a block's two coalescing candidates — the free block
//! ending where it begins, and the free block beginning where it ends —
//! adjacent in the walk, so coalescing is two comparisons rather than a data
//! structure of its own. Frames handed out one at a time are usually
//! themselves adjacent, so sequential growth merges into large blocks for
//! free.
//!
//! Each block opens with one word: the block's total byte size, with the low
//! bit set while the block is allocated. The bit is what makes a double free a
//! refused no-op instead of silent corruption. A *free* block keeps its
//! successor's address in a second word; an allocated block keeps nothing,
//! because the free of an allocation carries the layout with it. The smallest
//! block is 32 bytes, so every split leaves a remainder that can still hold a
//! header.
//!
//! # Interrupt discipline
//!
//! Heap operations mask interrupts *before* taking the lock, and this is not a
//! style choice. The kernel's spinlock is re-entrant by CPU slot: an interrupt
//! on the same processor that tries to lock while the interrupted code holds
//! the lock is treated as legitimate recursion and let straight in. For a
//! mutex guarding a multi-step free-list update that is precisely the wrong
//! behaviour — the handler would mutate a half-updated list. Masking first
//! makes same-processor re-entry impossible, and the frame allocator's own
//! lock is taken only while the mask is up.
//!
//! # Growth
//!
//! There is no fixed arena and no shrink: when no free block fits, the heap
//! claims enough frames from the frame allocator to cover the request and
//! links them in as one block. An allocation therefore fails only when the
//! physical frame supply does, which is the machine's real limit rather than a
//! constant someone picked. The frame allocator is the single owner of
//! physical memory; the heap is one consumer among several, and it never
//! hands frames back (kernel heaps do not shrink).
//!
//! # Bootstrap ordering
//!
//! The allocator is installed from the first instruction that runs, but the
//! frame allocator it draws from only exists after `mm::init`. No allocation
//! happens before then — the boot path is deliberately allocation-free — so
//! the ordering holds by construction rather than by a check on every call.

use core::alloc::{GlobalAlloc, Layout};

use crate::arch::capabilities::InterruptControl;
use crate::sync::SpinLock;

/// Every block begins on a 16-byte boundary, so any alignment up to this is
/// free.
const BLOCK_ALIGN: usize = 16;

/// The size field's low bit: set while the block is allocated.
const USED_BIT: usize = 1;

/// The smallest block the heap will carve, bytes including the header.
///
/// A free block needs room for its size word and its successor word; 32 bytes
/// leaves the remainder of any split able to hold both and keeps every block
/// two words even when the allocation itself is tiny.
const MIN_BLOCK: usize = 32;

/// The heap's state, all of it behind the lock.
struct HeapState {
    /// The free list head: an address, or `0` for an empty list.
    free: usize,
    /// How many frames the heap has claimed from the frame allocator.
    frames: usize,
    /// Bytes in allocated blocks, headers included — counted as blocks are
    /// taken and given back, independently of the free list the stats walk.
    used: usize,
}

impl HeapState {
    const fn new() -> Self {
        Self {
            free: 0,
            frames: 0,
            used: 0,
        }
    }
}

/// The kernel's global allocator. See the module docs for the design.
pub struct KernelHeap {
    inner: SpinLock<HeapState>,
}

impl KernelHeap {
    /// The allocator, empty: the first allocation claims the first frames.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(HeapState::new()),
        }
    }

    /// A snapshot of the heap's usage, for diagnostics and tests.
    ///
    /// The counts are derived by walking the free list, so they are exact
    /// rather than accumulated — no counter to drift.
    pub fn stats(&self) -> HeapStats {
        // Masking is the heap's interrupt discipline, not an optimisation:
        // see the module docs. The trait method needs the trait in scope,
        // which the import above provides.
        let _mask = crate::arch::interrupts().mask_preserving();
        let state = self.inner.lock();

        let mut free = 0usize;
        let mut cursor = state.free;
        while cursor != 0 {
            // SAFETY: every address on the free list is the start of a live
            // free block inside memory the heap owns.
            free += unsafe { block_size(cursor) };
            cursor = unsafe { next_of(cursor) };
        }

        HeapStats {
            frames: state.frames,
            used_bytes: state.used,
            free_bytes: free,
        }
    }

    /// Allocate, with the caller's layout.
    ///
    /// # Safety
    ///
    /// Same contract as [`GlobalAlloc::alloc`].
    unsafe fn allocate(&self, layout: Layout) -> *mut u8 {
        // Masking is the heap's interrupt discipline: see the module docs.
        let _mask = crate::arch::interrupts().mask_preserving();
        let mut state = self.inner.lock();

        let block = match self.first_fit(&mut state, layout) {
            Some(block) => block,
            None => return self.grow_and_take(&mut state, layout),
        };
        // SAFETY: `block` is a used block wide enough for `layout`.
        let payload = unsafe { take_block(block, layout) };
        payload
    }

    /// Free, with the caller's layout.
    ///
    /// # Safety
    ///
    /// Same contract as [`GlobalAlloc::dealloc`]: `ptr` must have come from
    /// this allocator with this layout, and must not be freed twice.
    unsafe fn release(&self, ptr: *mut u8, layout: Layout) {
        // Masking is the heap's interrupt discipline: see the module docs.
        let _mask = crate::arch::interrupts().mask_preserving();
        let mut state = self.inner.lock();

        let block = if layout.align() > BLOCK_ALIGN {
            // The aligned allocation stashed the distance back to the block
            // start in the word before the pointer it returned.
            // SAFETY: the word was written by `take_block` for exactly this
            // layout, inside memory the heap owns.
            let distance = unsafe { core::ptr::read((ptr as usize - 8) as *const usize) };
            ptr as usize - distance
        } else {
            ptr as usize - BLOCK_ALIGN
        };

        // SAFETY: `block` is the start of the allocation's own block.
        unsafe {
            if !is_used(block) {
                // A double free: refuse it here rather than letting the
                // coalesce walk build a list out of a live block's innards.
                return;
            }
            clear_used(block);
            state.used -= block_size(block);
        }
        // SAFETY: `block` is a whole block the heap owns, no longer marked
        // used.
        unsafe { insert_free(&mut state, block) };
    }

    /// The first free block that fits `layout`, splitting it when the
    /// remainder can still be a block. Marks the winner used and returns its
    /// address.
    fn first_fit(&self, state: &mut HeapState, layout: Layout) -> Option<usize> {
        let wanted = rounded_width(layout)?;

        let mut prev: usize = 0;
        let mut cursor = state.free;
        while cursor != 0 {
            // SAFETY: the walk visits only live free blocks.
            let size = unsafe { block_size(cursor) };
            if size >= wanted {
                if size - wanted >= MIN_BLOCK {
                    // Split: the tail of this block becomes a free block of
                    // its own, already in address order.
                    let rest = cursor + wanted;
                    // SAFETY: the split point and the original block are
                    // inside memory the heap owns.
                    unsafe {
                        // The tail is a free block; the head is trimmed to
                        // exactly the width this allocation takes, or the
                        // coalesce arithmetic would later read past it.
                        write_size(rest, size - wanted);
                        write_next(rest, next_of(cursor));
                        write_size(cursor, wanted);
                    }
                    // SAFETY: `cursor` was the walk position on the list.
                    unsafe { link_after(state, prev, rest, cursor) };
                    state.used += wanted;
                } else {
                    // Too small to split: the whole block goes.
                    // SAFETY: `cursor` was the walk position on the list.
                    unsafe { link_after(state, prev, 0, cursor) };
                    state.used += size;
                }
                // SAFETY: the block was on the free list a moment ago.
                unsafe { set_used(cursor) };
                return Some(cursor);
            }
            prev = cursor;
            cursor = unsafe { next_of(cursor) };
        }
        None
    }

    /// Claim frames until a block of `layout`'s width fits, then allocate
    /// from it.
    fn grow_and_take(&self, state: &mut HeapState, layout: Layout) -> *mut u8 {
        let wanted = match rounded_width(layout) {
            Some(width) => width,
            None => return core::ptr::null_mut(),
        };

        // SAFETY: `mm::init` has run — the boot path allocates only after it —
        // and the allocator hands each frame out exactly once.
        let allocator = unsafe { crate::mm::frame_allocator() };
        let page = crate::arch::ADDRESS_SPACE.page_size as usize;
        let need = wanted.div_ceil(page).max(1);

        // Frames from a sequential allocation are usually adjacent, so claim
        // frames until one run is wide enough for the request, banking every
        // shorter run into the free list as its own block. The heap never
        // returns frames, so a frame claimed here must land in some block —
        // a run abandoned mid-growth would leak it. A bitmap hole ends one
        // run and starts the next; it does not end the growth, because the
        // frames past the hole may be contiguous enough to satisfy the
        // request on their own, and `insert_free` will later merge runs that
        // turned out to touch. The budget keeps a pathologically fragmented
        // bitmap from consuming every frame in one growth.
        let mut run_start = 0usize;
        let mut run_frames = 0usize;
        let mut claimed = 0usize;
        let budget = need.saturating_mul(2);
        while claimed < budget {
            let frame = match allocator.alloc() {
                Ok(frame) => frame,
                Err(_) => break,
            };
            claimed += 1;
            let address = frame * page;
            if run_frames > 0 && run_start + run_frames * page != address {
                // A hole: the current run ends here. Bank it and start a new
                // run at the frame that broke the adjacency.
                let frames = run_frames;
                run_frames = 0;
                // SAFETY: the frames were just allocated to the heap, and the
                // direct map covers every physical frame the snapshot
                // described.
                let block = crate::mm::direct_map::phys_to_virt(run_start as u64) as usize;
                // SAFETY: `block` is a fresh, otherwise-unowned region.
                unsafe { write_size(block, frames * page) };
                // SAFETY: `block` is a whole free block the heap owns.
                unsafe { insert_free(state, block) };
                state.frames += frames;
            }
            if run_frames == 0 {
                run_start = address;
            }
            run_frames += 1;
            if run_frames * page >= wanted {
                break;
            }
        }
        // Bank whatever the last run accumulated: wide enough to be this
        // allocation's block, or a partial run a later growth will merge
        // with its neighbours through `insert_free`.
        if run_frames > 0 {
            let frames = run_frames;
            // SAFETY: as above — fresh frames, covered by the direct map.
            let block = crate::mm::direct_map::phys_to_virt(run_start as u64) as usize;
            // SAFETY: `block` is a fresh, otherwise-unowned region.
            unsafe { write_size(block, frames * page) };
            // SAFETY: `block` is a whole free block the heap owns.
            unsafe { insert_free(state, block) };
            state.frames += frames;
        }

        let found = match self.first_fit(state, layout) {
            Some(found) => found,
            None => return core::ptr::null_mut(),
        };
        // SAFETY: `found` is a used block wide enough for `layout`. The
        // payload pointer — not the block address — is what the caller sees;
        // handing back the block address would point the caller at the size
        // word, and the first write through it would erase the header.
        let payload = unsafe { take_block(found, layout) };
        payload
    }
}

/// A point-in-time picture of the heap, for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    /// Frames the heap has claimed from the frame allocator.
    pub frames: usize,
    /// Bytes in allocated blocks, headers included.
    pub used_bytes: usize,
    /// Bytes sitting on the free list.
    pub free_bytes: usize,
}

/// The block width `layout` needs, header included, or `None` when the layout
/// cannot be honoured at all.
fn rounded_width(layout: Layout) -> Option<usize> {
    if layout.align() > BLOCK_ALIGN {
        // Room for the payload, the alignment padding, and the back-offset
        // word: over-allocate by a full `align` and the arithmetic below is
        // safe whatever the block's own alignment.
        let width = layout.size().checked_add(layout.align() * 2)?;
        Some(width.max(MIN_BLOCK).next_multiple_of(BLOCK_ALIGN))
    } else {
        let width = layout.size().max(1).checked_add(BLOCK_ALIGN)?;
        Some(width.max(MIN_BLOCK).next_multiple_of(BLOCK_ALIGN))
    }
}

/// Claim a block for `layout` and return the pointer the caller sees.
///
/// For ordinary alignments the payload starts right after the header, on a
/// 16-byte boundary. A stricter alignment gets an interior pointer with the
/// distance back to the block start stashed in the word before it — the same
/// word `release` reads to find its way home.
///
/// # Safety
///
/// `block` must be a used block wide enough for `layout`.
unsafe fn take_block(block: usize, layout: Layout) -> *mut u8 {
    if layout.align() > BLOCK_ALIGN {
        // SAFETY: the payload area starts at `block + BLOCK_ALIGN` and runs
        // for at least `layout.align() * 2` bytes, so the word at `payload -
        // 8` is inside this block either way.
        let payload = unsafe {
            let start = block + BLOCK_ALIGN;
            let aligned = start.next_multiple_of(layout.align());
            core::ptr::write((aligned - 8) as *mut usize, aligned - block);
            aligned as *mut u8
        };
        // The stashed distance makes `release` find `block`; the size word
        // already says how wide it is.
        payload
    } else {
        (block + BLOCK_ALIGN) as *mut u8
    }
}

/// Replace `gone` with `block` on the free list, or drop `gone` entirely when
/// `block` is `0`; `block`'s successor becomes `gone`'s.
///
/// # Safety
///
/// `prev` and `gone` must name the walk position: `prev` is the free block
/// before `gone` (or `0` for the head) and `gone` is on the list.
unsafe fn link_after(state: &mut HeapState, prev: usize, block: usize, gone: usize) {
    // SAFETY: `gone` is a live free block.
    let follower = unsafe { next_of(gone) };
    let replacement = if block == 0 { follower } else { block };
    if prev == 0 {
        state.free = replacement;
    } else {
        // SAFETY: `prev` is a live free block.
        unsafe { write_next(prev, replacement) };
    }
    if block != 0 {
        // SAFETY: `block` is a live free block.
        unsafe { write_next(block, follower) };
    }
}

// The block header accessors. Every one of them addresses a live block the
// heap owns; the callers' SAFETY comments carry that invariant, and these
// wrappers keep the pointer arithmetic in one audited place rather than
// scattered across the walk.

/// A block's total byte size, allocated or free.
///
/// # Safety
///
/// `block` must be the start of a block in memory the heap owns.
unsafe fn block_size(block: usize) -> usize {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe { core::ptr::read(block as *const usize) & !USED_BIT }
}

/// Whether a block is allocated.
///
/// # Safety
///
/// `block` must be the start of a block in memory the heap owns.
unsafe fn is_used(block: usize) -> bool {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe { core::ptr::read(block as *const usize) & USED_BIT != 0 }
}

/// Mark a block allocated.
///
/// # Safety
///
/// `block` must be the start of a free block in memory the heap owns.
unsafe fn set_used(block: usize) {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe {
        let size = core::ptr::read(block as *const usize);
        core::ptr::write(block as *mut usize, size | USED_BIT);
    }
}

/// Mark a block free, keeping its size.
///
/// # Safety
///
/// `block` must be the start of an allocated block in memory the heap owns.
unsafe fn clear_used(block: usize) {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe {
        let size = core::ptr::read(block as *const usize) & !USED_BIT;
        core::ptr::write(block as *mut usize, size);
    }
}

/// Write a block's total size, replacing any previous one.
///
/// # Safety
///
/// `block` must be the start of a block in memory the heap owns.
unsafe fn write_size(block: usize, size: usize) {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe {
        core::ptr::write(block as *mut usize, size);
    }
}

/// A free block's successor on the list, or `0`.
///
/// # Safety
///
/// `block` must be the start of a block in memory the heap owns.
unsafe fn next_of(block: usize) -> usize {
    // SAFETY: the caller's contract puts `block` inside heap memory, and a
    // free block's second word is its successor.
    unsafe { core::ptr::read((block + 8) as *const usize) }
}

/// Overwrite a free block's successor.
///
/// # Safety
///
/// `block` must be the start of a free block in memory the heap owns.
unsafe fn write_next(block: usize, next: usize) {
    // SAFETY: the caller's contract puts `block` inside heap memory.
    unsafe {
        core::ptr::write((block + 8) as *mut usize, next);
    }
}

/// Put a whole free block on the list, coalescing with the free blocks that
/// end where it begins and begin where it ends.
///
/// # Safety
///
/// `block` must be the start of a free block — a freshly claimed run of
/// frames, or an allocated block with its used bit just cleared.
unsafe fn insert_free(state: &mut HeapState, block: usize) {
    // The walk finds the list position: `prev` the last free block below,
    // `next` the first free block at or above.
    let mut prev: usize = 0;
    let mut next = state.free;
    while next != 0 && next < block {
        prev = next;
        next = unsafe { next_of(next) };
    }

    // Merge with the block below when they touch.
    let mut start = block;
    let mut width = unsafe { block_size(block) };
    if prev != 0 {
        // SAFETY: `prev` is a live free block.
        let prev_size = unsafe { block_size(prev) };
        if prev + prev_size == block {
            width += prev_size;
            // SAFETY: `prev` is a live free block.
            unsafe { write_size(prev, width) };
            start = prev;
        } else {
            // SAFETY: `block` is a live free block.
            unsafe {
                write_next(block, next);
                if prev == 0 {
                    state.free = block;
                } else {
                    write_next(prev, block);
                }
            }
        }
    } else {
        // SAFETY: `block` is a live free block.
        unsafe {
            write_next(block, next);
            state.free = block;
        }
    }

    // Merge with the block above when they touch.
    if next != 0 && start + width == next {
        // SAFETY: `next` is a live free block.
        let next_size = unsafe { block_size(next) };
        width += next_size;
        // SAFETY: `start` is on the list already.
        unsafe {
            write_size(start, width);
            write_next(start, next_of(next));
        }
    }
}

/// The heap itself, installed as the kernel's global allocator.
#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap::new();

/// Borrow the global allocator.
///
/// The `GlobalAlloc` impl lives on the same value, so `alloc::` code goes
/// through the static directly; this accessor exists for the kernel's own
/// call sites and tests that want [`KernelHeap::stats`].
pub fn heap() -> &'static KernelHeap {
    &KERNEL_HEAP
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { self.allocate(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { self.release(ptr, layout) }
    }
}
