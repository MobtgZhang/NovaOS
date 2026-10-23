//! The validated snapshot of everything the kernel learns from the bootloader.
//!
//! # Architecture neutrality
//!
//! This file has no architecture-specific code at all, and that is deliberate.
//! It is the *only* representation of firmware facts the rest of the kernel
//! sees: no bootloader pointers survive past [`crate::boot::capture`], and no
//! caller ever interprets a Limine type constant directly. A field added for
//! AArch64 (say, the device tree pointer) lives here next to the x86-64 HHDM
//! offset rather than in a per-architecture struct, because the frame allocator
//! and the address-space code should not have to know which target they run on.

/// Page size in bytes. Every physical address the kernel handles is page aligned.
pub const PAGE_SIZE: u64 = 0x1000;

/// Sentinel for a memory map entry's type: ordinary allocatable RAM.
pub const MEMMAP_USABLE: u64 = 0;

/// How many memory map entries [`BootInfo`] keeps inline.
///
/// Firmware reports well under this on any real machine. The cap is what stops a
/// malformed response from being copied unboundedly into kernel memory.
pub const MAX_MEMMAP_ENTRIES: usize = 128;

/// Upper bound on simultaneously reported validation problems.
///
/// Boot is fatal on any problem, so this only bounds how much detail is printed
/// before the kernel panics.
pub const MAX_PROBLEMS: usize = 12;

/// Length of the fixed buffers holding bootloader name and version.
const NAME_LEN: usize = 32;

/// Physical memory region, normalised from the bootloader's map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// First physical byte of the region, guaranteed page aligned.
    pub base: u64,
    /// Length in bytes. Always non-zero for entries the kernel keeps.
    pub length: u64,
    /// Bootloader memory type (`MEMMAP_*`). Compared against
    /// [`MEMMAP_USABLE`] rather than a Limine constant so that callers outside
    /// the adapter need not depend on the crate.
    pub type_: u64,
}

impl MemoryRegion {
    /// One past the last physical byte.
    ///
    /// Callers must have checked `base + length` for overflow; that check lives
    /// in [`BootInfo::validate`].
    pub const fn end(&self) -> u64 {
        self.base + self.length
    }

    /// Whether this region can be handed to the frame allocator.
    pub const fn is_usable(&self) -> bool {
        self.type_ == MEMMAP_USABLE
    }
}

/// A single validation failure.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// The bootloader reported no protocol revision.
    NoProtocolRevision,
    /// The bootloader reported no memory map at all.
    EmptyMemmap,
    /// The direct-map offset was zero or unaligned.
    BadDirectMapOffset,
    /// An entry's base was not page aligned.
    UnalignedEntryBase,
    /// An entry had zero length.
    ZeroLengthEntry,
    /// `base + length` overflowed the address space.
    EntryOverflow,
    /// An entry extended past the target's addressable physical limit.
    EntryPastAddressLimit,
    /// An entry exceeded the inline capacity.
    TooManyEntries,
    /// No region of any type was reported.
    NoRegions,
    /// Regions were present but none were allocatable.
    NoUsableRegions,
}

impl Problem {
    /// Human-readable description for the serial log.
    pub const fn describe(self) -> &'static str {
        match self {
            Problem::NoProtocolRevision => "bootloader reported no protocol revision",
            Problem::EmptyMemmap => "memory map contains no entries",
            Problem::BadDirectMapOffset => "direct map offset is zero or not page aligned",
            Problem::UnalignedEntryBase => "memory map entry base is not page aligned",
            Problem::ZeroLengthEntry => "memory map entry has zero length",
            Problem::EntryOverflow => "memory map entry overflows the address space",
            Problem::EntryPastAddressLimit => "memory map entry extends past the addressable limit",
            Problem::TooManyEntries => "memory map exceeded the inline capacity",
            Problem::NoRegions => "memory map reports no regions at all",
            Problem::NoUsableRegions => "memory map reports zero allocatable bytes",
        }
    }
}

/// Outcome of validating a [`BootInfo`] snapshot.
#[derive(Clone, Copy)]
pub struct BootValidation {
    /// Count of invariant violations; the problem table holds the detail.
    pub problem_count: usize,
    /// Descriptions of each violation found, in discovery order.
    pub problems: [Problem; MAX_PROBLEMS],
    /// Number of non-monotonic (out-of-order) entries. Not fatal on its own:
    /// firmware sometimes lists overlapping regions.
    pub out_of_order: usize,
}

