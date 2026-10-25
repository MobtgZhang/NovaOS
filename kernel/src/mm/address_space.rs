//! Per-process address spaces: the kernel's own tables and each task's.
//!
//! # The two halves
//!
//! An x86-64 address space is a PML4 table with 512 entries. Entries 0..256
//! select the *lower* half of the address space and entries 256..512 the
//! *upper* half. NovaOS uses that split directly:
//!
//! - **Upper half** (entries 256..512): the kernel's image, the HHDM, and
//!   everything else that is shared by every process. The top-level entries
//!   are *copied* into every process's PML4, so they point at the *same*
//!   intermediate tables. The cost is one PML4's worth of memory per process,
//!   not one per mapping.
//!
//! - **Lower half** (entries 0..256): private to one process. Two processes
//!   may map different physical frames at the same virtual address, and
//!   neither can see the other's.
//!
//! # Why the kernel half is copied and not shared
//!
//! x86-64 has no "kernel address space" register: the only way to have the
//! kernel's mappings visible in a user process is to put them in the same
//! PML4. Copying the *entries* (not the tables they point at) is therefore
//! both necessary and cheap.
//!
//! # The HHDM
//!
//! The direct map is `physical + direct_map_offset`, and the offset comes
//! from the bootloader. The kernel's own tables must map it, because the
//! frame allocator reaches physical memory through it and the loader writes
//! into user frames through it. Without it the kernel could not even build the
//! tables it is building.
//!
//! # The recursive part
//!
//! Every page table is mapped into the HHDM, so a table named by a *physical*
//! address is reachable from the kernel's address space. That is what lets
//! one process's tables be edited while another address space is loaded.

use crate::arch::ADDRESS_SPACE;
use crate::arch::x86_64::paging::{
    ENTRIES_PER_TABLE, PageFlags, decode_entry, encode_entry, entry_is_present,
};
use crate::boot::BootInfo;
use crate::klog;
use crate::logger::Level;
use crate::mm::page_table::{self, NO_EXECUTE, PageTable};

use core::sync::atomic::{AtomicU64, Ordering};

use nova_core::address_space::{Protection, UserWindow, VmmError};

/// The number of PML4 entries that select the lower (user) half.
pub const USER_HALF_ENTRIES: usize = 256;

/// The first PML4 entry that selects the upper (kernel) half.
///
/// Entry 256 is the first one whose virtual addresses have bit 47 set, so it
/// is exactly the boundary between the two halves — and it is derived from the
/// entry count rather than written down.
pub const KERNEL_HALF_FIRST_ENTRY: usize = USER_HALF_ENTRIES;

/// Flags for an intermediate table entry.
///
/// A table must be writable (the walk writes into it) and supervisor-only: the
/// user half of an address space holds *user* leaves, and a user-readable
/// table would let ring 3 rewrite the walk and map the kernel's own pages.
pub const INTERMEDIATE_FLAGS: PageFlags = PageFlags::PRESENT.union(PageFlags::WRITABLE);

/// A page table reachable through the direct map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddressSpace {
    /// Physical address of the PML4.
    pml4_phys: u64,
}

impl AddressSpace {
    /// The address space rooted at the PML4 at `pml4_phys`.
    pub const fn at(pml4_phys: u64) -> Self {
        Self { pml4_phys }
    }

    /// The physical address of this space's PML4.
    pub const fn pml4_phys(&self) -> u64 {
        self.pml4_phys
    }

    /// The PML4 as a [`PageTable`].
    pub const fn pml4(&self) -> PageTable {
        PageTable::at(self.pml4_phys)
    }

    /// Whether this space has the kernel's own mappings.
    ///
    /// A space built by [`AddressSpace::create_kernel`] does; a space built by
    /// [`AddressSpace::create_user`] does not until the kernel half is cloned
    /// into it.
    pub fn has_kernel_half(&self) -> bool {
        for index in KERNEL_HALF_FIRST_ENTRY..ENTRIES_PER_TABLE {
            if self.pml4().is_present(index) {
                return true;
            }
        }
        false
    }
}

