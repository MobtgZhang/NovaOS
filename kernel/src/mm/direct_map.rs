//! Direct physical-memory access.
//!
//! # What this is
//!
//! The direct map is the kernel's window onto physical memory: every byte of RAM
//! is mapped at `virtual = physical + offset`. It is what lets the frame allocator
//! return *frames* rather than addresses, and it is the reason a frame's contents
//! can be written without the caller building a page table.
//!
//! # Naming
//!
//! x86-64 bootloaders call this the HHDM (Higher-Half Direct Map) and AArch64
//! bootloaders provide the equivalent mapping, but the concept is one idea. This
//! module uses the architecture-neutral name; the Limine adapter's request is
//! still called `HhdmRequest` because that is the protocol's own field.
//!
//! # Lifecycle
//!
//! The offset is written exactly once, during [`crate::boot::capture`], from the
//! bootloader's response. After that the bootloader is free to reclaim its pages,
//! so the value must live in kernel-owned storage — which is what the atomic
//! below is.

use core::sync::atomic::{AtomicU64, Ordering};

/// Captured direct-map offset, written once during boot capture.
///
/// `0` means "not yet initialised"; every reader runs after capture, so the
/// acquire/release pairing is a publication barrier rather than contention.
static DIRECT_MAP_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Store the direct-map offset.
///
/// # Safety
///
/// Must be called exactly once, from `boot::capture`, before the bootloader
/// reclaims its response pages.
pub unsafe fn set(offset: u64) {
    DIRECT_MAP_OFFSET.store(offset, Ordering::Release);
}

/// Read the direct-map offset.
///
/// # Panics
///
/// Never. Before capture this returns `0`, which turns any derived pointer into a
/// low address that will fault loudly rather than silently aliasing something.
/// Panicking here would make the pre-boot logging path unusable.
#[inline]
pub fn offset() -> u64 {
    DIRECT_MAP_OFFSET.load(Ordering::Acquire)
}

/// Convert a physical address to its direct-map virtual address.
///
/// # Safety
///
/// The caller must guarantee `physical` is the physical address of a frame inside
/// the addressable memory range, and that the kernel is permitted to access it.
/// Touching the resulting pointer is then safe.
#[inline]
pub fn phys_to_virt(physical: u64) -> *mut u8 {
    // Not `const`: it reads the offset from an atomic, so it can only be
    // evaluated after `boot::capture` has published it. A `const` signature here
    // would let a caller bake `physical + 0` into a constant and never see the
    // offset change at all.
    //
    // Adding a constant offset to a page-aligned physical address preserves
    // alignment, so the result is suitable for any type up to the page size.
    (physical + offset()) as *mut u8
}

/// Convert a direct-map virtual address back to its physical address.
#[inline]
pub fn virt_to_phys(virtual_address: u64) -> u64 {
    virtual_address.wrapping_sub(offset())
}

/// Borrow a frame's contents as a byte slice.
///
/// # Safety
///
/// `frame` must be a frame this kernel currently owns, and the slice must not
/// outlive that ownership: the frame allocator can hand the page to someone else
/// the moment it is freed, at which point the slice is a use-after-free.
#[inline]
pub unsafe fn frame_bytes(frame: u64, len: usize) -> &'static mut [u8] {
    // SAFETY: delegated to this function's contract; the direct map is live for
    // the kernel's whole lifetime and the caller owns the frame.
    unsafe { core::slice::from_raw_parts_mut(phys_to_virt(frame), len) }
}