impl Default for BootValidation {
    fn default() -> Self {
        Self {
            problem_count: 0,
            problems: [Problem::EmptyMemmap; MAX_PROBLEMS],
            out_of_order: 0,
        }
    }
}

impl BootValidation {
    /// Whether the snapshot is coherent.
    pub const fn is_ok(&self) -> bool {
        self.problem_count == 0
    }
}

/// Validated snapshot of everything the kernel learns from the bootloader.
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// Boot protocol revision actually in effect.
    pub protocol_revision: u64,
    /// Bootloader name, NUL-padded.
    pub(crate) bootloader_name: [u8; NAME_LEN],
    /// Bootloader version, NUL-padded.
    pub(crate) bootloader_version: [u8; NAME_LEN],
    /// Direct-map offset: `virtual = physical + direct_map_offset`.
    ///
    /// The x86-64 bootloader supplies this as the HHDM; the AArch64 bootloader as
    /// the equivalent higher-half mapping. Both are named the same way here so
    /// `mm` does not branch on the target.
    pub direct_map_offset: u64,
    /// Validated memory map.
    pub memmap: [MemoryRegion; MAX_MEMMAP_ENTRIES],
    /// Number of populated entries in `memmap`.
    pub memmap_len: usize,
}

impl BootInfo {
    /// An all-zero snapshot, used to initialise the storage before capture.
    pub const fn empty() -> Self {
        Self {
            protocol_revision: 0,
            bootloader_name: [0; NAME_LEN],
            bootloader_version: [0; NAME_LEN],
            direct_map_offset: 0,
            memmap: [MemoryRegion {
                base: 0,
                length: 0,
                type_: MEMMAP_USABLE,
            }; MAX_MEMMAP_ENTRIES],
            memmap_len: 0,
        }
    }

    /// The populated regions.
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.memmap[..self.memmap_len.min(MAX_MEMMAP_ENTRIES)]
    }

    /// Total allocatable bytes across all usable regions.
    pub fn usable_bytes(&self) -> u64 {
        self.regions()
            .iter()
            .filter(|region| region.is_usable())
            .fold(0u64, |total, region| total.saturating_add(region.length))
    }

    /// First physical address above every reported region.
    pub fn physical_limit(&self) -> u64 {
        self.regions().iter().fold(0u64, |limit, region| {
            let end = region.end();
            if end > limit { end } else { limit }
        })
    }

    /// The bootloader name as a NUL-trimmed string, or `"unknown"`.
    pub fn bootloader_name(&self) -> &str {
        trimmed(&self.bootloader_name)
    }

    /// The bootloader version as a NUL-trimmed string, or `"unknown"`.
    pub fn bootloader_version(&self) -> &str {
        trimmed(&self.bootloader_version)
    }

    /// Check every invariant the rest of the kernel relies on.
    ///
    /// Separated from capture so the host-side test suite can run the same checks
    /// over synthetic maps, on either architecture.
    pub fn validate(&self, max_physical_address: u64) -> BootValidation {
        let mut result = BootValidation::default();

        // A plain closure rather than a method: `record` needs to mutate
        // `result`, which is still borrowed by the loop below, so it takes the
        // target as a parameter instead of capturing it.
        let record = |result: &mut BootValidation, problem: Problem| {
            if result.problem_count < MAX_PROBLEMS {
                result.problems[result.problem_count] = problem;
            }
            result.problem_count += 1;
        };

        if self.protocol_revision == 0 {
            record(&mut result, Problem::NoProtocolRevision);
        }

        if self.direct_map_offset == 0 || self.direct_map_offset % PAGE_SIZE != 0 {
            record(&mut result, Problem::BadDirectMapOffset);
        }

        if self.memmap_len == 0 {
            record(&mut result, Problem::EmptyMemmap);
        }

        if self.memmap_len > MAX_MEMMAP_ENTRIES {
            record(&mut result, Problem::TooManyEntries);
        }

        let mut highest_end = 0u64;
        let mut has_usable = false;

        for region in self.regions() {
            match region.base.checked_add(region.length) {
                Some(end) if end > region.base => {}
                _ => {
                    record(&mut result, Problem::EntryOverflow);
                    continue;
                }
            }

            if region.base % PAGE_SIZE != 0 {
                record(&mut result, Problem::UnalignedEntryBase);
            }

            if region.length == 0 {
                record(&mut result, Problem::ZeroLengthEntry);
            }

            // The bound comes from the target's address space, not a constant:
            // 4-level x86-64 tops out at 52 physical bits, AArch64 at 48 with
            // LPA2 off. Hard-coding either would reject valid maps on the other.
            if region.end() > max_physical_address {
                record(&mut result, Problem::EntryPastAddressLimit);
            }

            if region.is_usable() {
                has_usable = true;
            }

            // Out-of-order entries are legal but worth counting: they usually
            // signal we copied the wrong array.
            if region.base < highest_end {
                result.out_of_order += 1;
            }
            highest_end = highest_end.max(region.end());
        }

        if self.memmap_len > 0 && !has_usable {
            record(&mut result, Problem::NoUsableRegions);
        }

        result
    }
}

