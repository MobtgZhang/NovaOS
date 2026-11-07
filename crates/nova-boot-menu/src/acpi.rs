//! ACPI table walk for processor discovery.
//!
//! The MP hand-over needs one fact the firmware owns and the boot manager is
//! the only party left to read: *which* application processors exist, and what
//! their local APIC identifiers are. On x86-64 that answer lives in the MADT —
//! the "APIC" table inside the RSDT/XSDT, reachable from the RSDP that the
//! firmware publishes in the UEFI configuration table.
//!
//! # Why the boot manager walks it
//!
//! The kernel could walk ACPI itself — it has a direct map once `mm` is up.
//! But the snapshot is the *boot boundary*: whatever the kernel needs at
//! `capture` time must already be in it, because the loader's pages are
//! reclaimed the moment control transfers. Enumerating the processors once,
//! here, and publishing plain `u32`s in the snapshot keeps the kernel's boot
//! path free of any firmware-table parsing.
//!
//! # Why raw pointers
//!
//! The configuration table's entries are firmware-owned physical addresses.
//! UEFI identity-maps the lower physical range while boot services are active,
//! so those addresses are directly dereferenceable here — and this module runs
//! strictly before `ExitBootServices`, which is exactly when they are valid.
//! Every dereference is bounds-checked against the table's own `length` field
//! and validated by the ACPI checksums before any data is trusted.

use core::arch::x86_64::__cpuid;

use uefi::data_types::Guid;
use uefi::table::Boot;
use uefi::table::SystemTable;
use uefi::table::cfg::ACPI2_GUID;

use nova_boot::MAX_CPUS;

/// The ACPI 2.0 configuration-table GUID: the entry whose address is the RSDP
/// of a revision-2 (XSDT-carrying) firmware.
///
/// `8868e871-e4f1-11d3-bc22-0080c73c8881` (ACPI 6.5 §5.2.5.2). The easy-to-
/// guess `eb9d2d30-…` is the ACPI *1.0* sibling, and an RSDP found through it
/// cannot be trusted to carry an XSDT, so this walk does not fall back to it.
const ACPI_20_GUID: Guid = ACPI2_GUID;

/// The processors ACPI described, in MADT order.
#[derive(Clone, Copy, Debug)]
pub struct ProcessorTable {
    /// Enabled processor count, `1..=MAX_CPUS`.
    pub count: u16,
    /// Local APIC identifiers, `ids[0..count]` populated.
    pub ids: [u32; MAX_CPUS],
    /// The local APIC identifier of *this* processor — the boot manager only
    /// ever runs on the bootstrap processor, so CPUID's answer is authoritative.
    pub bsp_lapic_id: u32,
}

impl ProcessorTable {
    /// The single-processor fallback for firmware with no ACPI tables.
    ///
    /// The bootstrap processor's own identity still comes from CPUID, so the
    /// kernel's `cpu_index_of_lapic_id` finds the slot even in this fallback.
    pub fn single() -> Self {
        Self {
            count: 1,
            ids: {
                let mut ids = [0u32; MAX_CPUS];
                ids[0] = bsp_lapic_id();
                ids
            },
            bsp_lapic_id: bsp_lapic_id(),
        }
    }
}

/// This processor's local APIC identifier, from CPUID leaf 1.
fn bsp_lapic_id() -> u32 {
    // `cpuid` writes only its four documented result registers, and this
    // toolchain exposes it as a safe intrinsic.
    __cpuid(1).ebx >> 24
}

/// Why no processor table could be built from the firmware's ACPI.
#[derive(Clone, Copy, Debug)]
pub enum DiscoveryError {
    /// The configuration table has no ACPI 2.0 RSDP entry.
    NoRsdpEntry,
    /// The RSDP's checksums or revision field are not trustworthy.
    BadRsdp,
    /// The XSDT's header or checksum is not trustworthy.
    BadXsdt,
    /// The XSDT carries no "APIC" table.
    NoMadt,
    /// The MADT listed no enabled processor.
    NoEnabledProcessors,
    /// The MADT did not list the bootstrap processor itself.
    BspMissing,
}

