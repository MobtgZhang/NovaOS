//! x86-64 paging: address decomposition, page table entries, and CR access.
//!
//! M0 builds the data structures and the bit-level encoding, and verifies both
//! against known-good answers. It does **not** yet install page tables: CR3 is
//! left on whatever the bootloader configured, because the kernel still runs
//! from the bootloader's identity mapping. M2 builds the recursive kernel page
//! tables and performs the switch.

use core::arch::asm;

use crate::arch::x86_64::port_io::{read_cr0, read_cr2, read_cr3, read_cr4};

/// Page size in bytes.
pub const PAGE_SIZE: u64 = 0x1000;
/// Number of entries in one page table (each entry is 8 bytes).
pub const ENTRIES_PER_TABLE: usize = 512;
/// Mask for selecting the 9 bits of a virtual address within one table level.
pub const INDEX_MASK: u64 = 0x1FF;

/// Number of significant bits in a virtual address under 4-level paging.
///
/// 48, not 64: bits 63:48 are the sign extension of bit 47 and carry no
/// information of their own. A canonical address is therefore reproduced by
/// filling every bit above 47 with the value of bit 47.
pub const VIRTUAL_ADDRESS_BITS: u64 = 48;

/// Flags stored in a page table entry.
///
/// Bits 0-8 are architectural; bits 9-11 are the OS-available bits, which NovaOS
/// uses as: bit 9 = "global" (TLB entry survives a CR3 reload, needed for the
/// higher-half kernel mapping), bit 10 = "accessed", bit 11 = "dirty".
///
/// This is the *same* type as [`nova_core::paging::PageFlags`], re-exported
/// rather than redefined. The flag bits are a property of the architecture, but
/// the policy layer (`nova_core::address_space`) has to speak about them too,
/// and two structs with the same name and the same bits are two types that
/// cannot be passed to each other. Re-exporting makes that duplication
/// impossible rather than merely discouraged.
pub use nova_core::paging::PageFlags;

/// The four page table levels of a 4-level x86-64 address split.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddressParts {
    /// P4 (top-level) index.
    pub p4: usize,
    /// P3 (directory) index.
    pub p3: usize,
    /// P2 (page table) index.
    pub p2: usize,
    /// P1 (page) offset within the 4 KiB frame.
    pub p1: usize,
}

