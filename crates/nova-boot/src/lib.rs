//! The architecture-neutral NovaBoot protocol and boot-menu model.
//!
//! # What this crate owns
//!
//! The firmware-to-kernel boundary. A bootloader chooses one menu entry, loads
//! the selected kernel image, and publishes a [`BootProtocol`] snapshot. The
//! kernel validates that snapshot before it trusts any address or CPU identity.
//!
//! The crate is `no_std` and allocation-free. It does not read firmware pages,
//! does not execute UEFI services, and does not know the hardware. That keeps the
//! protocol testable on the host and usable by every architecture backend.
//!
//! # Architecture support
//!
//! [`BootArch`] names the four target families the protocol is designed to
//! carry: x86-64, AArch64, RISC-V 64, and LoongArch 64. Naming an architecture
//! here does not mean its kernel backend exists. It means the boot contract can
//! represent that target without changing its layout.
//!
//! # Limits
//!
//! The protocol is bounded: a fixed CPU table, a fixed memory-region table, and
//! fixed menu-name/path bounds. A bootloader that reports more than the table
//! can hold must fail validation rather than silently truncate hardware facts.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

// The menu-config parser needs `alloc`; see the feature declaration in
// `Cargo.toml`. The kernel links this crate without default features and so
// never sees the module.
#[cfg(feature = "menu-config")]
pub mod menu_config;

/// Page-layout math shared by the boot manager and the kernel.
pub mod layout;

/// The protocol's four-byte magic.
pub const MAGIC: [u8; 4] = *b"NVBP";

/// Major protocol version. A kernel must accept exactly this major.
pub const PROTOCOL_MAJOR: u16 = 1;

/// Minor protocol version. A kernel may accept an older minor.
///
/// `1` added `acpi_century` at the end of the snapshot: the FADT's
/// `RTC century` register index, which the kernel's CMOS reader needs to
/// interpret the century byte correctly. A minor-0 producer never writes the
/// field, and the snapshot page is zeroed, so an older snapshot reads as
/// "unknown century" — exactly the value zero.
pub const PROTOCOL_MINOR: u16 = 1;

/// Maximum memory-map rows a snapshot can carry.
pub const MAX_MEMORY_REGIONS: usize = 64;

/// Maximum processors the protocol can describe.
pub const MAX_CPUS: usize = 8;

/// Maximum menu entries.
pub const MAX_MENU_ENTRIES: usize = 16;

/// Maximum menu-entry name length, in bytes.
pub const MAX_MENU_NAME: usize = 32;

/// Maximum menu-entry kernel-path length, in bytes.
pub const MAX_MENU_PATH: usize = 128;

/// The architecture a boot entry or kernel image targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootArch {
    X86_64 = 1,
    AArch64 = 2,
    Riscv64 = 3,
    LoongArch64 = 4,
}

impl BootArch {
    /// Turn a wire code into an architecture, or `None` for an unknown code.
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::X86_64),
            2 => Some(Self::AArch64),
            3 => Some(Self::Riscv64),
            4 => Some(Self::LoongArch64),
            _ => None,
        }
    }

    /// Turn a human-readable name into an architecture, case-insensitively.
    ///
    /// Accepts `x86_64`, `aarch64`, `riscv64`, and `loongarch64`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "x86_64" => Some(Self::X86_64),
            "aarch64" => Some(Self::AArch64),
            "riscv64" => Some(Self::Riscv64),
            "loongarch64" => Some(Self::LoongArch64),
            _ => None,
        }
    }

    /// Stable wire code.
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Human-readable architecture name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::AArch64 => "aarch64",
            Self::Riscv64 => "riscv64",
            Self::LoongArch64 => "loongarch64",
        }
    }
}

