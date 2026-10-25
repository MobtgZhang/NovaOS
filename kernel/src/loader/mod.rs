//! Loading a user program into an address space.
//!
//! # What the loader does
//!
//! Given an ELF64 image and an address space, the loader:
//!
//! 1. **Validates** the image against the frozen layout — every segment must
//!    fall inside the derived user window, must not be both writable and
//!    executable, and must not overlap another segment.
//! 2. **Maps** each `PT_LOAD` segment at the virtual address the image
//!    declares, with the protection the segment's flags imply.
//! 3. **Copies** the segment's contents into the frames it just mapped.
//! 4. **Maps** the user stack, with a guard page immediately below it.
//!
//! # Why the addresses are not hardcoded
//!
//! The user *image* is placed wherever the ELF header says, because the ELF
//! header is the contract between the linker script and the loader. The user
//! *window* is derived from the address-space geometry, and the *stack* is an
//! ABI constant shared with `user/linker.ld`. Nothing here repeats the
//! kernel-half boundary or the direct-map offset: those come from
//! [`crate::arch::ADDRESS_SPACE`] and the boot snapshot.
//!
//! # Why the segments are mapped one page at a time
//!
//! A segment's contents rarely end on a page boundary, and the pages at its
//! edges are shared with whatever is on the other side of the boundary. A
//! loader that mapped a whole segment as one unit would either round the
//! segment up — mapping pages that belong to no segment, and giving the
//! program a writable page it did not ask for — or round it down — leaving
//! the tail unmapped.
//!
//! So each page is mapped separately, and the protection of a page is decided
//! by the *flags of the segment* rather than by which part of the segment
//! falls in it. That is conservative in the right direction: a page shared
//! with a neighbouring segment gets the *narrower* of the two protections,
//! because the page that overlaps two segments is mapped twice and the
//! second mapping narrows it.
//!
//! # Why a segment that is both writable and executable is refused
//!
//! Splitting it would need two mappings of the same physical page with
//! different protections, which x86-64 does not support. Refusing is the only
//! honest answer, and it is the same rule the ABI documents.

use crate::arch::ADDRESS_SPACE;
use crate::boot::BootInfo;
use crate::klog;
use crate::logger::Level;
use crate::mm::address_space::{self, AddressSpace};
use crate::mm::frame;
use crate::mm::page_table;

use nova_core::address_space::Protection;
use nova_elf::{Elf, ElfError, PF_W, PF_X};

/// The maximum number of pages a single user stack occupies.
///
/// The stack size is an ABI constant — 64 KiB — and this is its page count.
/// It is derived rather than written down so that a change to the ABI's
/// stack size cannot leave the loader mapping a different number of pages
/// than the ABI reserves.
const STACK_PAGES: u64 = nova_abi::layout::USER_STACK_SIZE / nova_abi::layout::PAGE_SIZE;

/// Why an image could not be loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// The image is not a well-formed ELF64.
    Malformed(ElfError),
    /// A segment falls outside the user window.
    OutsideUserRange,
    /// A segment is both writable and executable.
    WritableAndExecutable,
    /// The frame allocator is out of frames.
    OutOfMemory,
    /// The page tables could not be built.
    TableWalkFailed,
}

impl LoadError {
    /// A stable, lowercase tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            LoadError::Malformed(error) => error.tag(),
            LoadError::OutsideUserRange => "segment-outside-user-range",
            LoadError::WritableAndExecutable => "segment-writable-and-executable",
            LoadError::OutOfMemory => "out-of-memory",
            LoadError::TableWalkFailed => "page-table-walk-failed",
        }
    }
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Malformed(error) => write!(f, "malformed-elf:{}", error.tag()),
            other => f.write_str(other.tag()),
        }
    }
}

/// The result of loading an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoadedImage {
    /// The image's entry point.
    pub entry: u64,
    /// The initial user stack pointer.
    pub stack_pointer: u64,
    /// How many user pages the image's segments occupy.
    pub image_pages: u64,
    /// How many user pages the stack occupies.
    pub stack_pages: u64,
}

/// The protection a segment's flags imply.
///
/// The W^X rule is enforced here, in the one place that turns a file's flags
/// into page-table flags.
fn protection_for(flags: u32) -> Result<Protection, LoadError> {
    let writable = flags & PF_W != 0;
    let executable = flags & PF_X != 0;

    if writable && executable {
        return Err(LoadError::WritableAndExecutable);
    }

    Ok(if writable {
        Protection::UserReadWrite
    } else if executable {
        Protection::UserReadExecute
    } else {
        Protection::UserReadOnly
    })
}