impl DiscoveryError {
    /// A short, boot-log-friendly name.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::NoRsdpEntry => "no-rsdp-entry",
            Self::BadRsdp => "bad-rsdp",
            Self::BadXsdt => "bad-xsdt",
            Self::NoMadt => "no-madt",
            Self::NoEnabledProcessors => "no-enabled-processors",
            Self::BspMissing => "bsp-missing",
        }
    }
}

/// Enumerate the enabled processors from the firmware's ACPI tables.
///
/// An error is a reason the caller should fall back to
/// [`ProcessorTable::single`] rather than a reason to fail the boot: a
/// machine whose firmware publishes no usable ACPI tables is still a
/// single-processor machine, which is exactly what the fallback describes.
pub fn discover(system_table: &SystemTable<Boot>) -> Result<ProcessorTable, DiscoveryError> {
    let entries = system_table.config_table();
    let rsdp = entries
        .iter()
        .find(|entry| entry.guid == ACPI_20_GUID)
        .ok_or(DiscoveryError::NoRsdpEntry)?
        .address;
    let xsdt = unsafe { rsdp_xsdt(rsdp.cast::<u8>()) }.ok_or(DiscoveryError::BadRsdp)?;
    let mut table = ProcessorTable::single();
    table.count = 0;
    unsafe { collect_madt_processors(xsdt, &mut table) }?;
    if table.count == 0 {
        return Err(DiscoveryError::NoEnabledProcessors);
    }
    // The bootstrap processor must be among the enabled ones: it is the CPU
    // executing this code, and the MADT always lists it. A MADT that omits it
    // is not one this walk can trust.
    if !table.ids[..table.count as usize].contains(&table.bsp_lapic_id) {
        return Err(DiscoveryError::BspMissing);
    }
    Ok(table)
}

/// Validate the RSDP at `pointer` and return its XSDT address.
///
/// # Safety
///
/// `pointer` must be a readable firmware address naming an RSDP — the
/// guarantee the configuration table's entry carries.
unsafe fn rsdp_xsdt(pointer: *const u8) -> Option<u64> {
    let base = pointer.cast::<u8>();
    // The RSDP's first 20 bytes are revision 1's extent; the extended fields
    // from 20 on exist only when the revision says so.
    let signature = unsafe { read_bytes(base, 8) };
    if &signature[..8] != b"RSD PTR " {
        return None;
    }
    let revision = unsafe { *base.add(15) };
    if revision < 2 {
        return None;
    }
    let length = unsafe { read_u32_le(base.add(20)) };
    if !(20..=36).contains(&length) {
        return None;
    }
    // Two checksums: the first 20 bytes for every reader, and the full
    // structure for a revision-2 reader. Either failing means the firmware's
    // table is corrupt in a way no later field is safe to trust.
    unsafe {
        if checksum(base, 20) != 0 || checksum(base, length as usize) != 0 {
            return None;
        }
    }
    Some(unsafe { read_u64_le(base.add(24)) })
}

