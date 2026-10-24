//! x86-64 four-level page-table geometry and address decomposition.
//!
//! # Why this is a separate crate
//!
//! The interesting part of a page-table implementation is the *walk*: the
//! sequence of index extractions, the decision about when an intermediate
//! table must be allocated, and the bookkeeping that frees it again when its
//! last child goes away. None of that needs a CPU.
//!
//! Putting the walk in a crate of its own means the host test suite can
//! exercise the exact code the kernel runs — including the cases that are
//! awkward to provoke on real hardware, like an intermediate table that must be
//! released when its last child is unmapped.
//!
//! # The hardware's rules, as data
//!
//! The four levels each hold 512 entries of eight bytes, and each entry
//! indexes a table of 512 further entries. The shifts below are the only
//! places those numbers appear, so a change of page size or level count is a
//! change in one file.
//!
//! # What this crate does *not* do
//!
//! It never touches a page table. It computes *which* table an entry lives in
//! and *which* index within it — the arithmetic — and leaves the reading and
//! writing to the kernel's arch-specific layer, which knows how to reach
//! physical memory.

/// Number of entries in one page table.
///
/// Each entry is eight bytes, so one table is exactly 4 KiB: the same size as
/// a page, which is what lets a page table itself be mapped by a page table.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Mask selecting the nine bits of a virtual address within one table level.
pub const INDEX_MASK: u64 = 0x1FF;

/// Number of significant bits in a virtual address under 4-level paging.
///
/// 48, not 64: bits 63:48 are the sign extension of bit 47 and carry no
/// information of their own.
pub const VIRTUAL_ADDRESS_BITS: u64 = 48;

/// Number of page-table levels.
pub const LEVELS: usize = 4;

/// The four page-table levels of a 4-level x86-64 address split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddressParts {
    /// Top-level (PML4) index.
    pub p4: usize,
    /// Directory (PDPT) index.
    pub p3: usize,
    /// Table (PD) index.
    pub p2: usize,
    /// Page (PT) index.
    pub p1: usize,
}

impl AddressParts {
    /// Reassemble the virtual address these parts describe.
    ///
    /// The low twelve bits are dropped, because that is what a page table
    /// encodes: the hardware supplies the page offset itself. A caller
    /// comparing against an address with a non-zero offset must compare
    /// against the page *base* of it.
    ///
    /// # Why the top index is sign-extended
    ///
    /// With 4-level paging the `p4` index occupies bits 47:39, but bits 48-63
    /// are *not* part of the address the hardware walks — they are the sign
    /// extension of bit 47. A kernel address such as `0xFFFF_8000_0000_0000`
    /// has `p4 == 0x100`, and shifting that by 39 yields `0x80_0000_0000`
    /// rather than the original `0xFFFF_8000_…`. Without the extension below,
    /// every higher-half address fails to round-trip, which is precisely the
    /// direction the kernel actually lives in.
    pub const fn recompose(&self) -> u64 {
        let assembled = ((self.p4 as u64 & INDEX_MASK) << 39)
            | ((self.p3 as u64 & INDEX_MASK) << 30)
            | ((self.p2 as u64 & INDEX_MASK) << 21)
            | ((self.p1 as u64 & INDEX_MASK) << 12);

        // Sign-extend from bit 47: the hardware addresses 48 bits, and the top
        // sixteen bits must replicate bit 47.
        //
        // The mask is `1 << 47`, *not* `VIRTUAL_ADDRESS_BITS - 1` read as a bit
        // count. That looks equivalent and is not: `VIRTUAL_ADDRESS_BITS - 1`
        // is 47, so `u64::MAX >> 47` is the mask of bits 12-16, which asks
        // whether the offset within a 32 KiB region is non-zero. That is a
        // question about page alignment, not about the sign — and its answer is
        // independent of the higher half, so it mistranslates roughly half of
        // all addresses.
        let sign_bit = 1u64 << (VIRTUAL_ADDRESS_BITS - 1);
        if assembled & sign_bit == 0 {
            assembled
        } else {
            // Fill bits 63:VIRTUAL_ADDRESS_BITS with ones.
            assembled | !(sign_bit - 1)
        }
    }
}

/// Decompose a 4-level x86-64 virtual address.
///
/// The low twelve bits are the page offset and are not represented in any
/// index.
#[inline]
pub const fn split_address(address: u64) -> AddressParts {
    AddressParts {
        p4: ((address >> 39) & INDEX_MASK) as usize,
        p3: ((address >> 30) & INDEX_MASK) as usize,
        p2: ((address >> 21) & INDEX_MASK) as usize,
        p1: ((address >> 12) & INDEX_MASK) as usize,
    }
}

/// Extract the page-aligned base of a virtual address.
#[inline]
pub const fn page_base(address: u64) -> u64 {
    address & !(PAGE_SIZE - 1)
}

/// The offset within a 4 KiB page.
#[inline]
pub const fn page_offset(address: u64) -> usize {
    (address & (PAGE_SIZE - 1)) as usize
}