impl AddressParts {
    /// Reassemble the virtual address these parts describe.
    ///
    /// The low 12 bits are dropped, which is exactly what a page table encodes:
    /// the hardware supplies the page offset itself. Callers comparing against
    /// an address with a non-zero offset must compare against
    /// [`page_base`] of it.
    ///
    /// # Why the top index is sign-extended
    ///
    /// With 4-level paging the `p4` index occupies bits 47:39, but bits 48-63
    /// are *not* part of the address the hardware walks -- they are the sign
    /// extension of bit 47. A kernel address such as `0xFFFF_8000_0000_0000`
    /// has `p4 == 0x100`, and shifting that by 39 yields `0x80_0000_0000`
    /// rather than the original `0xFFFF_8000_…`. Without the extension below,
    /// every higher-half address fails to round-trip, which is precisely the
    /// direction the kernel actually lives in.
    ///
    /// Bits 63:48 are therefore reconstructed from bit 47 of the assembled
    /// value. Under LA57 the same expression widens naturally, because the
    /// shift is expressed in terms of [`VIRTUAL_ADDRESS_BITS`].
    pub const fn recompose(&self) -> u64 {
        let assembled = ((self.p4 as u64 & INDEX_MASK) << 39)
            | ((self.p3 as u64 & INDEX_MASK) << 30)
            | ((self.p2 as u64 & INDEX_MASK) << 21)
            | ((self.p1 as u64 & INDEX_MASK) << 12);

        // Sign-extend from bit 47: the hardware addresses 48 bits, and the top
        // 16 bits must replicate bit 47.
        //
        // The mask is `1 << 47`, *not* `VIRTUAL_ADDRESS_BITS - 1` read as a bit
        // count. That looks equivalent and is not: `VIRTUAL_ADDRESS_BITS - 1` is
        // 47, so `u64::MAX >> 47` is the mask of bits 12-16, which asks whether
        // the offset within a 32 KiB region is non-zero. That is a question
        // about page alignment, not about the sign — and its answer is
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
/// The low 12 bits are the page offset and are not represented in any index.
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

/// Extract the offset within a 4 KiB page.
#[inline]
pub const fn page_offset(address: u64) -> usize {
    (address & (PAGE_SIZE - 1)) as usize
}

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
/// the bits above the addressable range, and it is stricter than "the sign is
/// all-zero or all-one":
///
/// - `0x0000_8000_0000_0000` has an all-zero sign and bit 47 **set**: the
///   sign does not replicate bit 47, so the address is **not** canonical and
///   the hardware raises `#GP` for it.
/// - `0xFFFF_0000_0000_0000` has an all-one sign and bit 47 **clear**: again
///   the sign does not replicate bit 47, so it is **not** canonical either.
///
/// The first non-canonical address above the user half is
/// `0x0000_8000_0000_0000`, and the last one below the kernel half is
/// `0xFFFF_7FFF_FFFF_FFFF`.
///
/// # Why the mask, and not `address >> 47`
///
/// Comparing `address >> 47` against `0xFFFF` looks equivalent and is not.
/// That shift yields a value in `0..=0xFFFF_FFFF`, and any kernel address
/// whose bits 63:48 are all-one compares unequal to `0xFFFF` -- so
/// `0xFFFF_8000_0000_0000`, the canonical form of every higher-half address
/// including the kernel's own, is reported as *non*-canonical.
///
/// That is the worst possible way for this function to fail: it rejects the
/// kernel's entire address range while accepting nothing exotic, so a caller
/// that trusts it concludes the higher half is unmappable and never gets past
/// boot. Extracting exactly bits 63:48 first is what makes the comparison
/// meaningful.
///
/// # One rule, one owner
///
/// This is [`nova_core::paging::is_canonical`] rather than a copy of it. The
/// kernel's mapping path, the ELF validator, and the host test suite must all
/// agree about which addresses the hardware can address; three copies of the
/// rule are three chances to disagree.
#[inline]
pub const fn is_canonical(address: u64) -> bool {
    nova_core::paging::is_canonical(address)
}

/// A physical address is canonical as an identity mapping only in the first
/// 52 bits; anything above is not addressable by 4-level paging.
#[inline]
pub const fn is_addressable_physical(address: u64) -> bool {
    address < (1u64 << 52)
}

/// Read the active page directory base.
#[inline]
pub fn current_page_directory() -> u64 {
    // SAFETY: reading CR3 is unprivileged and always safe.
    unsafe { read_cr3() & 0xFFFF_FFFF_FFF0 }
}

/// Read the linear address of the last page fault.
#[inline]
pub fn fault_address() -> u64 {
    // SAFETY: reading CR2 is unprivileged and always safe.
    unsafe { read_cr2() }
}

/// Snapshot of the paging-related control registers, for boot diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct PagingState {
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
}

impl core::fmt::Display for PagingState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "cr0={:#x} cr2={:#x} cr3={:#x} cr4={:#x}",
            self.cr0, self.cr2, self.cr3, self.cr4
        )
    }
}

/// Read every paging control register.
pub fn read_state() -> PagingState {
    // SAFETY: all four reads are unprivileged.
    unsafe {
        PagingState {
            cr0: read_cr0(),
            cr2: read_cr2(),
            cr3: read_cr3(),
            cr4: read_cr4(),
        }
    }
}

/// Whether 5-level paging is currently active.
///
/// This kernel targets 4-level paging, but a bootloader that negotiated 5-level
/// mode would hand us a CR3 with a different structure, so M2 must refuse to
/// install its own tables unless this is false.
#[inline]
pub fn is_5level_enabled() -> bool {
    // Not `const`: it reads a control register, so it can only be evaluated on a
    // live machine. Callers must treat the answer as a boot-time fact, not a
    // compile-time constant.
    // SAFETY: reading CR4 is unprivileged.
    unsafe { read_cr4() & (1 << 12) != 0 }
}