/// The kernel's own address space.
///
/// Built once, at boot, and used as the *source* for every process's kernel
/// half. The kernel keeps running on this space's tables until it installs a
/// process's; the scheduler reloads CR3 on every switch.
static KERNEL_SPACE_PML4: AtomicU64 = AtomicU64::new(0);

/// The kernel's address space.
///
/// # Safety
///
/// Must only be called after [`init`] has run.
pub fn kernel_space() -> AddressSpace {
    let pml4_phys = KERNEL_SPACE_PML4.load(Ordering::Acquire);
    if pml4_phys == 0 {
        panic!("kernel address space used before mm::address_space::init (pml4={pml4_phys:#x})");
    }
    AddressSpace::at(pml4_phys)
}

/// Build the kernel's own address space and install it.
///
/// The space is built by *copying* the bootloader's active page tables rather
/// than by mapping physical memory from scratch, because the bootloader
/// already mapped everything the kernel needs — the image, the HHDM, the
/// boot stack — and reproducing that mapping by hand would mean hardcoding
/// addresses the bootloader chose.
///
/// # Safety
///
/// Must be called exactly once, from boot, with interrupts masked, after
/// `mm::init` has published the frame allocator.
pub unsafe fn init(boot_info: &'static BootInfo) {
    let kernel = build_kernel_space(boot_info);

    let previous = KERNEL_SPACE_PML4.load(Ordering::Acquire);
    KERNEL_SPACE_PML4.store(kernel.pml4_phys(), Ordering::Release);
    klog!(
        Level::Info,
        "vmm: kernel space publish previous={previous:#x} value={:#x}",
        KERNEL_SPACE_PML4.load(Ordering::Acquire)
    );

    klog!(
        Level::Info,
        "vmm: kernel address space at pml4 {:#x}",
        kernel.pml4_phys()
    );
}

/// Build a new address space holding the kernel's mappings.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when the frame allocator cannot supply the
/// tables.
pub fn create_process_space(boot_info: &'static BootInfo) -> Result<AddressSpace, VmmError> {
    klog!(
        Level::Info,
        "vmm: create process enter global={:#x}",
        KERNEL_SPACE_PML4.load(Ordering::Acquire)
    );
    let kernel = kernel_space();
    let pml4 = page_table::allocate_table()?;
    klog!(
        Level::Info,
        "vmm: process pml4 {:#x} global={:#x}",
        pml4.phys(),
        KERNEL_SPACE_PML4.load(Ordering::Acquire)
    );

    // Clone the kernel half: the same intermediate tables, reachable from this
    // space's PML4.
    clone_kernel_half(&kernel.pml4(), &pml4)?;

    // The HHDM is part of the kernel half in the *bootloader's* tables, so the
    // clone above already carries it — but only if the bootloader's tables had
    // it. Building our own tables means we must ensure the HHDM is mapped in
    // *this* space, which is what `map_hhdm` does below.
    map_hhdm(&pml4, boot_info)?;

    Ok(AddressSpace::at(pml4.phys()))
}

/// Build the kernel's own address space from the bootloader's tables.
///
/// The bootloader leaves a working set of tables loaded in CR3, with the
/// kernel image and the HHDM already mapped. Rather than reconstruct that
/// mapping — which would mean hardcoding the addresses the bootloader picked —
/// the kernel *copies* the bootloader's tables into tables it owns.
///
/// The copy is a deep copy of the top level only: the kernel half's
/// intermediate tables are shared with the bootloader's, exactly as a
/// process's are shared with the kernel's. The user half is not copied at
/// all, because nothing has run in it yet.
fn build_kernel_space(boot_info: &'static BootInfo) -> AddressSpace {
    let boot_pml4 = PageTable::at(crate::arch::x86_64::paging::current_page_directory());

    let pml4 = page_table::allocate_table().expect("the kernel address space needs one frame");

    // Copy the kernel half of the bootloader's tables.
    clone_kernel_half(&boot_pml4, &pml4)
        .expect("cloning the kernel half needs only the frames it already has");

    // The bootloader's HHDM may not cover every usable region the firmware
    // reported — Limine maps the HHDM over the whole physical range, but the
    // kernel does not rely on that, and a firmware that mapped less would
    // otherwise leave the frame allocator reaching into unmapped memory.
    map_hhdm(&pml4, boot_info).expect("mapping the HHDM needs frames");

    AddressSpace::at(pml4.phys())
}