/// Page size in bytes.
pub const PAGE_SIZE: u64 = 0x1000;

/// Whether an address is in canonical form.
///
/// A canonical address is `0..=0x0000_7FFF_FFFF_FFFF` or
/// `0xFFFF_8000_0000_0000..=0xFFFF_FFFF_FFFF_FFFF`. The middle 16 bits must be
/// all-zero or all-one, which is the single most important check before ever
/// dereferencing a pointer.
///
/// # The rule is *replication*, not "all-zero or all-one"
///
/// Bits 63:48 must **replicate bit 47**. That is what the hardware does with
/// the bits above the addressable range, and it is a stricter rule than
/// "bits 63:48 are all-zero or all-one":
///
/// - `0x0000_8000_0000_0000` has an all-zero sign and bit 47 **set**, so the
///   sign does not replicate bit 47. It is **not** canonical — the hardware
///   raises `#GP` for it.
/// - `0xFFFF_0000_0000_0000` has an all-one sign and bit 47 **clear**, so
///   again the sign does not replicate bit 47. It is **not** canonical either.
///
/// The weaker rule — "the sign is all-zero or all-one, whatever bit 47 says" —
/// accepts both of those, and both are unaddressable. It is also *not* merely
/// too permissive: it is wrong in both directions, and the address
/// `0xFFFF_8000_0000_0000` — the canonical form of every higher-half address,
/// including the kernel's own — happens to satisfy it only by accident.
///
/// # Why the mask, and not `address >> 47`
///
/// Comparing `address >> 47` against `0xFFFF` looks equivalent and is not.
/// That shift yields a value in `0..=0xFFFF_FFFF`, and any kernel address
/// whose bits 63:48 are all-one compares unequal to `0xFFFF` — so
/// `0xFFFF_8000_0000_0000`, the canonical form of every higher-half address
/// including the kernel's own, is reported as *non*-canonical.
///
/// That is the worst possible way for this function to fail: it rejects the
/// kernel's entire address range while accepting nothing exotic, so a caller
/// that trusts it concludes the higher half is unmappable and never gets past
/// boot. Extracting exactly bits 63:48 first is what makes the comparison
/// meaningful.
#[inline]
pub const fn is_canonical(address: u64) -> bool {
    is_canonical_in(VIRTUAL_ADDRESS_BITS, address)
}

/// Whether an address is canonical on a `virtual_bits`-wide address space.
///
/// The general form of [`is_canonical`]: the bits above the addressable range
/// must replicate bit `virtual_bits - 1`. A 48-bit space therefore has sixteen
/// sign bits and a 57-bit LA57 space has seven, and both follow from the same
/// expression.
#[inline]
pub const fn is_canonical_in(virtual_bits: u64, address: u64) -> bool {
    let sign_bit = 1u64 << (virtual_bits - 1);
    let sign = address >> virtual_bits;

    if address & sign_bit == 0 {
        // Bit 47 is clear, so the sign must be all-zero.
        sign == 0
    } else {
        // Bit 47 is set, so the sign must be all-one. `u64::MAX >> virtual_bits`
        // is exactly the mask of the sign bits.
        sign == u64::MAX >> virtual_bits
    }
}

/// The level of a page-table walk, counted from the top.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// The top-level table: its entries point at directories.
    Pml4,
    /// The directory: its entries point at tables.
    Pdpt,
    /// The table: its entries point at pages.
    Pd,
    /// The leaf: its entries point at frames.
    Pt,
}

impl Level {
    /// Every level, from the top down.
    pub const ALL: [Level; LEVELS] = [Level::Pml4, Level::Pdpt, Level::Pd, Level::Pt];

    /// The index of this level within a walk.
    pub const fn depth(self) -> usize {
        match self {
            Level::Pml4 => 0,
            Level::Pdpt => 1,
            Level::Pd => 2,
            Level::Pt => 3,
        }
    }

    /// The level *below* this one, or `None` for the leaf.
    pub const fn child(self) -> Option<Level> {
        match self {
            Level::Pml4 => Some(Level::Pdpt),
            Level::Pdpt => Some(Level::Pd),
            Level::Pd => Some(Level::Pt),
            Level::Pt => None,
        }
    }

    /// The index this level selects for `address`.
    pub const fn index_of(self, address: u64) -> usize {
        match self {
            Level::Pml4 => ((address >> 39) & INDEX_MASK) as usize,
            Level::Pdpt => ((address >> 30) & INDEX_MASK) as usize,
            Level::Pd => ((address >> 21) & INDEX_MASK) as usize,
            Level::Pt => ((address >> 12) & INDEX_MASK) as usize,
        }
    }

    /// Whether this level's entries describe pages rather than tables.
    pub const fn is_leaf(self) -> bool {
        matches!(self, Level::Pt)
    }
}

/// The indices a walk through `address` selects, one per level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WalkIndices {
    /// Index within the top-level table.
    pub pml4: usize,
    /// Index within the directory.
    pub pdpt: usize,
    /// Index within the table.
    pub pd: usize,
    /// Index within the leaf table.
    pub pt: usize,
}

