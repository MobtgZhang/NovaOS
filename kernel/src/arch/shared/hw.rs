//! Address-space geometry, as reported by the architecture.
//!
//! # Why the kernel does not hard-code these
//!
//! The boot protocol already knows the answer — Limine reports a paging mode on
//! x86-64 and a granule size everywhere — and a hard-coded `0x1000` would be
//! wrong on any target with a larger granule. Worse, the *interpretation*
//! differs even where the number matches: on x86-64 a page address is a 4 KiB
//! frame index times 4096, while on AArch64 with 4 KiB granules the same holds,
//! but with 64 KiB granules only the low 16 bits are the offset and the rest
//! identifies a table entry rather than a page.
//!
//! [`AddressSpace`] therefore publishes the granularity plus a small amount of
//! derived arithmetic, and every user of a page size in the kernel goes through
//! it. `mm::frame` is the only place that turns bytes into frame indices, and it
//! asks this module for the shift.

/// The target's address-space geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AddressSpace {
    /// Smallest addressable unit in bytes (page/frame/granule).
    pub page_size: u64,
    /// `log2(page_size)`. Precomputed so the hot frame arithmetic in
    /// `mm::frame` is a shift rather than a division.
    pub page_shift: u32,
    /// Number of bits in a physical address, or `None` if unbounded/unknown.
    ///
    /// This is the ceiling that bounds every physical address the kernel accepts
    /// from the bootloader. x86-64 reports 52 (4-level paging); AArch64 with
    /// LPA2 disabled reports 48.
    pub physical_bits: Option<u32>,
    /// Number of bits in a virtual address, or `None` if the address space is
    /// flat.
    pub virtual_bits: Option<u32>,
}

impl AddressSpace {
    /// Build from a page size, deriving `page_shift` and checking it is sane.
    ///
    /// Returns `None` for a non-power-of-two size, because every downstream
    /// computation (`align_up`, frame indexing, offset masking) assumes a power of
    /// two. Catching it here means the error surfaces at boot rather than as
    /// silent misalignment much later.
    pub const fn new(page_size: u64, physical_bits: Option<u32>, virtual_bits: Option<u32>) -> Option<Self> {
        if page_size == 0 || !page_size.is_power_of_two() {
            return None;
        }
        Some(Self {
            page_size,
            page_shift: page_size.trailing_zeros(),
            physical_bits,
            virtual_bits,
        })
    }

    /// Mask selecting the offset within one page.
    pub const fn page_mask(self) -> u64 {
        self.page_size - 1
    }

    /// Round `value` up to a page boundary.
    ///
    /// Saturating rather than wrapping: this is used on lengths reported by
    /// firmware, and a wrapped `align_up` would produce a length *smaller* than
    /// the input, which is the direction that hides a bug instead of exposing it.
    pub const fn align_up(self, value: u64) -> u64 {
        let masked = value & !self.page_mask();
        if masked < value {
            match masked.checked_add(self.page_size) {
                Some(sum) => sum,
                None => u64::MAX,
            }
        } else {
            masked
        }
    }

    /// Round `value` down to a page boundary.
    pub const fn align_down(self, value: u64) -> u64 {
        value & !self.page_mask()
    }

    /// Whether `address` is a whole number of pages.
    pub const fn is_page_aligned(self, address: u64) -> bool {
        address & self.page_mask() == 0
    }

    /// Highest physical address this target can name, or `u64::MAX` if unbounded.
    ///
    /// The frame allocator and the boot validator both bound what they accept
    /// with this. Returning `u64::MAX` rather than panicking keeps the signature
    /// `const`-friendly and lets the caller decide how to treat "no limit".
    pub const fn max_physical_address(self) -> u64 {
        match self.physical_bits {
            Some(bits) if bits >= 64 => u64::MAX,
            Some(bits) => (1u64 << bits) - 1,
            None => u64::MAX,
        }
    }

    /// First address of the top canonical half, where the kernel image lives.
    ///
    /// A direct map placed *below* this would map physical memory onto kernel
    /// addresses, so boot validation uses this to reject one.
    ///
    /// Derived from `virtual_bits` rather than written as the literal
    /// `0xFFFF_8000_0000_0000` because the canonical half is defined by the
    /// address width: bits 63..width-1 are the sign extension of bit `width - 1`,
    /// so the boundary is the top `(64 - width)` bits set, followed by bit
    /// `width - 1`, followed by zeros. For a 48-bit space that is
    /// `0xFFFF_8000_0000_0000`; for a 57-bit LA57 space it is
    /// `0xFF00_0000_0000_0000`.
    pub const fn kernel_half_start(self) -> u64 {
        match self.virtual_bits {
            // A full 64-bit space has no upper half, so there is no boundary to
            // report; 0 keeps the comparison "offset below the half" trivially
            // false rather than rejecting every machine.
            Some(bits) if bits >= 64 => 0,
            Some(bits) => {
                let top = (1u64 << (64 - bits)) - 1;
                (top << bits) | (1u64 << (bits - 1))
            }
            // A flat address space has no canonical split.
            None => 0,
        }
    }
}