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
    /// The table holding the leaf entry.
    table: PageTable,
    /// The index of the entry within it.
    index: usize,
    /// The byte size of the page the entry describes: 4 KiB for an ordinary
    /// leaf, 2 MiB for a directory leaf (`PS` at the directory level), or
    /// 1 GiB for a directory-pointer leaf (`PS` one level up).
    size: u64,
}

/// The leaf sizes a walk can produce, named for the call sites that compare
/// against them.
pub const P1_PAGE_BYTES: u64 = 0x1000;
pub const P2_PAGE_BYTES: u64 = 0x20_0000;
pub const P3_PAGE_BYTES: u64 = 0x4000_0000;

/// The physical address a leaf entry maps for a virtual address.
///
/// A huge leaf names the base of its whole 2 MiB or 1 GiB region, so the
/// offset within the page has to be added back; an ordinary leaf's frame
/// address *is* the mapped address of the page base. Callers that want a
/// frame base mask the result themselves.
fn leaf_physical(entry: u64, virt: u64, size: u64) -> u64 {
    if size == P1_PAGE_BYTES {
        return decode_entry(entry);
    }
    decode_entry(entry) | (virt & (size - 1))
}

/// Walk `root` down to the leaf entry that holds `address`.
///
/// Allocates the intermediate tables that do not exist yet. `user` selects
/// whether the intermediate tables are user-accessible, which is true only for
/// the user half of an address space.
///
/// # Huge pages
///
/// The walk stops at the first level whose entry carries the `PS` bit: a
/// directory-pointer leaf means the walk is over at the *second* level, and a
/// directory leaf at the third. The returned [`Leaf`]'s `size` field tells the
/// caller which level it stopped at, and every write through the leaf goes to
/// that entry rather than to a fourth-level entry that does not exist.
fn walk_to_leaf(root: &PageTable, address: u64, user: bool) -> Result<Leaf, VmmError> {
    let mut table = *root;

    for (level, size) in [
        // A `PS` bit at the PML4 level is architecturally reserved — no real
        // leaf exists there — and the size paired with it is unreachable in
        // practice. A 1 GiB name keeps the pairing monotonic if a corrupted
        // entry ever sets the bit.
        (WalkLevel::Pml4, P3_PAGE_BYTES),
        // A `PS` bit one level below the PML4 — in a directory-pointer
        // entry — makes a 1 GiB leaf.
        (WalkLevel::Pdpt, P3_PAGE_BYTES),
        // A `PS` bit in a directory entry makes a 2 MiB leaf.
        (WalkLevel::Pd, P2_PAGE_BYTES),
    ] {
        let index = level.index_of(address);

        let entry = table.read(index);
        if entry_is_present(entry) && entry_flags(entry).contains(PageFlags::HUGE) {
            // The entry is itself a leaf. The walk cannot descend further, and
            // a caller that wanted a fourth-level entry is answered with the
            // huge entry that *is* the leaf for this address.
            return Ok(Leaf { table, index, size });
        }

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
        size: P1_PAGE_BYTES,
    })
}

/// Map `phys` at `virt` in the address space rooted at `root`.
///
/// The flags are the *leaf* flags. `no_execute` is kept separate because the
/// architectural bit is not part of the ordinary flag word — see
/// [`NO_EXECUTE`].
///
/// # Huge pages
///
/// When the walk reaches a huge leaf (an HHDM built from 2 MiB or 1 GiB
/// leaves), the entry it reaches *is* the mapping for `virt`. If that mapping
/// already names `phys`, the call is a no-op — remapping the same address is
/// idempotent. If it names something else, the call fails with
/// [`VmmError::AlreadyMapped`] rather than silently splitting the huge page:
/// a split would unmap the 511 neighbouring pages a caller may still be
/// using, and no caller here needs one.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when an intermediate table cannot be allocated,
/// [`VmmError::AlreadyMapped`] when a huge leaf covers `virt` with a
/// different frame.
pub fn map(
    root: &PageTable,
    virt: u64,
    phys: u64,
    flags: PageFlags,
    no_execute: bool,
) -> Result<(), VmmError> {
    let leaf = walk_to_leaf(root, virt, flags.contains(PageFlags::USER))?;

    if leaf.size != P1_PAGE_BYTES {
        let entry = leaf.table.read(leaf.index);
        if leaf_physical(entry, virt, leaf.size) == phys {
            return Ok(());
        }
        return Err(VmmError::AlreadyMapped);
    }

    let mut entry = phys | flags.bits();
    if no_execute {
        entry |= NO_EXECUTE;
    }

    leaf.table.write(leaf.index, entry);

    Ok(())
}