/// Memory-map row kinds the protocol defines.
///
/// The values are the protocol's own, not the firmware's raw classification: a
/// UEFI memory descriptor and a devicetree "reserved" node both reduce to the
/// same row kind, so the kernel never has to know which firmware produced the
/// map. Every kind other than [`REGION_USABLE`] is one the kernel must not
/// hand to its frame allocator.
pub mod region {
    /// Ordinary allocatable RAM.
    pub const USABLE: u16 = 0;
    /// Reserved by the firmware; must not be allocated or repurposed.
    pub const RESERVED: u16 = 1;
    /// Firmware-owned memory a re-enumerating OS may reclaim (ACPI tables,
    /// UEFI runtime code) once it has copied what it needs.
    pub const FIRMWARE_RECLAIMABLE: u16 = 2;
    /// Memory-mapped device space (MMIO), never RAM.
    pub const MMIO: u16 = 3;
}

/// A memory-map row. `kind` is one of the [`region`] constants and is
/// carried through unchanged so the kernel can decide which rows are usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootMemoryRegion {
    pub start: u64,
    pub length: u64,
    pub kind: u16,
}

impl BootMemoryRegion {
    /// End address, or `None` when start plus length overflows.
    pub const fn end(self) -> Option<u64> {
        self.start.checked_add(self.length)
    }
}

/// A framebuffer row. `present == 0` means no framebuffer was handed over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootFramebuffer {
    pub present: u16,
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

impl BootFramebuffer {
    pub const fn absent() -> Self {
        Self {
            present: 0,
            address: 0,
            width: 0,
            height: 0,
            stride: 0,
            format: 0,
        }
    }
}

/// Framebuffer pixel-format codes the protocol defines.
///
/// The firmware's own format description (masks, bitfield layout, or
/// "blit only") is reduced by the boot manager to one of these codes, so the
/// kernel never interprets a firmware structure. `0` is reserved so an
/// all-zero, absent framebuffer never accidentally names a real format.
pub mod fb_format {
    /// 32 bits per pixel, byte order `X R G B` — red in the high bits of each
    /// little-endian word. What QEMU's stdvga reports for a 32-bit mode.
    pub const XRGB8888: u32 = 1;
    /// 32 bits per pixel, byte order `R G B X` — red in the low bits.
    pub const RGB888X: u32 = 2;
}

/// The boot mode the menu selected.
///
/// The menu names an intent ("Desktop" or "Safe Terminal"), the snapshot
/// carries it as a code, and the kernel hands it to `init` through the
/// `GET_BOOT_MODE` syscall. The codes are the contract: `0` means "no choice
/// was made", which a real boot never reports but keeps an all-zero snapshot
/// from naming a mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootMode {
    /// Boot into the full desktop: every boot service starts.
    Desktop = 1,
    /// Boot straight to a terminal: only the essential services start.
    SafeTerminal = 2,
}

impl BootMode {
    /// Turn a wire code into a mode, or `None` for an unknown code.
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::Desktop),
            2 => Some(Self::SafeTerminal),
            _ => None,
        }
    }

    /// Stable wire code.
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Human-readable name, for boot logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::SafeTerminal => "safe-terminal",
        }
    }
}

/// The snapshot a bootloader publishes to the kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootProtocol {
    pub magic: [u8; 4],
    pub major: u16,
    pub minor: u16,
    pub architecture: u16,
    pub flags: u32,
    pub kernel_entry: u64,
    pub kernel_load_address: u64,
    pub direct_map_offset: u64,
    pub initramfs_start: u64,
    pub initramfs_length: u64,
    pub cpu_count: u16,
    pub cpu_ids: [u32; MAX_CPUS],
    /// The local APIC identifier of the bootstrap processor — the CPU the
    /// snapshot was published *from*. The kernel locates its own slot by this
    /// value rather than by position, so a MADT that lists the BSP last still
    /// lands the boot CPU in slot zero's opposite neighbour without surprise.
    pub bsp_lapic_id: u32,
    pub memory_count: u16,
    pub memory: [BootMemoryRegion; MAX_MEMORY_REGIONS],
    pub framebuffer: BootFramebuffer,
    pub selected_entry: u16,
    pub boot_mode: u16,
    pub reserved: u16,
    /// The FADT's `RTC century` register index — which CMOS register holds
    /// the century for the wall clock. `0` means "unknown": the producer
    /// found no FADT (or a minor-0 producer predated the field), and the
    /// kernel falls back to its own inference. The snapshot page is zeroed,
    /// so a producer that predates the field reads as zero here.
    pub acpi_century: u8,
}