impl WalkIndices {
    /// The indices for `address`.
    pub const fn for_address(address: u64) -> Self {
        Self {
            pml4: Level::Pml4.index_of(address),
            pdpt: Level::Pdpt.index_of(address),
            pd: Level::Pd.index_of(address),
            pt: Level::Pt.index_of(address),
        }
    }

    /// The index selected at `level`.
    pub const fn at(self, level: Level) -> usize {
        match level {
            Level::Pml4 => self.pml4,
            Level::Pdpt => self.pdpt,
            Level::Pd => self.pd,
            Level::Pt => self.pt,
        }
    }
}

/// Page-table entry flags.
///
/// The bit assignments are the architectural ones. NovaOS uses bit 9 as
/// "global" — the TLB entry survives a CR3 reload — which the kernel half
/// needs and the user half must not have.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PageFlags(u64);

impl PageFlags {
    /// No flags.
    pub const NONE: PageFlags = PageFlags(0);
    /// Entry is valid.
    pub const PRESENT: PageFlags = PageFlags(1 << 0);
    /// Entry is writable.
    pub const WRITABLE: PageFlags = PageFlags(1 << 1);
    /// Entry is accessible from ring 3.
    pub const USER: PageFlags = PageFlags(1 << 2);
    /// Write-through / cache-disable.
    pub const WRITE_THROUGH: PageFlags = PageFlags(1 << 3);
    /// Disable caching.
    pub const CACHE_DISABLED: PageFlags = PageFlags(1 << 4);
    /// Accessed (set by hardware).
    pub const ACCESSED: PageFlags = PageFlags(1 << 5);
    /// Dirty (set by hardware).
    pub const DIRTY: PageFlags = PageFlags(1 << 6);
    /// Entry points at a huge page rather than a 4 KiB frame.
    pub const HUGE: PageFlags = PageFlags(1 << 7);
    /// Page is global; survives a CR3 write.
    pub const GLOBAL: PageFlags = PageFlags(1 << 8);

    /// Every architecturally defined flag, used to validate external bits.
    pub const RESERVED_MASK: PageFlags =
        PageFlags(0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100);

    /// Raw bit pattern.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Build from raw bits, rejecting anything outside [`PageFlags::RESERVED_MASK`].
    ///
    /// Returning `None` for unknown bits is what keeps a corrupted entry from
    /// being written back with the OS-available bits silently cleared.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::RESERVED_MASK.0 == 0 {
            Some(PageFlags(bits))
        } else {
            None
        }
    }

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: PageFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two flag sets.
    pub const fn union(self, other: PageFlags) -> PageFlags {
        PageFlags(self.0 | other.0)
    }

    /// The intersection of two flag sets.
    pub const fn intersection(self, other: PageFlags) -> PageFlags {
        PageFlags(self.0 & other.0)
    }

    /// The set with every flag in `other` removed.
    ///
    /// The kernel needs this to *narrow* a mapping -- to turn a writable page
    /// into a read-only one, which is how the W^X rule is applied to a page
    /// that was first mapped writable. Narrowing must be expressible without
    /// reaching for the raw bit pattern, because the raw bits are private for
    /// exactly the reason that a caller should not be assembling them by hand.
    pub const fn without(self, other: PageFlags) -> PageFlags {
        PageFlags(self.0 & !other.0)
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = PageFlags;

    /// Not `const`: a trait method cannot be declared `const`, and the
    /// `BitOr` operator is therefore unavailable in a `const` context.
    ///
    /// The compile-time use case that motivated this is served by
    /// [`PageFlags::union`], which *is* a `const fn` and does the same thing.
    /// Callers in a `const` context must use that, not `|`.
    fn bitor(self, rhs: PageFlags) -> PageFlags {
        PageFlags(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for PageFlags {
    fn bitor_assign(&mut self, rhs: PageFlags) {
        self.0 |= rhs.0;
    }
}

/// Encode a page-table entry from a physical address and flags.
///
/// # Safety
///
/// `physical` must be page aligned and point at a real frame.
pub const unsafe fn encode_entry(physical: u64, flags: PageFlags) -> u64 {
    physical | flags.bits()
}

/// Decode the physical frame address from a page-table entry.
///
/// Reserved bits are masked off, so the result is usable even for a huge page.
#[inline]
pub const fn decode_entry(entry: u64) -> u64 {
    entry & 0x000F_FFFF_FFFF_F000
}

/// Decode the flags from a page-table entry.
#[inline]
pub const fn entry_flags(entry: u64) -> PageFlags {
    PageFlags(entry & PageFlags::RESERVED_MASK.0)
}

/// Whether an entry has its present bit set.
#[inline]
pub const fn entry_is_present(entry: u64) -> bool {
    entry & PageFlags::PRESENT.0 != 0
}

/// Whether an entry describes a huge page rather than a 4 KiB frame.
#[inline]
pub const fn entry_is_huge(entry: u64) -> bool {
    entry & PageFlags::HUGE.0 != 0
}