/// Map one huge leaf of `size` bytes at `virt`.
///
/// `size` is [`P3_PAGE_BYTES`] for a directory-pointer leaf or
/// [`P2_PAGE_BYTES`] for a directory leaf; the walk allocates every
/// intermediate table above the leaf level that does not exist yet. The leaf
/// entry carries the `HUGE` bit, which is what makes the hardware treat it as
/// a page rather than a table.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when an intermediate table cannot be allocated.
fn map_huge(
    root: &PageTable,
    virt: u64,
    phys: u64,
    size: u64,
    flags: PageFlags,
    no_execute: bool,
) -> Result<(), VmmError> {
    // The tables *above* the leaf level: the PML4 for a 1 GiB leaf, and the
    // PML4 plus the directory-pointer table for a 2 MiB one.
    let above: &[WalkLevel] = if size == P3_PAGE_BYTES {
        &[WalkLevel::Pml4]
    } else {
        &[WalkLevel::Pml4, WalkLevel::Pdpt]
    };

    let mut table = *root;
    for level in above {
        let index = level.index_of(virt);
        let child = match table.target(index) {
            Some(child) => PageTable::at(child),
            None => {
                let child = allocate_table()?;
                // The HHDM is a kernel mapping, so the intermediates are
                // supervisor-only and global, exactly as `walk_to_leaf`
                // builds them for the kernel half.
                let entry = encode_entry(
                    child.phys(),
                    KERNEL_INTERMEDIATE_FLAGS.union(PageFlags::GLOBAL),
                );
                table.write(index, entry);
                child
            }
        };
        table = child;
    }

    let index = if size == P3_PAGE_BYTES {
        WalkLevel::Pdpt.index_of(virt)
    } else {
        WalkLevel::Pd.index_of(virt)
    };

    let mut entry = phys | flags.union(PageFlags::HUGE).bits();
    if no_execute {
        entry |= NO_EXECUTE;
    }
    table.write(index, entry);

    Ok(())
}