/// Copy the kernel half of `source` into `destination`.
///
/// Only the top-level entries are copied, so both spaces share the same
/// intermediate tables. That is what makes the kernel's mappings cost one
/// page table's worth of memory in total rather than one per process.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when a table must be allocated.
fn clone_kernel_half(source: &PageTable, destination: &PageTable) -> Result<(), VmmError> {
    for index in KERNEL_HALF_FIRST_ENTRY..ENTRIES_PER_TABLE {
        if let Some(child) = source.target(index) {
            // The entry's flags are copied, including the `GLOBAL` bit that
            // keeps the kernel's mappings across a CR3 reload, but any
            // bootloader-created user bit is removed: the kernel half is
            // supervisor-only by definition.
            let entry = encode_entry(child, source.flags(index).without(PageFlags::USER));
            destination.write(index, entry);
        }
    }

    Ok(())
}

/// Map the HHDM over the firmware's usable regions.
///
/// The mapping covers exactly the regions the firmware called *usable*, not
/// the whole physical range: a process must not be handed a frame the firmware
/// said was not RAM.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when a frame cannot be allocated.
fn map_hhdm(pml4: &PageTable, boot_info: &'static BootInfo) -> Result<(), VmmError> {
    let offset = boot_info.direct_map_offset;
    let page_size = ADDRESS_SPACE.page_size;

    for region in boot_info.regions() {
        if !region.is_usable() {
            continue;
        }

        let base = region.base & !(page_size - 1);
        let end = region.end();
        let mut address = base;

        while address < end {
            // The HHDM is a kernel mapping: writable and executable, and never
            // user-accessible. It is the kernel's own window onto physical
            // memory, and a ring 3 mapping of it would hand every process the
            // whole machine.
            let flags = PageFlags::PRESENT
                .union(PageFlags::WRITABLE)
                .union(PageFlags::GLOBAL);

            page_table::map(pml4, address + offset, address, flags, false)?;

            address += page_size;
        }
    }

    Ok(())
}

/// The user window this kernel permits.
///
/// Derived from the address-space geometry, not written down: the lower bound
/// is the page after the null page and the upper bound is the last canonical
/// *user* page — one page below `2^(virtual_bits - 1)`, not one page below
/// the kernel half, which would be a non-canonical address above every
/// address the hardware can reach.
///
/// The kernel half's start is passed alongside the width because the two must
/// agree; `UserWindow::derive` refuses a geometry in which they do not.
pub fn user_window() -> UserWindow {
    UserWindow::derive(
        ADDRESS_SPACE.virtual_bits.unwrap_or(48),
        ADDRESS_SPACE.kernel_half_start(),
    )
}

/// Map `phys` at `virt` in `space`, with `protection`.
///
/// The protection is the *policy*; this function is the *mechanism*. The
/// policy check lives in `nova_core::address_space::AddressSpace::check_map`,
/// which is the same code the host test suite exercises.
///
/// # Errors
///
/// [`VmmError::ProtectionNotAllowed`] when the protection would give a user
/// page both write and execute rights, and [`VmmError::OutsideUserRange`]
/// when a user mapping falls outside the user window.
pub fn map(
    space: &AddressSpace,
    virt: u64,
    phys: u64,
    protection: Protection,
) -> Result<(), VmmError> {
    let window = user_window();

    if protection.is_user() {
        if !window.contains(virt) {
            return Err(VmmError::OutsideUserRange);
        }
        // The W^X rule, enforced here rather than by the caller: the caller is
        // exactly the code that would get it wrong.
        if protection.is_writable() && protection.is_executable() {
            return Err(VmmError::ProtectionNotAllowed);
        }
    }

    let flags = protection.page_flags();
    let no_execute = !protection.is_executable();

    page_table::map(&space.pml4(), virt, phys, flags, no_execute)
}