impl BootProtocol {
    /// An empty snapshot, used as storage before a bootloader writes it.
    pub const fn empty() -> Self {
        Self {
            magic: [0; 4],
            major: 0,
            minor: 0,
            architecture: 0,
            flags: 0,
            kernel_entry: 0,
            kernel_load_address: 0,
            direct_map_offset: 0,
            initramfs_start: 0,
            initramfs_length: 0,
            cpu_count: 0,
            cpu_ids: [0; MAX_CPUS],
            bsp_lapic_id: 0,
            memory_count: 0,
            memory: [const {
                BootMemoryRegion {
                    start: 0,
                    length: 0,
                    kind: 0,
                }
            }; MAX_MEMORY_REGIONS],
            framebuffer: BootFramebuffer::absent(),
            selected_entry: 0,
            boot_mode: 0,
            reserved: 0,
            acpi_century: 0,
        }
    }

    /// Validate every field that crosses the firmware boundary.
    pub fn validate(&self) -> Result<(), BootProtocolError> {
        if self.magic != MAGIC {
            return Err(BootProtocolError::BadMagic);
        }
        if self.major != PROTOCOL_MAJOR || self.minor > PROTOCOL_MINOR {
            return Err(BootProtocolError::BadVersion);
        }
        if BootArch::from_code(self.architecture).is_none() {
            return Err(BootProtocolError::BadArchitecture);
        }
        if self.reserved != 0 {
            return Err(BootProtocolError::ReservedField);
        }
        // The century field is a CMOS register index: port 0x70's high bit is
        // the NMI control, so a register index past 0x7F cannot be read. `0`
        // means "unknown" and is always acceptable.
        if self.acpi_century > 0x7F {
            return Err(BootProtocolError::BadAcpiCentury);
        }
        if self.boot_mode != 0 && BootMode::from_code(self.boot_mode).is_none() {
            return Err(BootProtocolError::BadBootMode);
        }
        if self.kernel_entry == 0 || self.kernel_load_address == 0 {
            return Err(BootProtocolError::MissingKernel);
        }
        if self.cpu_count == 0 || self.cpu_count as usize > MAX_CPUS {
            return Err(BootProtocolError::BadCpuCount);
        }
        for index in 0..self.cpu_count as usize {
            for other in 0..index {
                if self.cpu_ids[index] == self.cpu_ids[other] {
                    return Err(BootProtocolError::DuplicateCpu);
                }
            }
        }
        // The bootstrap processor must be among the listed ones: the kernel
        // maps its own identity through this field, and a BSP outside the
        // list would leave the boot CPU with no slot at all.
        if !self.cpu_ids[..self.cpu_count as usize].contains(&self.bsp_lapic_id) {
            return Err(BootProtocolError::BspNotListed);
        }

        if self.memory_count as usize > MAX_MEMORY_REGIONS {
            return Err(BootProtocolError::TooManyRegions);
        }
        for index in 0..self.memory_count as usize {
            let region = self.memory[index];
            let Some(end) = region.end() else {
                return Err(BootProtocolError::RegionOverflow);
            };
            if region.length == 0 {
                return Err(BootProtocolError::EmptyRegion);
            }
            if end <= region.start {
                return Err(BootProtocolError::RegionOverflow);
            }
            for previous in self.memory.iter().take(index) {
                let Some(previous_end) = previous.end() else {
                    return Err(BootProtocolError::RegionOverflow);
                };
                if region.start < previous_end && previous.start < end {
                    return Err(BootProtocolError::OverlappingRegions);
                }
            }
        }

        if self.initramfs_length != 0 {
            let Some(end) = self.initramfs_start.checked_add(self.initramfs_length) else {
                return Err(BootProtocolError::InitramfsOverflow);
            };
            let mut inside = false;
            for index in 0..self.memory_count as usize {
                let region = self.memory[index];
                let Some(region_end) = region.end() else {
                    return Err(BootProtocolError::RegionOverflow);
                };
                if self.initramfs_start >= region.start && end <= region_end {
                    inside = true;
                    break;
                }
            }
            if !inside {
                return Err(BootProtocolError::InitramfsOutsideMemory);
            }
        }

        if self.framebuffer.present != 0 {
            if self.framebuffer.address == 0 {
                return Err(BootProtocolError::BadFramebuffer);
            }
            if self.framebuffer.stride < self.framebuffer.width {
                return Err(BootProtocolError::BadFramebuffer);
            }
        }

        Ok(())
    }
}