/// Borrow the leading run of non-NUL bytes as a string.
fn trimmed(bytes: &[u8; NAME_LEN]) -> &str {
    let len = bytes.iter().position(|&byte| byte == 0).unwrap_or(NAME_LEN);
    core::str::from_utf8(&bytes[..len]).unwrap_or("unknown")
}

/// The highest physical address this build accepts from firmware.
///
/// Derived from the compiled-in address space rather than declared per
/// architecture, so a new target cannot forget to bound its inputs.
pub const MAX_PHYSICAL_ADDRESS: u64 = {
    // `ADDRESS_SPACE` is a const, but computing this by hand keeps the bound
    // visible in one place regardless of which backend is compiled.
    let bits = if cfg!(target_arch = "x86_64") {
        52
    } else if cfg!(target_arch = "aarch64") {
        48
    } else {
        52
    };
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
};

#[cfg(test)]
mod tests {
    use super::{
        BootInfo, MAX_MEMMAP_ENTRIES, MAX_PHYSICAL_ADDRESS, MEMMAP_USABLE, MemoryRegion, PAGE_SIZE,
        Problem,
    };

    /// A snapshot describing one 64 MiB usable region, which is what every test
    /// starts from unless it is specifically exercising a malformed map.
    fn sample() -> BootInfo {
        let mut info = BootInfo::empty();
        info.protocol_revision = 6;
        info.direct_map_offset = 0xFFFF_8000_0000_0000;
        info.memmap[0] = MemoryRegion {
            base: 0x10_0000,
            length: 64 * 1024 * 1024,
            type_: MEMMAP_USABLE,
        };
        info.memmap_len = 1;
        info
    }