/// Collect the enabled local APIC identifiers from the XSDT's MADT.
///
/// # Safety
///
/// `xsdt` must be a readable XSDT address, as produced by [`rsdp_xsdt`].
unsafe fn collect_madt_processors(
    xsdt: u64,
    table: &mut ProcessorTable,
) -> Result<(), DiscoveryError> {
    let base = (xsdt as usize) as *const u8;
    let signature = unsafe { read_bytes(base, 4) };
    if &signature[..4] != b"XSDT" {
        return Err(DiscoveryError::BadXsdt);
    }
    let length = unsafe { read_u32_le(base.add(4)) } as usize;
    if !(36..=1024 * 1024).contains(&length) {
        return Err(DiscoveryError::BadXsdt);
    }
    unsafe {
        if checksum(base, length) != 0 {
            return Err(DiscoveryError::BadXsdt);
        }
    }

    // The entries are 8-byte physical addresses from offset 36 (after the
    // common header) to `length`.
    let entry_count = length.saturating_sub(36) / 8;
    for index in 0..entry_count {
        let entry = unsafe { read_u64_le(base.add(36 + index * 8)) };
        if entry == 0 {
            continue;
        }
        // A failure inside one table is not a reason to stop: the walk keeps
        // looking for the MADT among the remaining entries.
        if unsafe { try_madt(entry, table) }.is_some() && table.count != 0 {
            return Ok(());
        }
    }
    Err(DiscoveryError::NoMadt)
}

/// Read one ACPI table and, when it is the MADT, record its enabled
/// processors into `table`.
///
/// # Safety
///
/// `address` must be a readable ACPI table address.
unsafe fn try_madt(address: u64, table: &mut ProcessorTable) -> Option<()> {
    let base = (address as usize) as *const u8;
    let signature = unsafe { read_bytes(base, 4) };
    if &signature[..4] != b"APIC" {
        return None;
    }
    let length = unsafe { read_u32_le(base.add(4)) } as usize;
    // The MADT's fixed header is 44 bytes: the 36-byte common header plus the
    // local-interrupt-controller address and the global flags. Anything
    // shorter carries no entries.
    if length < 44 {
        return None;
    }
    unsafe {
        if checksum(base, length) != 0 {
            return None;
        }
    }

    // Each entry is `type`, `length`, then the type's payload.
    let mut cursor = 44usize;
    while cursor + 2 <= length {
        let entry_type = unsafe { *base.add(cursor) };
        let entry_length = unsafe { *base.add(cursor + 1) } as usize;
        if entry_length < 2 || cursor + entry_length > length {
            return None;
        }
        if entry_type == 0 && entry_length >= 8 {
            // The record is: type, length, ACPI processor id (1 byte), the
            // local APIC id this walk is after (1 byte), and the four flag
            // bytes whose low bit says the processor is enabled.
            let id = unsafe { *base.add(cursor + 3) } as u32;
            let flags = unsafe { read_u32_le(base.add(cursor + 4)) };
            let enabled = flags & 1 != 0;
            if enabled && (table.count as usize) < MAX_CPUS {
                table.ids[table.count as usize] = id;
                table.count += 1;
            }
        }
        cursor += entry_length;
    }
    Some(())
}

/// Sum `length` bytes at `base`, truncated to a byte — the ACPI checksum rule.
///
/// # Safety
///
/// `base..base+length` must be readable.
unsafe fn checksum(base: *const u8, length: usize) -> u8 {
    let mut sum = 0u8;
    for offset in 0..length {
        // SAFETY: the caller's contract covers the whole range.
        sum = sum.wrapping_add(unsafe { *base.add(offset) });
    }
    sum
}

/// Read `length` bytes at `base`.
///
/// # Safety
///
/// `base..base+length` must be readable, and `length` at most 8.
unsafe fn read_bytes(base: *const u8, length: usize) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    for (offset, slot) in bytes.iter_mut().enumerate().take(length) {
        // SAFETY: the caller's contract covers the range, and `offset` is
        // below `length` by the `take` above.
        *slot = unsafe { *base.add(offset) };
    }
    bytes
}

/// Read a little-endian `u32` at `base`.
///
/// # Safety
///
/// `base..base+4` must be readable.
unsafe fn read_u32_le(base: *const u8) -> u32 {
    let bytes = unsafe { read_bytes(base, 4) };
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Read a little-endian `u64` at `base`.
///
/// # Safety
///
/// `base..base+8` must be readable.
unsafe fn read_u64_le(base: *const u8) -> u64 {
    u64::from_le_bytes(unsafe { read_bytes(base, 8) })
}
