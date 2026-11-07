//! x86-64 page-layout math shared by both sides of the boot hand-off.
//!
//! The boot manager builds the early page tables and the kernel validates and
//! rebuilds them, so the address decomposition and the leaf-size rule must be
//! *the same code* on both sides — a disagreement would be a hand-off failure
//! no single side could diagnose. This module is therefore free of `uefi` and
//! of kernel types: it compiles for the host, for UEFI, and inside the kernel,
//! and its tests run in the ordinary host suite.

/// Page size in bytes.
pub const PAGE_SIZE: u64 = 0x1000;

/// 4 KiB, 2 MiB, and 1 GiB in bytes — the three leaf sizes.
pub const P1_PAGE: u64 = PAGE_SIZE;
pub const P2_PAGE: u64 = 0x20_0000;
pub const P3_PAGE: u64 = 0x4000_0000;

/// The page-size bit (PS): set on an entry to make it a *leaf* of the next
/// size down rather than a table pointer. On a PDPT entry it means a 1 GiB
/// leaf; on a PD entry, a 2 MiB leaf.
pub const PS: u64 = 0x80;

/// The leaf size to map at `addr` with `left` bytes remaining in the region.
///
/// 1 GiB when the CPU can form such leaves and `addr` is 1 GiB aligned with at
/// least 1 GiB left; otherwise 2 MiB under the same rule at the 2 MiB level;
/// otherwise 4 KiB. The rule is alignment-driven, so a region's unaligned head
/// and tail cost a few 4 KiB leaves and its body costs one entry per 2 MiB or
/// 1 GiB.
///
/// The HHDM offset is 1 GiB aligned, so `addr`'s alignment is the virtual
/// address's alignment and one check per level is enough.
pub const fn chunk_size(addr: u64, left: u64, one_gib_pages: bool) -> u64 {
    if one_gib_pages && addr.is_multiple_of(P3_PAGE) && left >= P3_PAGE {
        return P3_PAGE;
    }
    if addr.is_multiple_of(P2_PAGE) && left >= P2_PAGE {
        return P2_PAGE;
    }
    P1_PAGE
}

/// Whether the CPU can form 1 GiB leaves (`CPUID 0x80000001`, `EDX` bit 26,
/// `PDPE1GB`).
///
/// 2 MiB leaves are part of every long-mode implementation; 1 GiB leaves are
/// an optional extension, so the window builders ask rather than assuming.
/// A `false` here only costs table frames — the mapping is still correct.
///
/// The kernel links this crate with `default-features = false`, and the boot
/// manager links it as it is; the intrinsic is available on both x86-64
/// targets. Other architectures are answered `false` — a caller there is
/// building tables for a different ISA and never consults this answer.
#[inline]
#[cfg(target_arch = "x86_64")]
pub fn has_1gib_pages() -> bool {
    // `cpuid` writes only its four documented result registers, and this
    // toolchain exposes it as a safe intrinsic.
    let extended = core::arch::x86_64::__cpuid(0x8000_0000);
    if extended.eax < 0x8000_0001 {
        return false;
    }
    let features = core::arch::x86_64::__cpuid(0x8000_0001);
    features.edx & (1 << 26) != 0
}

/// Non-x86-64 targets have no `PDPE1GB` to probe.
#[inline]
#[cfg(not(target_arch = "x86_64"))]
pub fn has_1gib_pages() -> bool {
    false
}

/// Decompose a 4-level virtual address into its indices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddressParts {
    /// PML4 index (bits 47:39).
    pub p4: usize,
    /// Directory-pointer index (bits 38:30).
    pub p3: usize,
    /// Directory index (bits 29:21).
    pub p2: usize,
    /// Page-table index (bits 20:12).
    pub p1: usize,
}

/// Bits of a virtual address that select one entry within a table.
const INDEX_MASK: u64 = 0x1FF;

impl AddressParts {
    /// Decompose `address`.
    pub const fn from(address: u64) -> Self {
        Self {
            p4: ((address >> 39) & INDEX_MASK) as usize,
            p3: ((address >> 30) & INDEX_MASK) as usize,
            p2: ((address >> 21) & INDEX_MASK) as usize,
            p1: ((address >> 12) & INDEX_MASK) as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parts_hhdm_kernel() {
        let hhdm = AddressParts::from(0xFFFF_8000_0000_1000);
        assert_eq!(hhdm.p4, 256);
        assert_eq!(hhdm.p3, 0);
        assert_eq!(hhdm.p2, 0);
        assert_eq!(hhdm.p1, 1);

        let kernel = AddressParts::from(0xFFFF_FFFF_8010_0000);
        // The kernel window sits 4 GiB below the top of the canonical half:
        // `0xFFFFFFFF80000000` makes PML4[511] and PDPT[510] the two levels
        // that name the window, and the image's first megabyte lands in
        // directory 0, page 256.
        assert_eq!(kernel.p4, 511);
        assert_eq!(kernel.p3, 510);
        assert_eq!(kernel.p2, 0);
        assert_eq!(kernel.p1, 256);
    }

    #[test]
    fn an_aligned_body_uses_the_largest_leaf() {
        // 2 GiB into a region that starts 1 GiB aligned: the first step is a
        // 1 GiB leaf and the second one too.
        assert_eq!(chunk_size(0x4000_0000, 2 * P3_PAGE, true), P3_PAGE);
        assert_eq!(chunk_size(0, P3_PAGE, true), P3_PAGE);
    }

    #[test]
    fn an_unaligned_head_steps_in_4k_pages() {
        // One page past a 2 MiB boundary: nothing bigger fits.
        assert_eq!(chunk_size(P2_PAGE + PAGE_SIZE, 4 * P3_PAGE, true), P1_PAGE);
        // One page short of a full 2 MiB: same answer, from the length side.
        assert_eq!(chunk_size(0, P2_PAGE - PAGE_SIZE, true), P1_PAGE);
    }

    #[test]
    fn without_1gib_support_two_mib_leaves_fill_the_body() {
        assert_eq!(chunk_size(0, 4 * P3_PAGE, false), P2_PAGE);
        assert_eq!(chunk_size(P2_PAGE, P3_PAGE, false), P2_PAGE);
        // But a 1 GiB-aligned start with 1 GiB left still only steps 2 MiB.
        assert_eq!(chunk_size(0, P3_PAGE, false), P2_PAGE);
    }

    #[test]
    fn the_tail_of_a_region_is_4k_pages() {
        // 3 MiB left from a 2 MiB boundary: one 2 MiB leaf, then 1 MiB of
        // 4 KiB pages. The second step sees only the remainder.
        const MIB: u64 = 1024 * 1024;
        let first = chunk_size(0, 3 * MIB, false);
        assert_eq!(first, P2_PAGE);
        let second = chunk_size(P2_PAGE, 3 * MIB - P2_PAGE, false);
        assert_eq!(second, P1_PAGE);
    }

    #[test]
    fn a_fully_covered_last_page_is_4k() {
        // The final page of a region is `last` itself; `left` is one page and
        // nothing larger may run past the region's end.
        assert_eq!(chunk_size(0x3000_0000, PAGE_SIZE, true), P1_PAGE);
    }
}
