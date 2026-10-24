//! Page tables as physical-frame-backed structures.
//!
//! # Why this layer exists
//!
//! Three different things are called "paging" in this kernel, and they are kept
//! apart on purpose:
//!
//! - the *geometry* — the four levels, the shifts, the index extraction — which
//!   lives in [`crate::arch::x86_64::paging`] and in `nova_core::paging`;
//! - the *policy* — which mappings are legal, what the W^X rule is — which
//!   lives in `nova_core::address_space`; and
//! - the *mechanism* — which frames back which tables, and how a walk reaches
//!   a leaf — which is this file.
//!
//! The split matters because the mechanism is the only part that can corrupt
//! memory, and the only part that needs to know how to reach a physical frame.
//! The policy knows nothing about frames, so it can be exercised by the host
//! test suite without a machine.
//!
//! # How a table is reached
//!
//! A page table is one physical frame. The kernel never keeps a pointer to it:
//! a pointer would be tied to one address space, and the tables have to be
//! reachable from every address space. Instead the table is named by its
//! *physical* address, and every access goes through the direct map.
//!
//! That is the whole reason the direct map exists in the kernel's address
//! space: it is the only way to read and write a page table while the CPU is
//! running under a *different* address space. The alternative — mapping each
//! table into every address space — costs a page table's worth of memory per
//! table and buys nothing.

use crate::arch::ADDRESS_SPACE;
use crate::arch::x86_64::paging::{
    ENTRIES_PER_TABLE, PageFlags, decode_entry, encode_entry, entry_flags, entry_is_present,
};
use crate::mm::direct_map;
use nova_core::address_space::VmmError;

/// The no-execute bit of a page-table entry.
///
/// x86-64 has no architectural "execute" bit: a present page is executable
/// unless the *no-execute* bit is set. The bit lives at position 63, which the
/// architectural entry format reserves for exactly this, and it only has its
/// meaning when `EFER.NXE` is enabled.
///
/// It is a bare `u64` rather than a [`PageFlags`] member because
/// [`PageFlags`] validates against the architecturally *defined* bits, and this
/// one is defined only in the presence of `NXE`. Folding it in would make every
/// `from_bits` call reject a perfectly good entry.
pub const NO_EXECUTE: u64 = 1 << 63;

/// A page table occupying exactly one physical frame.
///
/// `Copy` because a table is named by an address, not owned: two handles to
/// the same table are the same table, and copying the handle must not copy the
/// table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageTable {
    /// Physical address of the table's frame.
    phys: u64,
}

impl PageTable {
    /// The table at physical address `phys`.
    ///
    /// `phys` must be page aligned and name a frame that holds a table.
    pub const fn at(phys: u64) -> Self {
        Self { phys }
    }

    /// The table's physical address.
    pub const fn phys(&self) -> u64 {
        self.phys
    }

    /// The raw pointer to this table's entries.
    ///
    /// The pointer is into the direct map, so it is valid in *every* address
    /// space the kernel installs — which is exactly the property the kernel
    /// needs when it edits one process's tables while another address space is
    /// loaded.
    fn entries(&self) -> *mut u64 {
        direct_map::phys_to_virt(self.phys).cast::<u64>()
    }

    /// Read entry `index`.
    ///
    /// Returns `0` for an index past the end of the table rather than
    /// panicking: a walk that asks about an impossible index has already lost
    /// the information needed to explain why.
    pub fn read(&self, index: usize) -> u64 {
        if index >= ENTRIES_PER_TABLE {
            return 0;
        }
        // SAFETY: `entries` points at `ENTRIES_PER_TABLE` eight-byte slots in
        // the direct map, and `index` was just checked to be within them.
        unsafe { *self.entries().add(index) }
    }

    /// Write entry `index`.
    ///
    /// Out-of-range indices are ignored rather than panicking, for the same
    /// reason as [`PageTable::read`].
    pub fn write(&self, index: usize, value: u64) {
        if index >= ENTRIES_PER_TABLE {
            return;
        }
        // SAFETY: as `read`.
        unsafe { *self.entries().add(index) = value };
    }

    /// Whether entry `index` is present.
    pub fn is_present(&self, index: usize) -> bool {
        entry_is_present(self.read(index))
    }

    /// The physical address entry `index` names, if it is present.
    pub fn target(&self, index: usize) -> Option<u64> {
        let entry = self.read(index);
        if entry_is_present(entry) {
            Some(decode_entry(entry))
        } else {
            None
        }
    }

    /// The flags of entry `index`.
    pub fn flags(&self, index: usize) -> PageFlags {
        entry_flags(self.read(index))
    }
}

/// Allocate one zeroed frame to hold a page table.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when the frame allocator is exhausted.
pub fn allocate_table() -> Result<PageTable, VmmError> {
    // SAFETY: `mm::init` has run by the time any address space exists, and the
    // allocator is single-CPU with interrupts masked during construction.
    let allocator = unsafe { crate::mm::frame_allocator() };

    let frame = allocator.alloc().map_err(|_| VmmError::OutOfMemory)?;
    let phys = (frame as u64) * ADDRESS_SPACE.page_size;

    // A table must start out empty. The allocator hands back recycled frames
    // whose old contents are still there, and a stale entry in a fresh table
    // is a mapping nobody asked for — of a frame that may since have been
    // handed to somebody else.
    //
    // SAFETY: `phys` names a frame this process just took ownership of, it is
    // page aligned, and the direct map covers it.
    unsafe {
        core::ptr::write_bytes(
            direct_map::phys_to_virt(phys).cast::<u8>(),
            0,
            ADDRESS_SPACE.page_size as usize,
        );
    }

    Ok(PageTable::at(phys))
}

