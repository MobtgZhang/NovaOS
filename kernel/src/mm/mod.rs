//! Physical memory: the frame allocator and the direct map.
//!
//! # Architecture neutrality
//!
//! The frame allocator is the largest piece of kernel logic that has nothing to
//! do with instruction sets, so it is shared wholesale. What it takes from the
//! architecture is exactly two things:
//!
//! - the page size, via [`arch::ADDRESS_SPACE`](crate::arch::ADDRESS_SPACE);
//! - a way to touch a frame, via [`direct_map`].
//!
//! That is why the allocator's unsafe surface is one thin shell around a safe,
//! host-testable [`frame::Bitmap`]: a third architecture would reuse the bitmap
//! unchanged.

pub mod direct_map;
pub mod frame;

use core::cell::UnsafeCell;

use crate::arch::ADDRESS_SPACE;
use crate::boot::BootInfo;
use crate::klog;
use crate::logger::Level;
use crate::mm::frame::FrameAllocator;

/// Bring up physical memory management for the validated boot snapshot.
///
/// Records the direct-map offset so frames become reachable, then publishes the
/// frame allocator with everything below the kernel image already reserved.
///
/// # Safety
///
/// Requires `boot_info` to be a validated snapshot (i.e. `boot::capture` has
/// returned) and the direct map to be live.
pub unsafe fn init(boot_info: &'static BootInfo) {
    // SAFETY: delegated to this function's contract. The bootloader leaves the
    // direct map mapped for the kernel's whole lifetime.
    unsafe {
        direct_map::set(boot_info.direct_map_offset);
    }

    // The kernel image is linked at a higher-half VMA but loaded at a low LMA.
    // Allocating any frame the image occupies would corrupt the running kernel,
    // so the allocator's floor sits one frame past its physical end.
    let image_end = kernel_image_end_frame();

    klog!(
        Level::Debug,
        "mm: kernel image ends at frame {image_end} (phys {:#x}), direct_map={:#x}",
        image_end as u64 * ADDRESS_SPACE.page_size,
        boot_info.direct_map_offset
    );

    // SAFETY: called once from `_start`, before any reader exists. `init` is the
    // only writer of `ALLOCATOR`.
    unsafe {
        ALLOCATOR.init(FrameAllocator::new(boot_info, image_end));
    }

    // SAFETY: delegated to this function's contract; the allocator was just
    // published and has no other readers yet.
    let allocator = unsafe { ALLOCATOR.get() };

    klog!(Level::Info, "mm: {}", frame::describe_allocator(allocator));
}

/// Holds the frame allocator behind an `UnsafeCell`.
///
/// The allocator is installed once and then read from arbitrary kernel code.
/// Wrapping it keeps that hand-off explicit; M2 replaces it with a locked
/// allocator when preemption makes single-CPU access an assumption rather than a
/// guarantee.
struct SingleAllocator {
    inner: UnsafeCell<Option<FrameAllocator>>,
}

// SAFETY: `init` writes `inner` exactly once from `_start` before any reader
// exists, and no other method ever creates a second reference to it. Concurrent
// `alloc`/`dealloc` — the case this impl does *not* claim to cover — is
// prevented by v0.1.0's single-CPU, interrupts-masked-allocation rule. M2
// replaces this type with a real lock.
unsafe impl Sync for SingleAllocator {}

impl SingleAllocator {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }

    /// Install the allocator.
    ///
    /// # Safety
    ///
    /// Must be called exactly once, before any reader exists.
    unsafe fn init(&self, allocator: FrameAllocator) {
        // SAFETY: delegated to this function's contract.
        unsafe {
            *self.inner.get() = Some(allocator);
        }
    }

    /// Borrow the installed allocator.
    ///
    /// # Safety
    ///
    /// Must only be called after `init`. The returned reference never aliases a
    /// later `init`, because `init` runs at most once.
    #[inline]
    unsafe fn get(&self) -> &'static FrameAllocator {
        // SAFETY: delegated to this function's contract.
        unsafe {
            (&*self.inner.get())
                .as_ref()
                .expect("frame allocator used before mm::init")
        }
    }
}

static ALLOCATOR: SingleAllocator = SingleAllocator::new();

/// The global frame allocator.
///
/// # Safety
///
/// `mm::init` must already have run.
#[inline]
pub unsafe fn frame_allocator() -> &'static FrameAllocator {
    // SAFETY: delegated to this function's contract.
    unsafe { ALLOCATOR.get() }
}

/// First physical frame past the kernel image.
///
/// Derived from linker-script symbols rather than hard-coded, so changing the
/// link-time load address needs no code change. The symbols each backend's linker
/// script defines are named identically across architectures precisely so this
/// function does not need a `cfg`.
fn kernel_image_end_frame() -> usize {
    let size = kernel_image_size_bytes();
    let phys_end = kernel_image_phys_base() + size;

    // Round *up*: a partial trailing page is still partly occupied, so rounding
    // down would hand it out.
    let page = ADDRESS_SPACE.page_size;
    (ADDRESS_SPACE.align_up(phys_end) / page) as usize
}

/// Physical load base of the kernel image.
///
/// The `extern` block is what makes the symbol exist at link time; reading its
/// address never dereferences the memory it marks, so no `unsafe` is needed at
/// the call site beyond the block's own declaration.
fn kernel_image_phys_base() -> u64 {
    unsafe extern "C" {
        static __kernel_phys_base: u8;
    }
    core::ptr::addr_of!(__kernel_phys_base) as u64
}

/// Virtual base of the kernel image.
fn kernel_image_vma_base() -> u64 {
    unsafe extern "C" {
        static __kernel_vma_base: u8;
    }
    core::ptr::addr_of!(__kernel_vma_base) as u64
}

/// First byte past the kernel image, in virtual addresses.
fn kernel_image_vma_end() -> u64 {
    unsafe extern "C" {
        static __image_end_vma: u8;
    }
    core::ptr::addr_of!(__image_end_vma) as u64
}

/// Size of the kernel image in bytes.
///
/// Derived from the virtual-address span, which equals the physical size because
/// every linker script offsets LMA and VMA by the same constant.
fn kernel_image_size_bytes() -> u64 {
    kernel_image_vma_end().saturating_sub(kernel_image_vma_base())
}