/// Load `image` into `space`.
///
/// The space must already have the kernel half cloned into it — a user
/// address space without the kernel's mappings cannot run a program, because
/// the program's interrupts and syscalls land in the kernel.
///
/// # Errors
///
/// Returns the first rule the image breaks. A malformed image is refused
/// rather than half-loaded, because a program running with half its segments
/// mapped is a program whose failure mode is unpredictable.
pub fn load(
    space: &AddressSpace,
    image: &[u8],
    boot_info: &'static BootInfo,
) -> Result<LoadedImage, LoadError> {
    let elf = Elf::parse(image).map_err(LoadError::Malformed)?;

    // The kernel half's start is derived from the address-space geometry, so
    // the same number the layout document names reaches the validator without
    // any code repeating it.
    elf.validate(ADDRESS_SPACE.kernel_half_start())
        .map_err(LoadError::Malformed)?;

    let window = address_space::user_window();

    // Every segment is checked against the user window *before* anything is
    // mapped, so a rejected image leaves no half-built address space behind.
    for segment in elf.loadable_segments() {
        let start = segment.p_vaddr;
        let end = segment.vaddr_end();

        if !window.contains(start) || !window.contains(end.saturating_sub(1)) {
            return Err(LoadError::OutsideUserRange);
        }
    }

    let mut image_pages = 0u64;

    for segment in elf.loadable_segments() {
        let protection = protection_for(segment.p_flags)?;
        image_pages += map_segment(space, &segment, image, protection)?;
    }

    let stack_pointer = map_user_stack(space, boot_info)?;

    let loaded = LoadedImage {
        entry: elf.entry_point(),
        stack_pointer,
        image_pages,
        stack_pages: STACK_PAGES,
    };

    klog!(
        Level::Info,
        "elf: loaded entry {:#x}, {} image pages, {} stack pages",
        loaded.entry,
        loaded.image_pages,
        loaded.stack_pages,
    );

    Ok(loaded)
}

/// Map one segment's pages and copy its contents into them.
///
/// Returns the number of pages the segment occupied.
fn map_segment(
    space: &AddressSpace,
    segment: &nova_elf::ProgramHeader,
    image: &[u8],
    protection: Protection,
) -> Result<u64, LoadError> {
    let page_size = ADDRESS_SPACE.page_size;
    let base = segment.p_vaddr & !(page_size - 1);
    let end = segment.vaddr_end();
    let last = (end - 1) & !(page_size - 1);

    let mut pages = 0u64;
    let mut virt = base;

    while virt <= last {
        let frame_index = allocate_frame()?;
        let phys = frame_index as u64 * page_size;

        address_space::map(space, virt, phys, protection).map_err(|error| match error {
            nova_core::address_space::VmmError::OutOfMemory => LoadError::OutOfMemory,
            _ => LoadError::TableWalkFailed,
        })?;

        // Zero the frame first. A segment's tail page is only partly covered
        // by the file's contents, and the rest must read as zero rather than
        // as whatever the frame previously held — which, for a recycled
        // frame, is another program's data.
        zero_frame(frame_index, page_size);

        // Copy the part of this page that the segment actually covers.
        //
        // The overlap is computed with saturating arithmetic: a segment whose
        // declared size runs past the end of the file was already rejected by
        // the validator, and a segment that ends exactly at a page boundary
        // has no overlap with the *next* page.
        let page_end = virt + page_size;
        let copy_start = segment.p_vaddr.max(virt);
        let file_end = segment.p_vaddr.saturating_add(segment.p_filesz);
        let copy_end = end.min(page_end).min(file_end);
        if copy_end > copy_start {
            let file_start = (segment.p_offset + (copy_start - segment.p_vaddr)) as usize;
            let length = (copy_end - copy_start) as usize;

            if file_start + length > image.len() {
                return Err(LoadError::Malformed(ElfError::SegmentPastEndOfFile));
            }

            let destination = crate::mm::direct_map::phys_to_virt(phys + (copy_start - virt));
            // SAFETY: the frame was just allocated, so the kernel owns it
            // exclusively, and it is mapped in the direct map at
            // `destination`. The length is bounded by the page size and the
            // copy stays inside the frame.
            unsafe {
                core::ptr::copy_nonoverlapping(image.as_ptr().add(file_start), destination, length);
            }
        }

        pages += 1;
        virt += page_size;
    }

    Ok(pages)
}