/// The flags an intermediate (non-leaf) entry needs.
///
/// A table must be writable — the walk writes into it — and it must be
/// supervisor-only: the user half of an address space holds *user* leaves, and
/// a user-readable table would let ring 3 rewrite the walk.
pub const INTERMEDIATE_FLAGS: PageFlags = PageFlags::PRESENT.union(PageFlags::WRITABLE);

/// The flags an intermediate entry in the *kernel* half needs.
///
/// The `GLOBAL` bit is what makes the kernel's own mappings survive a CR3
/// reload: the CPU may keep a global entry in the TLB across a CR3 write,
/// which is the difference between a context switch costing a TLB flush and
/// one costing nothing.
pub const KERNEL_INTERMEDIATE_FLAGS: PageFlags = INTERMEDIATE_FLAGS.union(PageFlags::GLOBAL);

/// The four levels of a walk, from the top down.
///
/// Named rather than numbered so that a call site reads as what it means; the
/// index into a table is the level's *index selector*, not its position in the
/// hierarchy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WalkLevel {
    /// The top-level table: its entries point at directories.
    Pml4,
    /// The directory: its entries point at tables.
    Pdpt,
    /// The table: its entries point at pages.
    Pd,
    /// The leaf: its entries point at frames.
    Pt,
}

impl WalkLevel {
    /// The index a virtual address selects at this level.
    const fn index_of(self, address: u64) -> usize {
        match self {
            WalkLevel::Pml4 => ((address >> 39) & 0x1FF) as usize,
            WalkLevel::Pdpt => ((address >> 30) & 0x1FF) as usize,
            WalkLevel::Pd => ((address >> 21) & 0x1FF) as usize,
            WalkLevel::Pt => ((address >> 12) & 0x1FF) as usize,
        }
    }
}

/// The result of walking to a leaf.
struct Leaf {
    /// The leaf table.
    table: PageTable,
    /// The index of the entry within it.
    index: usize,
}

/// Walk `root` down to the leaf table that holds `address`.
///
/// Allocates the intermediate tables that do not exist yet. `user` selects
/// whether the intermediate tables are user-accessible, which is true only for
/// the user half of an address space.
fn walk_to_leaf(root: &PageTable, address: u64, user: bool) -> Result<Leaf, VmmError> {
    let mut table = *root;

    for level in [WalkLevel::Pml4, WalkLevel::Pdpt, WalkLevel::Pd] {
        let index = level.index_of(address);

        let child = match table.target(index) {
            Some(phys) => PageTable::at(phys),
            None => {
                let child = allocate_table()?;
                let mut flags = if user {
                    INTERMEDIATE_FLAGS.union(PageFlags::USER)
                } else {
                    KERNEL_INTERMEDIATE_FLAGS
                };
                // A kernel-half table that is not global would be flushed on
                // every CR3 reload, and the kernel's own text would be
                // re-faulted on every context switch.
                if !user {
                    flags = flags.union(PageFlags::GLOBAL);
                }
                // SAFETY: the entry's physical address is page aligned and
                // names a frame this walk just allocated.
                let entry = encode_entry(child.phys(), flags);
                table.write(index, entry);
                child
            }
        };

        table = child;
    }

    Ok(Leaf {
        table,
        index: WalkLevel::Pt.index_of(address),
    })
}

/// Map `phys` at `virt` in the address space rooted at `root`.
///
/// The flags are the *leaf* flags. `no_execute` is kept separate because the
/// architectural bit is not part of the ordinary flag word — see
/// [`NO_EXECUTE`].
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when an intermediate table cannot be allocated.
pub fn map(
    root: &PageTable,
    virt: u64,
    phys: u64,
    flags: PageFlags,
    no_execute: bool,
) -> Result<(), VmmError> {
    let leaf = walk_to_leaf(root, virt, flags.contains(PageFlags::USER))?;

    let mut entry = phys | flags.bits();
    if no_execute {
        entry |= NO_EXECUTE;
    }

    leaf.table.write(leaf.index, entry);

    Ok(())
}

/// Remove the mapping at `virt` from the address space rooted at `root`.
///
/// # Errors
///
/// [`VmmError::NotMapped`] when the address has no leaf entry.
pub fn unmap(root: &PageTable, virt: u64) -> Result<(), VmmError> {
    let leaf = walk_to_leaf(root, virt, false)?;

    if !leaf.table.is_present(leaf.index) {
        return Err(VmmError::NotMapped);
    }

    leaf.table.write(leaf.index, 0);

    Ok(())
}

/// The raw leaf entry for `virt`, if the walk reaches one.
pub fn leaf_entry(root: &PageTable, virt: u64) -> Option<u64> {
    let leaf = walk_to_leaf(root, virt, false).ok()?;
    if leaf.table.is_present(leaf.index) {
        Some(leaf.table.read(leaf.index))
    } else {
        None
    }
}

/// Overwrite the raw leaf entry for `virt`.
///
/// # Errors
///
/// [`VmmError::NotMapped`] when the walk reaches no leaf entry.
pub fn write_leaf(root: &PageTable, virt: u64, entry: u64) -> Result<(), VmmError> {
    let leaf = walk_to_leaf(root, virt, false)?;

    if !leaf.table.is_present(leaf.index) {
        return Err(VmmError::NotMapped);
    }

    leaf.table.write(leaf.index, entry);

    Ok(())
}

/// The physical frame mapped at `virt`, if any.
pub fn translate(root: &PageTable, virt: u64) -> Option<u64> {
    let mut table = *root;

    for level in [WalkLevel::Pml4, WalkLevel::Pdpt, WalkLevel::Pd] {
        let index = level.index_of(virt);
        match table.target(index) {
            Some(phys) => table = PageTable::at(phys),
            None => return None,
        }
    }

    let index = WalkLevel::Pt.index_of(virt);
    table.target(index)
}