/// Why a snapshot was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootProtocolError {
    BadMagic,
    BadVersion,
    BadArchitecture,
    ReservedField,
    MissingKernel,
    BadCpuCount,
    DuplicateCpu,
    BspNotListed,
    TooManyRegions,
    RegionOverflow,
    EmptyRegion,
    OverlappingRegions,
    InitramfsOverflow,
    InitramfsOutsideMemory,
    BadFramebuffer,
    BadBootMode,
    BadAcpiCentury,
}

impl BootProtocolError {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::BadMagic => "bad-magic",
            Self::BadVersion => "bad-version",
            Self::BadArchitecture => "bad-architecture",
            Self::ReservedField => "reserved-field",
            Self::MissingKernel => "missing-kernel",
            Self::BadCpuCount => "bad-cpu-count",
            Self::DuplicateCpu => "duplicate-cpu",
            Self::BspNotListed => "bsp-not-listed",
            Self::TooManyRegions => "too-many-regions",
            Self::RegionOverflow => "region-overflow",
            Self::EmptyRegion => "empty-region",
            Self::OverlappingRegions => "overlapping-regions",
            Self::InitramfsOverflow => "initramfs-overflow",
            Self::InitramfsOutsideMemory => "initramfs-outside-memory",
            Self::BadFramebuffer => "bad-framebuffer",
            Self::BadBootMode => "bad-boot-mode",
            Self::BadAcpiCentury => "bad-acpi-century",
        }
    }
}

/// One selectable menu entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootMenuEntry<'a> {
    pub id: u16,
    pub name: &'a [u8],
    pub architecture: BootArch,
    pub kernel_path: &'a [u8],
    pub flags: u32,
}

/// A bounded menu model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootMenu<'a> {
    entries: &'a [BootMenuEntry<'a>],
}

impl<'a> BootMenu<'a> {
    pub const fn new(entries: &'a [BootMenuEntry<'a>]) -> Self {
        Self { entries }
    }

    /// Validate menu bounds, IDs, names, paths, and architecture codes.
    pub fn validate(&self) -> Result<(), BootMenuError> {
        if self.entries.is_empty() {
            return Err(BootMenuError::EmptyMenu);
        }
        if self.entries.len() > MAX_MENU_ENTRIES {
            return Err(BootMenuError::TooManyEntries);
        }
        for index in 0..self.entries.len() {
            let entry = self.entries[index];
            if entry.name.is_empty() || entry.name.len() > MAX_MENU_NAME {
                return Err(BootMenuError::BadName);
            }
            if entry.kernel_path.is_empty() || entry.kernel_path.len() > MAX_MENU_PATH {
                return Err(BootMenuError::BadPath);
            }
            for other in self.entries.iter().take(index) {
                if entry.id == other.id {
                    return Err(BootMenuError::DuplicateId);
                }
            }
        }
        Ok(())
    }

    /// Select by stable entry ID rather than by list position.
    pub fn select(&self, id: u16) -> Option<&'a BootMenuEntry<'a>> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The first entry, used as the default.
    pub fn default_entry(&self) -> Option<&'a BootMenuEntry<'a>> {
        self.entries.first()
    }

    /// The number of entries.
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the menu holds no entries.
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Why a menu was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootMenuError {
    EmptyMenu,
    TooManyEntries,
    BadName,
    BadPath,
    DuplicateId,
}