/// Map the user stack, leaving a guard page below it.
///
/// The guard is the *absence* of a mapping: the page immediately below the
/// stack is deliberately not mapped, so a stack overflow raises a page fault
/// at the guard rather than silently corrupting whatever lies below.
fn map_user_stack(space: &AddressSpace, _boot_info: &'static BootInfo) -> Result<u64, LoadError> {
    let page_size = ADDRESS_SPACE.page_size;
    let stack_top = nova_abi::layout::USER_STACK_TOP;
    let stack_base = nova_abi::layout::USER_STACK_BASE;

    let mut virt = stack_base;
    while virt < stack_top {
        let frame_index = allocate_frame()?;
        let phys = frame_index as u64 * page_size;

        address_space::map(space, virt, phys, Protection::UserReadWrite).map_err(|error| {
            match error {
                nova_core::address_space::VmmError::OutOfMemory => LoadError::OutOfMemory,
                _ => LoadError::TableWalkFailed,
            }
        })?;

        zero_frame(frame_index, page_size);

        virt += page_size;
    }

    Ok(stack_top)
}

/// Allocate one physical frame.
fn allocate_frame() -> Result<usize, LoadError> {
    // SAFETY: the allocator is installed by `mm::init` before any loader runs,
    // and the kernel's single-CPU discipline means no other allocation is in
    // flight.
    let allocator = unsafe { crate::mm::frame_allocator() };
    allocator.alloc().map_err(|_| LoadError::OutOfMemory)
}

/// Zero a frame through the direct map.
fn zero_frame(frame_index: usize, page_size: u64) {
    let phys = frame_index as u64 * page_size;
    let destination = crate::mm::direct_map::phys_to_virt(phys);

    // SAFETY: the frame was just allocated, so the kernel owns it
    // exclusively, and it is mapped in the direct map. The write covers
    // exactly the frame.
    unsafe {
        core::ptr::write_bytes(destination, 0, page_size as usize);
    }
}

/// Whether `virt` is mapped in `space`.
///
/// Exposed for the acceptance tests: `user_cannot_write_kernel` needs to
/// check that a user address in the kernel half is *not* mapped, and
/// `task_a_cannot_read_task_b` needs to check that one process's pages are
/// invisible to another.
pub fn is_mapped(space: &AddressSpace, virt: u64) -> bool {
    page_table::leaf_entry(&space.pml4(), virt).is_some()
}

/// The physical frame mapped at `virt` in `space`, if any.
pub fn translate(space: &AddressSpace, virt: u64) -> Option<u64> {
    page_table::translate(&space.pml4(), virt)
}

/// Free the frames a loaded image occupies.
///
/// # Safety
///
/// Must only be called for an address space that is being torn down, and the
/// caller must not use the space afterwards.
pub unsafe fn unload(space: &AddressSpace, image: &[u8]) {
    let Ok(elf) = Elf::parse(image) else {
        return;
    };

    let page_size = ADDRESS_SPACE.page_size;

    for segment in elf.loadable_segments() {
        let base = segment.p_vaddr & !(page_size - 1);
        let end = segment.vaddr_end();
        let last = (end - 1) & !(page_size - 1);

        let mut virt = base;
        while virt <= last {
            if let Some(phys) = translate(space, virt) {
                let _ = address_space::unmap(space, virt);
                // SAFETY: the frame was allocated by this loader for this
                // space and is being torn down, so it is not referenced
                // anywhere else.
                unsafe {
                    let allocator = crate::mm::frame_allocator();
                    allocator.dealloc((phys / page_size) as usize);
                }
            }
            virt += page_size;
        }
    }
}

/// Report the loader's view of the image, for the boot log.
pub fn describe_image(image: &[u8]) -> Result<(), LoadError> {
    let elf = Elf::parse(image).map_err(LoadError::Malformed)?;
    elf.validate(ADDRESS_SPACE.kernel_half_start())
        .map_err(LoadError::Malformed)?;
    Ok(())
}

/// The frame allocator's statistics, for the boot log.
pub fn describe_allocator() -> frame::AllocatorStats {
    // SAFETY: the allocator is installed before any loader runs.
    let allocator = unsafe { crate::mm::frame_allocator() };
    frame::describe_allocator(allocator)
}
