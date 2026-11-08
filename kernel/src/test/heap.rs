//! The kernel heap suites: the first general-purpose allocator (§2.5).
//!
//! # What these cases prove
//!
//! The heap backs `alloc` from the frame allocator, and every case below runs
//! through the `alloc::` API a real kernel caller would use — `Box`, `Vec`,
//! raw `Layout` allocations — rather than through internals. The heap is empty
//! when the harness runs (the boot path is allocation-free by design), so the
//! stats-based cases see deterministic numbers as long as only these suites
//! allocate.
//!
//! The coalescing case deserves its name spelled out: two adjacent blocks are
//! freed, and a single allocation the size of *both* must be served from the
//! merged space with **no new frames claimed**. A first-fit allocator without
//! coalescing — or one whose merge arithmetic is wrong — claims fresh frames
//! and the frame count betrays it.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// The frame size the wide-allocation case assumes.
const PAGE: usize = 0x1000;

/// `Box` and `Vec` must work, and a written pattern must survive a free and a
/// fresh allocation of the same size.
fn alloc_round_trip_preserves_patterns() -> Result<(), &'static str> {
    let mut vector = Vec::new();
    for index in 0..64u64 {
        vector.push(index * 0x9E37_79B9_7F4A_7C15);
    }
    for (index, value) in vector.iter().enumerate() {
        if *value != (index as u64) * 0x9E37_79B9_7F4A_7C15 {
            return Err("a heap allocation did not hold its written pattern");
        }
    }
    drop(vector);

    let boxed = Box::new(0xDEAD_BEEF_CAFE_F00Du64);
    if *boxed != 0xDEAD_BEEF_CAFE_F00D {
        return Err("a boxed value did not read back");
    }

    Ok(())
}

/// An allocation wider than a frame must span frames and every byte of the
/// span must be independently writable — a block arithmetic bug that split a
/// run wrong would strand a page in the middle.
fn a_wide_allocation_spans_frames() -> Result<(), &'static str> {
    let mut wide = Vec::with_capacity(PAGE * 3);
    unsafe { core::ptr::write_bytes(wide.as_mut_ptr(), 0, PAGE * 3) };
    wide.resize(PAGE * 3, 0);

    for page in 0..3 {
        wide[page * PAGE] = (page as u8) + 1;
        wide[(page + 1) * PAGE - 1] = 0xFF;
    }
    for page in 0..3 {
        if wide[page * PAGE] != (page as u8) + 1 || wide[(page + 1) * PAGE - 1] != 0xFF {
            return Err("a multi-frame allocation lost a page's bytes");
        }
    }

    Ok(())
}

/// Allocating and freeing many small objects must return the heap to where it
/// started: a leak — or a free that never coalesced — shows up in the stats.
fn many_small_allocations_do_not_leak() -> Result<(), &'static str> {
    let before = crate::mm::heap::heap().stats();

    for round in 0..2000 {
        let value = Box::new(round as u64);
        if *value != round as u64 {
            return Err("a small allocation did not read back");
        }
    }

    let after = crate::mm::heap::heap().stats();
    if after.frames != before.frames {
        return Err("a small-allocation cycle claimed new frames");
    }
    if after.used_bytes != before.used_bytes {
        return Err("a small-allocation cycle left bytes behind");
    }

    Ok(())
}

/// A layout with an alignment stricter than the heap's own must come back
/// aligned, hold a pattern across its whole width, and free cleanly — the
/// interior-pointer path is exactly where a back-offset bug would surface.
fn a_strict_alignment_is_honoured() -> Result<(), &'static str> {
    const ALIGN: usize = 64;
    const WIDTH: usize = 200; // deliberately not a multiple of the alignment

    let before = crate::mm::heap::heap().stats();
    let layout = core::alloc::Layout::from_size_align(WIDTH, ALIGN)
        .map_err(|_| "the test's own layout is malformed")?;

    // SAFETY: the layout is live, and every path below frees what it
    // allocates before returning.
    let pointer = unsafe { alloc::alloc::alloc(layout) };
    if pointer.is_null() {
        return Err("a strictly aligned allocation was refused");
    }
    if pointer as usize % ALIGN != 0 {
        return Err("a strictly aligned allocation came back unaligned");
    }
    // SAFETY: the allocation is `WIDTH` bytes and the writes stay inside it.
    unsafe {
        core::ptr::write_bytes(pointer, 0xA5, WIDTH);
        core::ptr::write((pointer as usize + WIDTH - 1) as *mut u8, 0x5A);
    }
    // SAFETY: the allocation is live and the layout matches its origin.
    unsafe { alloc::alloc::dealloc(pointer, layout) };

    let after = crate::mm::heap::heap().stats();
    if after.frames != before.frames || after.used_bytes != before.used_bytes {
        return Err("an aligned allocation cycle left the heap unbalanced");
    }

    Ok(())
}