/// Map a physical range at `virt` using the largest leaves possible.
///
/// Each aligned 1 GiB chunk becomes a directory-pointer leaf (when
/// `one_gib_pages` says the CPU can form one), each aligned 2 MiB chunk a
/// directory leaf, and everything else 4 KiB pages. A region's unaligned head
/// and tail therefore cost a handful of 4 KiB pages, and the *body* costs one
/// entry per 1 GiB or 2 MiB — which is the difference between a handful of
/// table frames for a 192 GiB machine and the 48 million page-table frames a
/// 4 KiB-only direct map would need.
///
/// The caller guarantees `virt`, `phys`, and `len` are page aligned and that
/// the range does not overlap an existing mapping: the walk never splits a
/// huge leaf, so a range that crosses one is answered by [`VmmError::AlreadyMapped`]
/// from [`map`] rather than by a silent partial mapping.
///
/// # Errors
///
/// [`VmmError::OutOfMemory`] when a table cannot be allocated, and
/// [`VmmError::AlreadyMapped`] when any part of the range hits a huge leaf
/// mapping a different frame.
pub fn map_range(
    root: &PageTable,
    virt: u64,
    phys: u64,
    len: u64,
    flags: PageFlags,
    no_execute: bool,
    one_gib_pages: bool,
) -> Result<(), VmmError> {
    let mut done = 0u64;
    while done < len {
        let v = virt + done;
        let p = phys + done;
        let left = len - done;

        if one_gib_pages
            && v % P3_PAGE_BYTES == 0
            && p % P3_PAGE_BYTES == 0
            && left >= P3_PAGE_BYTES
        {
            map_huge(root, v, p, P3_PAGE_BYTES, flags, no_execute)?;
            done += P3_PAGE_BYTES;
            continue;
        }

        if v % P2_PAGE_BYTES == 0 && p % P2_PAGE_BYTES == 0 && left >= P2_PAGE_BYTES {
            map_huge(root, v, p, P2_PAGE_BYTES, flags, no_execute)?;
            done += P2_PAGE_BYTES;
            continue;
        }

        map(root, v, p, flags, no_execute)?;
        done += P1_PAGE_BYTES;
    }

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

/// The raw leaf PTE for `virt`, without allocating tables.
///
/// Temporary diagnosis helper: returns the leaf entry's exact value, or `0`
/// when the walk reaches a not-present intermediate entry.
pub fn raw_leaf_entry(root: &PageTable, virt: u64) -> u64 {
    let mut table = *root;

    for level in [WalkLevel::Pml4, WalkLevel::Pdpt, WalkLevel::Pd] {
        let index = level.index_of(virt);
        let entry = table.read(index);
        if !entry_is_present(entry) || entry_flags(entry).contains(PageFlags::HUGE) {
            return entry;
        }
        match table.target(index) {
            Some(phys) => table = PageTable::at(phys),
            None => return 0,
        }
    }

    table.read(WalkLevel::Pt.index_of(virt))
}

/// The physical frame mapped at `virt`, if any.
///
/// Huge leaves are handled: a 2 MiB or 1 GiB leaf contributes the offset
/// within its page, so the result is the exact physical address of `virt`
/// rather than the base of the region it falls in.
pub fn translate(root: &PageTable, virt: u64) -> Option<u64> {
    let mut table = *root;

    for (level, size) in [
        // A `PS` bit at the PML4 level is architecturally reserved — no real
        // leaf exists there — and the size paired with it is unreachable in
        // practice. A 1 GiB name keeps the pairing monotonic if a corrupted
        // entry ever sets the bit.
        (WalkLevel::Pml4, P3_PAGE_BYTES),
        // A `PS` bit one level below the PML4 — in a directory-pointer
        // entry — makes a 1 GiB leaf.
        (WalkLevel::Pdpt, P3_PAGE_BYTES),
        // A `PS` bit in a directory entry makes a 2 MiB leaf.
        (WalkLevel::Pd, P2_PAGE_BYTES),
    ] {
        let index = level.index_of(virt);
        let entry = table.read(index);
        if !entry_is_present(entry) {
            return None;
        }
        if entry_flags(entry).contains(PageFlags::HUGE) {
            return Some(leaf_physical(entry, virt, size));
        }
        table = PageTable::at(decode_entry(entry));
    }

    let index = WalkLevel::Pt.index_of(virt);
    table.target(index)
}

/// Translate a *user* virtual address only if every walk level is user-accessible.
///
/// Process spaces share the kernel half with ring 0, so a plain translation can
/// resolve a supervisor page. User-pointer copies must not use that result.
pub fn translate_user(root: &PageTable, virt: u64) -> Option<u64> {
    let mut table = *root;

    for (level, size) in [
        (WalkLevel::Pml4, P3_PAGE_BYTES),
        (WalkLevel::Pdpt, P3_PAGE_BYTES),
        (WalkLevel::Pd, P2_PAGE_BYTES),
    ] {
        let index = level.index_of(virt);
        let entry = table.read(index);
        if !entry_is_present(entry) || !entry_flags(entry).contains(PageFlags::USER) {
            return None;
        }
        // A huge leaf in a user space would make the descent below read the
        // leaf's 2 MiB base as if it were a table. None of today's user
        // mappings are huge, but a corrupted or future one gets the same
        // offset arithmetic a kernel translation would give rather than
        // garbage.
        if entry_flags(entry).contains(PageFlags::HUGE) {
            return Some(leaf_physical(entry, virt, size));
        }
        table = PageTable::at(decode_entry(entry));
    }

    let index = WalkLevel::Pt.index_of(virt);
    let entry = table.read(index);
    if !entry_is_present(entry) || !entry_flags(entry).contains(PageFlags::USER) {
        return None;
    }
    Some(decode_entry(entry))
}
