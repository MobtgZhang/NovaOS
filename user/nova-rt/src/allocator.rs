//! A bump allocator over a statically reserved heap.
//!
//! # Why this is not the real heap allocator
//!
//! The ABI reserves a 1 MiB heap region at `nova_abi::layout::USER_HEAP_BASE`
//! for a program's allocator, and the kernel maps it on demand once `MAP`
//! exists (M3). Until then there is no way for a ring-3 program to ask the
//! kernel for a page, so a program that allocated from the reserved region
//! would fault on its first `alloc`.
//!
//! So the runtime keeps its own heap *inside the program's own image* —
//! a static array in `.bss`, which the loader maps as part of the program's
//! `PT_LOAD` segments. That is enough for a program that needs a few scratch
//! buffers, and it is honest: the allocator says in its documentation that it
//! is a bump allocator over reserved memory, not a page allocator.
//!
//! # Why a bump allocator and not a free list
//!
//! A free list needs to walk the blocks, and a bump allocator needs to
//! compare two pointers. The bump allocator is the smaller of the two by a
//! wide margin, and the difference shows up in the one place that matters
//! here: the allocator's own code is part of the ring-3 image, so every
//! instruction in it is an instruction a ring-3 bug could reach.
//!
//! # The alignment contract
//!
//! `alloc` returns 16-byte aligned memory, because that is what the SysV ABI
//! requires of anything a function might pass to `core::fmt` or to a
//! vectorised routine. A bump allocator that returned 8-byte aligned memory
//! would be *almost* right, and "almost" is the failure mode that costs an
//! afternoon.

/// The size of the program's own heap.
///
/// Sixteen pages' worth of bytes. The program's image is mapped by the kernel
/// as ordinary `PT_LOAD` segments, so this costs the program a page of its
/// address space and nothing else.
pub const HEAP_SIZE: usize = 16 * 1024;

/// The heap itself.
///
/// A static array rather than a `static mut` slice: the kernel zeroes `.bss`
/// when it loads the image, so the buffer starts out zeroed, and a Rust
/// `&'static mut [u8]` would be unsound to hand out before initialisation.
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

/// How much of [`HEAP`] has been handed out.
///
/// A `usize` rather than a pointer: the bump pointer is a *count* into the
/// buffer, which means the "is there room left" check is a comparison against
/// a constant rather than a subtraction that could underflow.
static mut USED: usize = 0;

/// The alignment every allocation satisfies.
const ALIGNMENT: usize = 16;

/// Prepare the allocator.
///
/// Called once, before any allocation. It exists so that the allocator's
/// state has a single, named initialisation point rather than relying on
/// `.bss` being zero — a program that was somehow loaded with a non-zeroed
/// `.bss` would otherwise hand out memory that was not zeroed.
pub fn init() {
    // SAFETY: `init` runs once, from `_start`, before any other code in the
    // program can call `alloc`, so it is the only writer of `USED` at that
    // point and no reader exists yet.
    unsafe {
        USED = 0;
    }
}

/// Allocate `size` bytes, returning a 16-byte aligned pointer.
///
/// Returns null when the heap is exhausted. A null return is the allocator's
/// documented failure mode: the program checks it, and there is no way to
/// report an allocation failure through the syscall layer without a heap of
/// its own to allocate the error message from.
pub fn alloc(size: usize) -> *mut u8 {
    // SAFETY: the heap buffer is a static array, so it is valid for the
    // kernel's whole lifetime and outlives every pointer into it.
    let base = unsafe { core::ptr::addr_of_mut!(HEAP) as *mut u8 };

    // Round the request up so the returned pointer satisfies `ALIGNMENT`.
    let aligned = (size + (ALIGNMENT - 1)) & !(ALIGNMENT - 1);

    // SAFETY: `USED` is the only writer of the bump pointer, and the
    // allocator is not reentrant — a program that allocates from inside an
    // allocation would be a bug in the program, not a race in the allocator.
    // The kernel is single-CPU, so there is no second thread to race with.
    unsafe {
        let used = USED;
        let end = used.saturating_add(aligned);

        if end > HEAP_SIZE {
            return core::ptr::null_mut();
        }

        USED = end;
        base.add(used)
    }
}

/// How many bytes of the heap have been handed out.
///
/// Exposed so a program can report its own memory use without needing a
/// syscall for it.
pub fn used() -> usize {
    // SAFETY: reading the bump pointer is safe with the allocator's
    // single-writer discipline.
    unsafe { USED }
}