    #[test]
    fn a_well_formed_map_validates() {
        let validation = sample().validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.is_ok(), "unexpected problems: {:?}", validation.problems);
        assert_eq!(validation.out_of_order, 0);
    }

    #[test]
    fn a_missing_protocol_revision_is_a_problem() {
        let mut info = sample();
        info.protocol_revision = 0;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count].contains(&Problem::NoProtocolRevision));
    }

    #[test]
    fn a_zero_direct_map_offset_is_a_problem() {
        let mut info = sample();
        info.direct_map_offset = 0;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::BadDirectMapOffset));
    }

    #[test]
    fn a_misaligned_direct_map_offset_is_a_problem() {
        let mut info = sample();
        info.direct_map_offset = 0xFFFF_8000_0000_1000 + 1;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::BadDirectMapOffset));
    }

    #[test]
    fn an_empty_map_is_a_problem() {
        let mut info = sample();
        info.memmap_len = 0;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count].contains(&Problem::EmptyMemmap));
    }

    #[test]
    fn an_overfull_map_is_a_problem() {
        let mut info = sample();
        info.memmap_len = MAX_MEMMAP_ENTRIES + 1;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::TooManyEntries));
    }

    #[test]
    fn an_unaligned_base_is_a_problem() {
        let mut info = sample();
        info.memmap[0].base = 0x10_0001;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::UnalignedEntryBase));
    }

    #[test]
    fn a_zero_length_entry_is_a_problem() {
        let mut info = sample();
        info.memmap[0].length = 0;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(
            validation.problems[..validation.problem_count].contains(&Problem::ZeroLengthEntry),
            "a zero-length entry has nothing to allocate",
        );
    }

    #[test]
    fn an_overflowing_entry_is_rejected() {
        let mut info = sample();
        info.memmap[0].base = 0x1000;
        info.memmap[0].length = u64::MAX;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count].contains(&Problem::EntryOverflow));
    }

    #[test]
    fn an_entry_past_the_address_limit_is_rejected() {
        let mut info = sample();
        // The bound comes from the caller's architecture, which is the whole
        // point of threading it through rather than hard-coding 52 bits.
        info.memmap[0].base = 0x1000;
        info.memmap[0].length = 0x10_0000;
        let validation = info.validate(0x2000);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::EntryPastAddressLimit));
    }

    #[test]
    fn a_map_with_no_usable_region_is_a_problem() {
        let mut info = sample();
        info.memmap[0].type_ = 1; // reserved
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.problems[..validation.problem_count]
            .contains(&Problem::NoUsableRegions));
    }

    #[test]
    fn out_of_order_entries_are_counted_not_fatal() {
        // Firmware legitimately lists overlapping regions; counting them is a
        // diagnostic, and treating it as fatal would reject valid machines.
        let mut info = sample();
        info.memmap[1] = MemoryRegion {
            base: 0x1000,
            length: 0x1000,
            type_: MEMMAP_USABLE,
        };
        info.memmap_len = 2;
        let validation = info.validate(MAX_PHYSICAL_ADDRESS);
        assert!(validation.is_ok());
        assert_eq!(validation.out_of_order, 1);
    }

    #[test]
    fn every_problem_has_a_description() {
        // The reporter prints `describe()`; a `Missing` arm would print nothing
        // and look like a kernel that died mid-message.
        let all = [
            Problem::NoProtocolRevision,
            Problem::EmptyMemmap,
            Problem::BadDirectMapOffset,
            Problem::UnalignedEntryBase,
            Problem::ZeroLengthEntry,
            Problem::EntryOverflow,
            Problem::EntryPastAddressLimit,
            Problem::TooManyEntries,
            Problem::NoRegions,
            Problem::NoUsableRegions,
        ];
        for problem in all {
            assert!(!problem.describe().is_empty());
        }
    }

    #[test]
    fn usable_bytes_sums_only_usable_regions() {
        let mut info = sample();
        info.memmap[1] = MemoryRegion {
            base: 0x1000,
            length: 0x2000,
            type_: 1, // reserved: must not count
        };
        info.memmap_len = 2;
        assert_eq!(info.usable_bytes(), 64 * 1024 * 1024);
    }

    #[test]
    fn physical_limit_is_the_highest_region_end() {
        let mut info = sample();
        info.memmap[1] = MemoryRegion {
            base: 0x8_0000_0000,
            length: 0x1000,
            type_: MEMMAP_USABLE,
        };
        info.memmap_len = 2;
        assert_eq!(info.physical_limit(), 0x8_0000_1000);
    }

    #[test]
    fn regions_are_clamped_to_the_inline_capacity() {
        // `memmap_len` is trusted enough to iterate, but not trusted to index
        // past the array: a corrupted count must not read out of bounds.
        let mut info = sample();
        info.memmap_len = MAX_MEMMAP_ENTRIES * 4;
        assert_eq!(info.regions().len(), MAX_MEMMAP_ENTRIES);
    }

    #[test]
    fn names_are_nul_trimmed_and_default_to_unknown() {
        let mut info = sample();
        assert_eq!(info.bootloader_name(), "");
        info.bootloader_name[..3].copy_from_slice(b"Lim");
        assert_eq!(info.bootloader_name(), "Lim");

        // Invalid UTF-8 must not panic: it degrades to "unknown" so a
        // misconfigured bootloader cannot stop the kernel from booting.
        info.bootloader_name[0] = 0xFF;
        info.bootloader_name[1] = 0;
        assert_eq!(info.bootloader_name(), "unknown");
    }

    #[test]
    fn page_size_is_the_documented_value() {
        assert_eq!(PAGE_SIZE, 0x1000);
    }
}