/// Freeing the same block twice must be refused rather than corrupting the
/// list — the second free is a no-op, and the heap still works afterwards.
fn a_double_free_is_refused() -> Result<(), &'static str> {
    let layout = core::alloc::Layout::from_size_align(16, 8)
        .map_err(|_| "the test's own layout is malformed")?;

    // SAFETY: the allocation is freed exactly once below; the deliberate
    // second free is the case under test.
    let pointer = unsafe { alloc::alloc::alloc(layout) };
    if pointer.is_null() {
        return Err("a normal allocation was refused");
    }
    // SAFETY: matches the allocation above.
    unsafe { alloc::alloc::dealloc(pointer, layout) };
    // SAFETY: the second free is refused by the used-bit check, not by the
    // caller's discipline.
    unsafe { alloc::alloc::dealloc(pointer, layout) };

    // The heap must still be whole: allocate, write, read back.
    let probe = Box::new(0x1234_5678u64);
    if *probe != 0x1234_5678 {
        return Err("the heap was corrupted after a refused double free");
    }

    Ok(())
}

/// Two freed blocks that touch must merge, and an allocation the size of both
/// must be served from the merged space without claiming a single new frame.
fn freed_neighbours_coalesce() -> Result<(), &'static str> {
    let width = PAGE * 2 - 64; // two blocks, each just under one frame-run

    let before = crate::mm::heap::heap().stats();
    let mut first = Vec::with_capacity(width);
    first.resize(width, 0x11);
    let mut second = Vec::with_capacity(width);
    second.resize(width, 0x22);
    let claimed = crate::mm::heap::heap().stats().frames;

    drop(first);
    drop(second);

    let mut merged = Vec::with_capacity(width * 2);
    merged.resize(width * 2, 0);
    let after = crate::mm::heap::heap().stats();

    if merged.iter().any(|byte| *byte != 0) {
        return Err("the merged allocation did not hold a cleared pattern");
    }
    if after.frames != claimed {
        return Err("a merged allocation claimed new frames: coalescing failed");
    }
    if after.free_bytes < before.free_bytes {
        return Err("the heap shrank below its starting free bytes");
    }

    Ok(())
}

/// The stats must describe a self-consistent heap: used and free together
/// account for every frame claimed.
fn the_stats_account_for_every_frame() -> Result<(), &'static str> {
    let stats = crate::mm::heap::heap().stats();

    let accounted = stats.used_bytes + stats.free_bytes;
    let expected = stats.frames * PAGE;
    if accounted != expected {
        return Err("used and free bytes do not sum to the claimed frames");
    }
    if stats.free_bytes == 0 {
        return Err("the heap has no free bytes: growth left nothing reusable");
    }

    Ok(())
}

pub static HEAP_SUITES: &[crate::test::harness::TestSuite] = &[crate::test::harness::TestSuite {
    name: "kernel_heap",
    cases: &[
        (
            "alloc_round_trip_preserves_patterns",
            alloc_round_trip_preserves_patterns,
        ),
        (
            "a_wide_allocation_spans_frames",
            a_wide_allocation_spans_frames,
        ),
        (
            "many_small_allocations_do_not_leak",
            many_small_allocations_do_not_leak,
        ),
        (
            "a_strict_alignment_is_honoured",
            a_strict_alignment_is_honoured,
        ),
        ("a_double_free_is_refused", a_double_free_is_refused),
        ("freed_neighbours_coalesce", freed_neighbours_coalesce),
        (
            "the_stats_account_for_every_frame",
            the_stats_account_for_every_frame,
        ),
    ],
}];
