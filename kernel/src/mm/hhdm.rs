//! Higher-Half Direct Map (HHDM) access.
//!
//! The bootloader maps every physical byte at `virtual = physical + hhdm_offset`,
//! which is how the kernel touches memory that is not in its own image. The
//! offset itself is captured once during boot and stored in [`HHDM_OFFSET`].

use core::sync::atomic::{AtomicU64, Ordering};

/// Captured HHDM offset, written once during `boot::limine_adapter::capture`.
///
/// `0` means "not yet initialised"; every reader is expected to run after
/// capture, so the atomic load is really just a compiler fence.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Store the HHDM offset, taking a writable copy from the Limine response.
///
/// # Safety
///
/// Must be called exactly once, from `boot::limine_adapter::capture`, before
/// the bootloader reclaims its response pages.
pub unsafe fn set(offset: u64) {
    HHDM_OFFSET.store(offset, Ordering::Release);
}

/// Read the HHDM offset, panicking if it has not been set.
///
/// Useful for kernel code that runs after capture, where an unset offset
/// indicates a programming error rather than a recoverable condition.
#[inline]
pub fn offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Acquire)
}

/// Convert a physical address to its HHDM virtual address.
///
/// # Safety
///
/// The caller must guarantee `physical` is the physical address of a frame
/// inside the addressable memory range, and that the kernel is permitted to
/// access it. Touching the resulting pointer is then safe.
#[inline]
pub const fn phys_to_virt(physical: u64) -> *mut u8 {
    // Adding the offset is a constant expression at compile time when
    // `physical` is a literal, so this does not require `unsafe` arithmetic.
    (physical + offset()) as *mut u8
}

/// Convert an HHDM virtual address to its physical address.
#[inline]
pub const fn virt_to_phys(virtual_address: u64) -> u64 {
    virtual_address - offset()
}