/// Encode a page table entry from a physical address and flags.
///
/// The physical address is masked to the frame bits, so a caller that passes
/// an address with a page offset cannot smuggle those bits into the entry's
/// reserved region. That masking is what makes this function safe: it touches
/// no memory, and the value it returns is a well-formed entry whatever
/// physical address it is handed.
///
/// The *validity* of the physical address -- is it page aligned, does it point
/// at a real frame -- is not something this function can check, and it is not
/// what `unsafe` would buy a caller anyway. A bad frame address produces a
/// page table entry that points nowhere, which the hardware reports as a
/// fault at the point of use. Marking the function `unsafe` would have
/// promised a guarantee it cannot keep, and every call site would have carried
/// a `SAFETY:` comment that could only restate the caller's own care.
pub const fn encode_entry(physical: u64, flags: PageFlags) -> u64 {
    (physical & 0x000F_FFFF_FFFF_F000) | flags.bits()
}

/// Decode the physical frame address from a page table entry.
///
/// Reserved bits are masked off, so the result is usable even for a huge page.
#[inline]
pub const fn decode_entry(entry: u64) -> u64 {
    entry & 0x000F_FFFF_FFFF_F000
}

/// Decode the flags from a page table entry.
///
/// The entry's bits are masked to the architecturally defined ones *before*
/// they are handed to [`PageFlags::from_bits`], so a reserved bit set by
/// hardware cannot make the decode fail. `from_bits` rejects unknown bits
/// precisely so that a corrupted entry is not silently reinterpreted, and the
/// mask is what keeps that rejection from firing on every entry the CPU has
/// touched.
#[inline]
pub const fn entry_flags(entry: u64) -> PageFlags {
    match PageFlags::from_bits(entry & PageFlags::RESERVED_MASK.bits()) {
        Some(flags) => flags,
        None => PageFlags::NONE,
    }
}

/// Whether an entry has its present bit set.
#[inline]
pub const fn entry_is_present(entry: u64) -> bool {
    entry & PageFlags::PRESENT.bits() != 0
}

/// Store an entry into a page table at `index`.
///
/// # Safety
///
/// `table` must point at a page-aligned page table with room for `index + 1`
/// entries, and `entry` must describe a valid frame.
pub unsafe fn write_entry(table: *mut u64, index: usize, entry: u64) {
    debug_assert!(index < ENTRIES_PER_TABLE);
    // SAFETY: delegated to this function's contract.
    unsafe {
        *table.add(index) = entry;
    }
}

/// Read an entry from a page table at `index`.
///
/// # Safety
///
/// `table` must point at a page-aligned page table with at least `index + 1`
/// entries.
#[inline]
pub unsafe fn read_entry(table: *const u64, index: usize) -> u64 {
    debug_assert!(index < ENTRIES_PER_TABLE);
    // SAFETY: delegated to this function's contract.
    unsafe { *table.add(index) }
}

/// Execute `invlpg` for one virtual address.
///
/// Invalidating a single entry rather than a full TLB flush matters once the
/// kernel page tables are installed: a global `invlpg` on the HHDM would throw
/// away thousands of hot entries.
///
/// # Safety
///
/// `virtual_address` must be a valid canonical address for the active tables.
#[inline]
pub unsafe fn invlpg(virtual_address: u64) {
    // SAFETY: delegated to this function's contract.
    unsafe {
        asm!("invlpg [{0}]", in(reg) virtual_address, options(readonly, nostack, preserves_flags));
    }
}

/// Reload CR3, invalidating non-global TLB entries.
///
/// # Safety
///
/// `page_directory` must be the physical base of a page table that maps every
/// address currently in use, including this function's own stack.
#[inline]
pub unsafe fn load_page_directory(page_directory: u64) {
    use crate::arch::x86_64::port_io::write_cr3;

    // SAFETY: delegated to this function's contract.
    unsafe { write_cr3(page_directory) };
}