impl BootMenuError {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::EmptyMenu => "empty-menu",
            Self::TooManyEntries => "too-many-entries",
            Self::BadName => "bad-name",
            Self::BadPath => "bad-path",
            Self::DuplicateId => "duplicate-id",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [u32; MAX_CPUS] = [0, 1, 2, 3, 4, 5, 6, 7];

    fn valid_protocol() -> BootProtocol {
        let mut protocol = BootProtocol::empty();
        protocol.magic = MAGIC;
        protocol.major = PROTOCOL_MAJOR;
        protocol.minor = PROTOCOL_MINOR;
        protocol.architecture = BootArch::X86_64.code();
        protocol.kernel_entry = 0xFFFF_FFFF_8010_0000;
        protocol.kernel_load_address = 0x0010_0000;
        protocol.direct_map_offset = 0xFFFF_8000_0000_0000;
        protocol.cpu_count = 4;
        protocol.cpu_ids[..4].copy_from_slice(&IDS[..4]);
        protocol.memory_count = 2;
        protocol.memory[0] = BootMemoryRegion {
            start: 0,
            length: 0x0010_0000,
            kind: 1,
        };
        protocol.memory[1] = BootMemoryRegion {
            start: 0x0020_0000,
            length: 0x0100_0000,
            kind: 1,
        };
        protocol.initramfs_start = 0x0020_0000;
        protocol.initramfs_length = 0x0001_0000;
        protocol.selected_entry = 1;
        protocol.boot_mode = BootMode::Desktop.code();
        protocol
    }

    #[test]
    fn architecture_codes_round_trip() {
        for architecture in [
            BootArch::X86_64,
            BootArch::AArch64,
            BootArch::Riscv64,
            BootArch::LoongArch64,
        ] {
            assert_eq!(BootArch::from_code(architecture.code()), Some(architecture));
        }
        assert_eq!(BootArch::from_code(5), None);
    }

    #[test]
    fn a_valid_snapshot_is_accepted() {
        assert_eq!(valid_protocol().validate(), Ok(()));
    }

    #[test]
    fn a_wrong_magic_is_refused() {
        let mut protocol = valid_protocol();
        protocol.magic = [0; 4];
        assert_eq!(protocol.validate(), Err(BootProtocolError::BadMagic));
    }

    #[test]
    fn a_newer_minor_is_refused_but_an_older_one_is_accepted() {
        let mut protocol = valid_protocol();
        protocol.minor = PROTOCOL_MINOR + 1;
        assert_eq!(protocol.validate(), Err(BootProtocolError::BadVersion));
        protocol.minor = 0;
        assert_eq!(protocol.validate(), Ok(()));
    }

    #[test]
    fn duplicate_processor_ids_are_refused() {
        let mut protocol = valid_protocol();
        protocol.cpu_ids[1] = protocol.cpu_ids[0];
        assert_eq!(protocol.validate(), Err(BootProtocolError::DuplicateCpu));
    }

    #[test]
    fn a_bootstrap_processor_outside_the_list_is_refused() {
        let mut protocol = valid_protocol();
        // The listed ids never include this one, so a kernel that trusted it
        // would find no slot for the CPU it is running on.
        protocol.bsp_lapic_id = 0xFF;
        assert_eq!(protocol.validate(), Err(BootProtocolError::BspNotListed));
    }

    #[test]
    fn an_overflowing_memory_region_is_refused() {
        let mut protocol = valid_protocol();
        protocol.memory[0] = BootMemoryRegion {
            start: u64::MAX - 4,
            length: 8,
            kind: 1,
        };
        assert_eq!(protocol.validate(), Err(BootProtocolError::RegionOverflow));
    }