/// Unmap `virt` from `space`.
///
/// # Errors
///
/// [`VmmError::NotMapped`] when the address has no leaf entry.
pub fn unmap(space: &AddressSpace, virt: u64) -> Result<(), VmmError> {
    page_table::unmap(&space.pml4(), virt)
}

/// The physical frame mapped at `virt` in `space`, if any.
pub fn translate(space: &AddressSpace, virt: u64) -> Option<u64> {
    page_table::translate(&space.pml4(), virt)
}

/// Translate a user-owned page, refusing the shared kernel half.
pub fn translate_user(space: &AddressSpace, virt: u64) -> Option<u64> {
    page_table::translate_user(&space.pml4(), virt)
}

pub fn map_kernel_mmio(virt: u64, phys: u64) -> Result<(), VmmError> {
    let space = kernel_space();
    let flags = PageFlags::PRESENT
        .union(PageFlags::WRITABLE)
        .union(PageFlags::GLOBAL);
    page_table::map(&space.pml4(), virt, phys, flags, false)
}

/// Install `space`'s tables in CR3.
///
/// # Safety
///
/// `space` must map every address the kernel is currently using — the kernel
/// image, the HHDM, and the current stack — or the very next instruction
/// faults.
pub unsafe fn activate(space: &AddressSpace) {
    // SAFETY: delegated to this function's contract. The space was built by
    // `create_process_space` or `build_kernel_space`, both of which map the
    // kernel image's virtual range and the HHDM.
    unsafe {
        crate::arch::x86_64::paging::load_page_directory(space.pml4_phys());
    }
}

/// Whether the address space at `pml4_phys` has the kernel half mapped.
///
/// Exposed for the acceptance tests, which need to check that a *new*
/// address space really is private below the kernel half.
pub fn has_kernel_half(pml4_phys: u64) -> bool {
    let space = AddressSpace::at(pml4_phys);
    space.has_kernel_half()
}

/// The physical address of the intermediate table a PML4 entry names.
///
/// Used by the acceptance tests to prove that two address spaces share the
/// *same* kernel tables rather than each having its own copy — the property
/// that makes the kernel half cost one page table in total.
pub fn kernel_half_child(pml4_phys: u64, index: usize) -> Option<u64> {
    let space = AddressSpace::at(pml4_phys);
    let entry = space.pml4().read(index);
    if entry_is_present(entry) {
        Some(decode_entry(entry))
    } else {
        None
    }
}

/// The number of present PML4 entries in the kernel half.
///
/// The acceptance tests use this to show that a process's kernel half is a
/// *copy* of the kernel's — the same number of entries, pointing at the same
/// tables — rather than an independently-built hierarchy.
pub fn kernel_half_entry_count(pml4_phys: u64) -> usize {
    let space = AddressSpace::at(pml4_phys);
    (KERNEL_HALF_FIRST_ENTRY..ENTRIES_PER_TABLE)
        .filter(|index| space.pml4().is_present(*index))
        .count()
}

/// Mark a page as not writable, in place.
///
/// The W^X rule is enforced at mapping time, but a loader that maps a
/// segment's contents and *then* makes it read-only needs this: the segment's
/// last page is partly initialised and partly not, so it must be writable
/// while the loader fills it and read-only afterwards.
///
/// # Errors
///
/// [`VmmError::NotMapped`] when the address is not mapped.
pub fn set_writable(space: &AddressSpace, virt: u64, writable: bool) -> Result<(), VmmError> {
    let leaf = page_table::leaf_entry(&space.pml4(), virt).ok_or(VmmError::NotMapped)?;

    let mut flags =
        PageFlags::from_bits(leaf & PageFlags::RESERVED_MASK.bits()).unwrap_or(PageFlags::NONE);
    if writable {
        flags = flags.union(PageFlags::WRITABLE);
    } else {
        flags = flags.without(PageFlags::WRITABLE);
    }

    let entry = encode_entry(decode_entry(leaf), flags) | (leaf & NO_EXECUTE);
    page_table::write_leaf(&space.pml4(), virt, entry);

    Ok(())
}