    #[test]
    fn overlapping_memory_regions_are_refused() {
        let mut protocol = valid_protocol();
        protocol.memory[1] = BootMemoryRegion {
            start: 0x0008_0000,
            length: 0x0010_0000,
            kind: 1,
        };
        assert_eq!(
            protocol.validate(),
            Err(BootProtocolError::OverlappingRegions)
        );
    }

    #[test]
    fn initramfs_must_lie_inside_a_reported_region() {
        let mut protocol = valid_protocol();
        protocol.initramfs_start = 0x0020_0000;
        protocol.initramfs_length = 0x0200_0000;
        assert_eq!(
            protocol.validate(),
            Err(BootProtocolError::InitramfsOutsideMemory)
        );
    }

    #[test]
    fn a_bad_framebuffer_stride_is_refused() {
        let mut protocol = valid_protocol();
        protocol.framebuffer.present = 1;
        protocol.framebuffer.address = 0x0100_0000;
        protocol.framebuffer.width = 10;
        protocol.framebuffer.height = 10;
        protocol.framebuffer.stride = 9;
        assert_eq!(protocol.validate(), Err(BootProtocolError::BadFramebuffer));
    }

    #[test]
    fn boot_mode_codes_round_trip() {
        assert_eq!(
            BootMode::from_code(BootMode::Desktop.code()),
            Some(BootMode::Desktop)
        );
        assert_eq!(
            BootMode::from_code(BootMode::SafeTerminal.code()),
            Some(BootMode::SafeTerminal)
        );
        assert_eq!(BootMode::from_code(0), None);
        assert_eq!(BootMode::from_code(3), None);
    }

    #[test]
    fn an_unknown_boot_mode_is_refused_but_absent_is_accepted() {
        let mut protocol = valid_protocol();
        protocol.boot_mode = 3;
        assert_eq!(protocol.validate(), Err(BootProtocolError::BadBootMode));
        protocol.boot_mode = 0;
        assert_eq!(protocol.validate(), Ok(()));
    }

    #[test]
    fn menu_ids_are_unique_and_selectable() {
        let entries = [
            BootMenuEntry {
                id: 1,
                name: b"NovaOS",
                architecture: BootArch::X86_64,
                kernel_path: b"/boot/nova-kernel.elf",
                flags: 0,
            },
            BootMenuEntry {
                id: 2,
                name: b"NovaOS recovery",
                architecture: BootArch::X86_64,
                kernel_path: b"/boot/nova-kernel.elf",
                flags: 1,
            },
        ];
        let menu = BootMenu::new(&entries);
        assert_eq!(menu.validate(), Ok(()));
        assert_eq!(menu.select(1).unwrap().name, b"NovaOS");
        assert_eq!(menu.select(3), None);
    }

    #[test]
    fn duplicate_menu_ids_are_refused() {
        let entries = [
            BootMenuEntry {
                id: 1,
                name: b"A",
                architecture: BootArch::X86_64,
                kernel_path: b"/a",
                flags: 0,
            },
            BootMenuEntry {
                id: 1,
                name: b"B",
                architecture: BootArch::AArch64,
                kernel_path: b"/b",
                flags: 0,
            },
        ];
        assert_eq!(
            BootMenu::new(&entries).validate(),
            Err(BootMenuError::DuplicateId)
        );
    }

    #[test]
    fn menu_names_and_paths_are_bounded() {
        let long_name = [b'a'; MAX_MENU_NAME + 1];
        let long_path = [b'p'; MAX_MENU_PATH + 1];
        assert_eq!(
            BootMenu::new(&[BootMenuEntry {
                id: 1,
                name: &long_name,
                architecture: BootArch::X86_64,
                kernel_path: b"/a",
                flags: 0,
            }])
            .validate(),
            Err(BootMenuError::BadName)
        );
        assert_eq!(
            BootMenu::new(&[BootMenuEntry {
                id: 1,
                name: b"A",
                architecture: BootArch::X86_64,
                kernel_path: &long_path,
                flags: 0,
            }])
            .validate(),
            Err(BootMenuError::BadPath)
        );
    }
}
