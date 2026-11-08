//! PCI enumeration through the ECAM window and configuration mechanism #1.
//!
//! # What this is
//!
//! PCI devices are found by reading a small window of configuration space —
//! 256 bytes per function through the ports, the full 4 KiB per function
//! through the ECAM window. When the boot protocol's MCFG walk found a window,
//! every bus it covers is read through memory-mapped access; the two I/O ports
//! of mechanism #1 — `0xCF8` takes an address, `0xCFC` returns (or takes) the
//! data at it — remain for what the window cannot reach. Both mechanisms
//! answer the same question: which device it is, where its BARs are, what
//! capabilities it advertises, and which interrupt it wires to.
//!
//! # Why enumeration is bounded
//!
//! Scanning "every bus, every slot, every function" is 65,536 config reads even
//! on a machine with four devices on one bus, and each read is three IO-port
//! instructions. Under emulation that is a noticeable slice of boot for an
//! answer the topology already bounds. This module instead starts at bus 0 and
//! follows PCI-to-PCI bridges to the buses they own, which is how the devices
//! actually get attached — and both the bus list and the device list are
//! bounded, so a misread bridge cannot send the walk into an unbounded region.
//!
//! # What is *not* here
//!
//! Interrupts. The M5 route is polled VirtIO (see `drivers::virtio::blk`), so
//! the interrupt line a device reports is read and recorded but never
//! dispatched to. Adding an IRQ path before anything routes one would be a
//! handler installed with nothing to say to it.

use crate::arch::x86_64::port_io::{inl, outl};
use crate::klog;
use crate::logger::Level;
use crate::mm::{address_space, direct_map};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The configuration *address* port of mechanism #1.
const CONFIG_ADDRESS: u16 = 0xCF8;

/// The configuration *data* port of mechanism #1.
const CONFIG_DATA: u16 = 0xCFC;

/// The physical base of the ECAM/MMCONFIG window, once installed.
///
/// Zero means "no window": the boot protocol reported none, and every
/// configuration access goes through mechanism #1's ports.
static ECAM_BASE: AtomicU64 = AtomicU64::new(0);

/// The last bus the window covers, when `ECAM_BASE` is nonzero.
static ECAM_BUS_END: AtomicU32 = AtomicU32::new(0);

/// One bit per bus whose 1-MiB window slice has been mapped into the kernel
/// half. The window can span 256 MiB — every bus the ports can reach — and
/// mapping all of it eagerly would be 65,536 page-table walks for an answer
/// the topology bounds to a handful of buses, so each bus's slice is mapped
/// on first access instead.
static ECAM_MAPPED_BUSES: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Install the ECAM window the boot protocol discovered.
///
/// After this, configuration reads for buses the window covers go through
/// memory-mapped access — the mechanism modern firmware wires up and the ports
/// are the compatibility leftover of — and reads for buses past the window
/// fall back to the ports. Calling twice replaces the window, which the tests
/// do deliberately; the boot path calls it once.
pub fn install_ecam(base: u64, bus_end: u8) {
    // A window that is not page-aligned cannot be real: the spec sizes it
    // 4 KiB per function, and the boot protocol's validation refuses one
    // already. The check is repeated here because this function is also the
    // tests' entry point.
    if base & 0xFFF != 0 {
        klog!(Level::Warn, "pci: refusing unaligned ECAM base {:#x}", base);
        return;
    }
    // Forget any slices an earlier window had mapped: the mappings live in
    // the shared kernel half, and a reinstalled window names new addresses.
    for slice in &ECAM_MAPPED_BUSES {
        slice.store(0, Ordering::Relaxed);
    }
    ECAM_BASE.store(base, Ordering::Relaxed);
    ECAM_BUS_END.store(bus_end as u32, Ordering::Relaxed);
    klog!(
        Level::Info,
        "pci: ECAM window {:#x}.. covering buses 0..={}",
        base,
        bus_end
    );
}

/// The installed ECAM window's physical base, if one is installed.
pub fn ecam_base() -> Option<u64> {
    match ECAM_BASE.load(Ordering::Relaxed) {
        0 => None,
        base => Some(base),
    }
}

/// The installed ECAM window's last covered bus, if one is installed.
pub fn ecam_bus_end() -> Option<u8> {
    match ECAM_BASE.load(Ordering::Relaxed) {
        0 => None,
        _ => Some(ECAM_BUS_END.load(Ordering::Relaxed) as u8),
    }
}

/// The physical address of one dword of config space in the ECAM window.
///
/// The window is 4 KiB per function: the bus number selects one of the 1-MiB
/// slices a bus occupies, the slot one of the thirty-two 32-KiB pages in it,
/// the function one of the eight 4-KiB pages in that, and the offset the dword
/// in the page. This is pure arithmetic — the one part of the mechanism a
/// test can pin without touching hardware.
fn ecam_address(base: u64, location: Location, offset: u8) -> u64 {
    base | ((location.bus as u64) << 20)
        | ((location.slot as u64) << 15)
        | ((location.func as u64) << 12)
        | (offset as u64)
}

/// Bytes of the window one bus occupies: 4 KiB per function × 8 functions
/// × 32 slots.
const ECAM_SLICE_BYTES: u64 = 1 << 20;

/// Bytes per mapped page — the leaf `map_kernel_mmio` installs.
const MMIO_PAGE_BYTES: u64 = 0x1000;

/// Map the bus's 1-MiB slice of the window into the shared kernel half.
///
/// The direct map covers the memory the boot protocol reported as RAM — the
/// ECAM window lives in a PCI MMIO hole the HHDM never mapped, and a read
/// through it faults. This is the same fix the virtio transport applies to
/// its BARs: an explicit kernel-half mapping, installed once per bus and
/// remembered, so repeated configuration reads are pure memory accesses.
///
/// Returns `false` when the mapping could not be installed — the frame
/// allocator is out of page-table frames, say — and the caller should take
/// the port fallback rather than fault.
fn ecam_map_bus(bus: u8) -> bool {
    let index = (bus >> 6) as usize;
    let bit = 1u64 << (bus & 63);
    if ECAM_MAPPED_BUSES[index].load(Ordering::Acquire) & bit != 0 {
        return true;
    }

    let base = ECAM_BASE.load(Ordering::Relaxed);
    let slice_phys = base + ((bus as u64) << 20);
    // SAFETY: each call maps one page of the window the boot protocol
    // validated, at its own direct-map alias — the exact pair
    // `map_kernel_mmio` exists to install. `AlreadyMapped` is tolerated: the
    // slice is idempotently mapped, either by a previous window or by an
    // earlier call that recorded nothing.
    for page in 0..(ECAM_SLICE_BYTES / MMIO_PAGE_BYTES) {
        let offset = page * MMIO_PAGE_BYTES;
        match address_space::map_kernel_mmio(
            direct_map::phys_to_virt(slice_phys + offset) as u64,
            slice_phys + offset,
        ) {
            Ok(()) | Err(nova_core::address_space::VmmError::AlreadyMapped) => {}
            Err(_) => {
                klog!(Level::Warn, "pci: could not map ECAM slice for bus {}", bus);
                return false;
            }
        }
    }
    ECAM_MAPPED_BUSES[index].fetch_or(bit, Ordering::Release);
    true
}

/// Read one dword through the ECAM window, or `None` when the bus's slice
/// could not be mapped.
///
/// # Safety
///
/// The window must be installed and cover `location`'s bus, and `mm` must be
/// up — the mapping goes through the frame allocator.
unsafe fn read_u32_ecam(location: Location, offset: u8) -> Option<u32> {
    if !ecam_map_bus(location.bus) {
        return None;
    }
    let base = ECAM_BASE.load(Ordering::Relaxed);
    let address = ecam_address(base, location, offset & 0xFC);
    // SAFETY: the slice was mapped by `ecam_map_bus`, and `mmio::read_u32`
    // performs the single aligned volatile load.
    Some(unsafe { crate::drivers::mmio::read_u32(direct_map::phys_to_virt(address), 0) })
}

/// Write one dword through the ECAM window, or `None` when the bus's slice
/// could not be mapped.
///
/// # Safety
///
/// As [`read_u32_ecam`], plus this function's own contract for the value — a
/// configuration write is device control.
unsafe fn write_u32_ecam(location: Location, offset: u8, value: u32) -> Option<()> {
    if !ecam_map_bus(location.bus) {
        return None;
    }
    let base = ECAM_BASE.load(Ordering::Relaxed);
    let address = ecam_address(base, location, offset & 0xFC);
    // SAFETY: as `read_u32_ecam`.
    unsafe { crate::drivers::mmio::write_u32(direct_map::phys_to_virt(address), 0, value) };
    Some(())
}

/// Whether the ECAM window covers this bus.
///
/// The window is the *only* mechanism that reaches past bus 255, and it is
/// also the only one that reaches a bus a bridge-following walk cannot name
/// through the ports' 8-bit bus field — though this walk never goes there.
fn ecam_covers(bus: u8) -> bool {
    ECAM_BASE.load(Ordering::Relaxed) != 0 && (bus as u32) <= ECAM_BUS_END.load(Ordering::Relaxed)
}

/// Enable bit the address port requires before any access is decoded.
const CONFIG_ENABLE: u32 = 1 << 31;

/// Slots per bus, as the slot field of the address port encodes them.
const SLOTS_PER_BUS: u8 = 32;

/// Functions per slot, as the function field encodes them.
const FUNCS_PER_SLOT: u8 = 8;

/// Buses this walk is willing to visit, bridges included.
///
/// The reference machine has one bus plus a bridge or two. Eight is generous
/// headroom, and it is a *bound*, not an estimate: a device tree that really
/// needs more than eight buses is reported as "buses past this were not
/// scanned" rather than walked off the end of the array.
pub const MAX_BUSES: usize = 8;

/// Devices one scan records.
///
/// q35 presents roughly a dozen functions (the host bridge, the ISA bridge,
/// AHCI, the virtio disks, and what OVMF left behind). Thirty-two leaves room
/// for all of them and refuses, loudly, a machine with more.
pub const MAX_DEVICES: usize = 32;

/// Configuration bytes a capability walk is willing to read.
///
/// A capability list is a linked list in config space, and a *corrupt* one can
/// be circular. The walk stops after this many hops rather than following a
/// cycle forever — which on a polled path with no interrupt would hang boot.
pub const MAX_CAPABILITIES: u8 = 32;

/// The lowest offset a capability may live at.
///
/// 0x40 is the end of the 64-byte standard header. A "capability" that points
/// into the header is not a capability; it is a misread pointer, and reading
/// it as one would interpret the device-ID field as a next-pointer.
const FIRST_CAPABILITY_OFFSET: u8 = 0x40;

/// Intel's vendor ID, for recognising the host bridge.
/// The MSI-X capability's ID in the capability list.
pub const CAP_MSIX: u8 = 0x11;

pub const VENDOR_INTEL: u16 = 0x8086;

/// The vendor ID of every VirtIO device, legacy and modern alike.
pub const VENDOR_VIRTIO: u16 = 0x1AF4;

/// VirtIO block, legacy device ID.
///
/// A device carrying this ID may speak only the legacy transport, which this
/// kernel does not implement — but a *transitional* device (the kind QEMU
/// presents by default) keeps the legacy ID and advertises the modern
/// capability list beside it, so this ID alone says nothing about which
/// transport is available. See [`Device::virtio_capabilities`].
pub const DEVICE_VIRTIO_BLK_LEGACY: u16 = 0x1001;

/// VirtIO block, modern (VirtIO 1.0) device ID.
pub const DEVICE_VIRTIO_BLK_MODERN: u16 = 0x1042;

/// VirtIO network, legacy device ID.
pub const DEVICE_VIRTIO_NET_LEGACY: u16 = 0x1000;

/// VirtIO network, modern (VirtIO 1.0) device ID.
pub const DEVICE_VIRTIO_NET_MODERN: u16 = 0x1041;

/// Class code: the device is a bridge between buses.
pub const CLASS_BRIDGE: u8 = 0x06;

/// Subclass of [`CLASS_BRIDGE`]: a PCI-to-PCI bridge.
pub const SUBCLASS_PCI_BRIDGE: u8 = 0x04;

/// Where a function lives.
///
/// Three bytes, copied by value: a location is a *name*, not a reference to
/// anything, and a driver keeps one for the device's whole lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Location {
    /// The bus number.
    pub bus: u8,
    /// The slot number on that bus.
    pub slot: u8,
    /// The function number in that slot.
    pub func: u8,
}

impl Location {
    /// The canonical print form, `bus:slot.func`.
    pub const fn formatted(self) -> FormattedLocation {
        FormattedLocation(self)
    }
}

/// [`Location`]'s `Display`, so a diagnostic can name a device exactly.
pub struct FormattedLocation(pub Location);

impl core::fmt::Display for FormattedLocation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.0.bus, self.0.slot, self.0.func)
    }
}

/// One capability in a function's list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capability {
    /// The capability ID. `0x09` is vendor-specific (VirtIO's).
    pub id: u8,
    /// The byte offset of the capability's own structure.
    pub offset: u8,
}

/// What one BAR turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarInfo {
    /// The physical base address, already shifted into a memory address.
    pub address: u64,
    /// The region's size in bytes, a power of two.
    pub size: u64,
    /// Whether the BAR is an IO port range rather than memory.
    pub io: bool,
    /// Whether the region may sit below the memory hole (memory BARs only).
    pub prefetchable: bool,
}

/// One enumerated PCI function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Device {
    /// Where it lives.
    pub location: Location,
    /// Vendor ID from config offset 0x00.
    pub vendor_id: u16,
    /// Device ID from config offset 0x02.
    pub device_id: u16,
    /// Programming interface byte.
    pub prog_if: u8,
    /// Subclass code.
    pub subclass: u8,
    /// Class code.
    pub class_code: u8,
    /// Revision ID.
    pub revision: u8,
    /// Header type, with the multi-function bit already stripped.
    pub header_type: u8,
    /// Whether the header advertises more than one function.
    pub multi_function: bool,
    /// The interrupt line the firmware assigned, if any.
    pub interrupt_line: u8,
}

impl Device {
    /// Whether this is a VirtIO block device this kernel can drive.
    ///
    /// The device ID is *not* the whole answer. A transitional device keeps
    /// the legacy ID while offering the modern transport, so the final word
    /// belongs to [`Device::virtio_capabilities`]: a device with no capability
    /// list has only the legacy interface, which is not implemented here.
    pub const fn is_virtio_block(&self) -> bool {
        self.vendor_id == VENDOR_VIRTIO
            && (self.device_id == DEVICE_VIRTIO_BLK_LEGACY
                || self.device_id == DEVICE_VIRTIO_BLK_MODERN)
    }

    /// Whether this is a VirtIO network device this kernel can drive.
    ///
    /// As with block devices, the ID alone is not the whole answer: a
    /// transitional device keeps the legacy ID while offering the modern
    /// capability list. The transport check remains the final word.
    pub const fn is_virtio_net(&self) -> bool {
        self.vendor_id == VENDOR_VIRTIO
            && (self.device_id == DEVICE_VIRTIO_NET_LEGACY
                || self.device_id == DEVICE_VIRTIO_NET_MODERN)
    }

    /// Whether this function is a PCI-to-PCI bridge.
    pub const fn is_pci_bridge(&self) -> bool {
        self.class_code == CLASS_BRIDGE && self.subclass == SUBCLASS_PCI_BRIDGE
    }

    /// Walk the capability list, in list order.
    ///
    /// The walk is bounded and self-terminating: a pointer that leaves the
    /// capability region, a pointer that is not dword-aligned, or a list that
    /// loops all end the iteration. A corrupt list therefore yields a prefix
    /// of real capabilities rather than a hang — the reader still sees the
    /// capabilities before the damage, and can decide for itself whether that
    /// is enough.
    pub fn capabilities(&self) -> CapabilityWalker {
        let status = read_u16(self.location, 0x06);
        let has_list = status & (1 << 4) != 0;
        let first = if has_list {
            read_u8(self.location, 0x34)
        } else {
            0
        };
        CapabilityWalker {
            location: self.location,
            next: first,
            hops: 0,
        }
    }

    /// Read the standard header's interrupt line register.
    pub fn read_interrupt_line(&self) -> u8 {
        read_u8(self.location, 0x3C)
    }
}

/// A bounded walk over one function's capability list.
pub struct CapabilityWalker {
    location: Location,
    next: u8,
    hops: u8,
}

impl Iterator for CapabilityWalker {
    type Item = Capability;

    fn next(&mut self) -> Option<Capability> {
        if self.next == 0 || self.hops >= MAX_CAPABILITIES {
            return None;
        }
        // Alignment and region checks: see the type's docs. `FIRST_CAPABILITY_OFFSET`
        // also excludes a pointer that lands back on the list head's own storage.
        if self.next < FIRST_CAPABILITY_OFFSET || self.next & 3 != 0 {
            return None;
        }

        let offset = self.next;
        let id = read_u8(self.location, offset);
        self.next = read_u8(self.location, offset + 1);
        self.hops += 1;

        Some(Capability { id, offset })
    }
}

/// A raw VirtIO vendor capability, as it sits in config space.
///
/// Parsed here rather than in the VirtIO code because *finding* the capability
/// is PCI's job; interpreting `cfg_type` is the transport's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VirtioCapability {
    /// Which structure this capability points at (1 common, 2 notify, 3 ISR, 4 device).
    pub cfg_type: u8,
    /// The BAR the structure lives in.
    pub bar: u8,
    /// Byte offset of the structure within that BAR.
    pub offset: u32,
    /// Length of the structure, for a range check before it is read.
    pub length: u32,
    /// For a notify capability: bytes per queue-notify register step.
    pub notify_off_multiplier: u32,
}

/// A fixed set of the VirtIO capabilities one device advertises.
pub struct VirtioCapabilities {
    /// Up to [`MAX_VIRTIO_CAPS`] of them, in list order.
    pub entries: [Option<VirtioCapability>; MAX_VIRTIO_CAPS],
    /// How many were filled.
    pub len: usize,
}

/// VirtIO capability structures one device may advertise.
///
/// A block device needs four (common, notify, ISR, device config); five with a
/// spare is the bound that keeps this a fixed array.
pub const MAX_VIRTIO_CAPS: usize = 5;

impl VirtioCapabilities {
    /// The capability of the given `cfg_type`, if the device advertises one.
    pub fn find(&self, cfg_type: u8) -> Option<VirtioCapability> {
        self.entries[..self.len]
            .iter()
            .flatten()
            .find(|capability| capability.cfg_type == cfg_type)
            .copied()
    }
}

/// Parse this device's VirtIO vendor capabilities.
///
/// Returns `None` when the function is not a VirtIO device or has no vendor
/// capability at all — which is exactly the transitional-vs-legacy distinction
/// [`Device::is_virtio_block`] documents: the ID says "block device", this says
/// "modern transport".
pub fn virtio_capabilities(device: &Device) -> Option<VirtioCapabilities> {
    if device.vendor_id != VENDOR_VIRTIO {
        return None;
    }

    let mut capabilities = VirtioCapabilities {
        entries: [None; MAX_VIRTIO_CAPS],
        len: 0,
    };

    for capability in device.capabilities() {
        if capability.id != VENDOR_CAP_VIRTIO || capabilities.len == MAX_VIRTIO_CAPS {
            continue;
        }

        // The structure begins three bytes in: the first three bytes of the
        // capability are the standard capability header (`cap_vndr`,
        // `cap_next`, `cap_len`), which the walk already consumed.
        let base = capability.offset as usize + 3;
        let cfg_type = read_u8(device.location, base as u8);
        let bar = read_u8(device.location, base as u8 + 1);
        let offset = read_u32(device.location, base as u8 + 5);
        let length = read_u32(device.location, base as u8 + 9);
        // The notify multiplier trails the common fields and only exists for a
        // notify capability; reading it for another type would pick up whatever
        // the next structure's header happens to be.
        let notify_off_multiplier = if cfg_type == VIRTIO_CAP_NOTIFY {
            read_u32(device.location, base as u8 + 13)
        } else {
            0
        };

        capabilities.entries[capabilities.len] = Some(VirtioCapability {
            cfg_type,
            bar,
            offset,
            length,
            notify_off_multiplier,
        });
        capabilities.len += 1;
    }

    Some(capabilities)
}

/// The VirtIO vendor capability ID.
pub const VENDOR_CAP_VIRTIO: u8 = 0x09;

/// VirtIO capability type: the common configuration structure.
pub const VIRTIO_CAP_COMMON: u8 = 1;

/// VirtIO capability type: the notification structure.
pub const VIRTIO_CAP_NOTIFY: u8 = 2;

/// VirtIO capability type: the interrupt status register.
pub const VIRTIO_CAP_ISR: u8 = 3;

/// VirtIO capability type: the device's own configuration.
pub const VIRTIO_CAP_DEVICE: u8 = 4;

/// Read a 32-bit word of a function's configuration space.
///
/// `offset` must be dword-aligned; bits 0-1 are dropped, because both
/// mechanisms decode on word boundaries and an unaligned request would address
/// a neighbouring register.
///
/// The access goes through the ECAM window when one is installed and covers
/// the bus, and through mechanism #1's ports otherwise — the two return the
/// same dword for any bus both can reach, which is what the test suite pins.
pub fn read_u32(location: Location, offset: u8) -> u32 {
    if ecam_covers(location.bus) {
        // SAFETY: the window covers this bus; see `read_u32_ecam`.
        if let Some(word) = (unsafe { read_u32_ecam(location, offset) }) {
            return word;
        }
        // The slice could not be mapped; the ports below still answer for
        // every bus they reach, which is every bus this walk can name.
    }
    read_u32_ports(location, offset)
}

/// Read one dword through mechanism #1's ports — the fallback path.
///
/// Kept separate from [`read_u32`] so the test suite can read the same
/// register through both mechanisms and pin their agreement.
pub fn read_u32_ports(location: Location, offset: u8) -> u32 {
    let address = CONFIG_ENABLE
        | ((location.bus as u32) << 16)
        | ((location.slot as u32) << 11)
        | ((location.func as u32) << 8)
        | ((offset as u32) & 0xFC);

    // SAFETY: these are the two ports mechanism #1 defines, and the address
    // written is masked to the enable bit, the three five-bit fields, and a
    // dword-aligned offset — so the address port can only ever name a register
    // inside this function's 256-byte window.
    unsafe {
        outl(CONFIG_ADDRESS, address);
        inl(CONFIG_DATA)
    }
}

/// Write a 32-bit word of a function's configuration space.
///
/// # Safety
///
/// Configuration space is device control: a write can detach a BAR, retarget a
/// DMA engine, or disable a device outright. The caller must know which
/// register it is writing and why.
///
/// `offset` must be dword-aligned; see [`read_u32`].
pub unsafe fn write_u32(location: Location, offset: u8, value: u32) {
    if ecam_covers(location.bus) {
        // SAFETY: the window covers this bus; see `write_u32_ecam`.
        if unsafe { write_u32_ecam(location, offset, value) }.is_some() {
            return;
        }
        // As `read_u32`: fall through to the ports rather than fault.
    }
    let address = CONFIG_ENABLE
        | ((location.bus as u32) << 16)
        | ((location.slot as u32) << 11)
        | ((location.func as u32) << 8)
        | ((offset as u32) & 0xFC);

    // SAFETY: as `read_u32`, plus this function's own contract for the value.
    unsafe {
        outl(CONFIG_ADDRESS, address);
        outl(CONFIG_DATA, value);
    }
}

/// The command register: the device's own switches for what it decodes.
///
/// Two bits matter here. The **memory space** bit says the device may
/// respond to memory addresses in its BARs, and the **bus master** bit says the
/// device may initiate a DMA transaction. A device with both clear is inert:
/// its registers read as nothing and its DMA engine never runs, and the
/// symptom is a request that never completes with no error anywhere.
pub const COMMAND_MEMORY: u16 = 1 << 1;

/// The command register's bus-master bit.
pub const COMMAND_MASTER: u16 = 1 << 2;

/// The command register's offset in the configuration window.
pub const COMMAND_OFFSET: u8 = 0x04;

/// Let a device answer memory accesses and run its DMA engine.
///
/// # Why this must happen before anything else
///
/// A PCI device that the operating system has not "claimed" is switched off:
/// firmware programs its BARs, but the command register still says the device
/// may neither answer memory accesses nor initiate DMA. Two consequences
/// follow, and both are silent:
///
/// * The BARs are not mapped into the machine's address space, so reading the
///   device's registers returns nothing at all.
/// * The device's DMA engine is gated off, so a request the driver publishes
///   is never fetched by the device — and the driver waits for a completion
///   that will never come.
///
/// The order matters too: the device is *reset* when the command register is
/// written without the bus-master bit, so the write has to happen before the
/// device is brought up, not after.
///
/// Returns the command register as the device now reports it.
pub fn enable_transport(location: Location) -> u32 {
    let command = read_u16(location, COMMAND_OFFSET);

    // Preserve the bits the device already had: the IO-space bit may matter
    // for a device that also has an IO BAR, and clearing a bit the device set
    // is a change nobody asked for.
    let wanted = command | COMMAND_MEMORY | COMMAND_MASTER;

    // SAFETY: the offset is the command register, the value sets the two
    // bits this device's transport needs and nothing else, and the write is
    // made before the device is initialized — so no device state is lost.
    //
    // The dword write also covers the status register that sits next to it
    // (0x06); its bits are read-only in the specification apart from the
    // error bits, so writing zero there clears nothing meaningful.
    unsafe { write_u32(location, COMMAND_OFFSET, wanted as u32) };

    let readback = read_u16(location, COMMAND_OFFSET);
    klog!(
        Level::Info,
        "pci: command {command:#06x} -> {wanted:#06x} readback {readback:#06x} at {location:?}",
    );

    readback as u32
}

/// Write a 16-bit word of configuration space.
///
/// # Safety
///
/// Configuration space is device control: a write can detach a BAR, retarget a
/// device's DMA, or mask its interrupts. Callers must know the device state
/// they are changing.
pub unsafe fn write_u16(location: Location, offset: u8, value: u16) {
    // A word write is a dword read-modify-write: the two bytes of `offset`
    // live in one dword, and the other half of that dword belongs to
    // neighbouring registers the caller did not name.
    let word = read_u32(location, offset);
    let shifted = (value as u32) << (8 * (offset & 3));
    let mask = 0xFFFFu32 << (8 * (offset & 3));
    // SAFETY: the caller owns the device's configuration; this write changes
    // exactly the sixteen bits the caller named.
    unsafe {
        write_u32(location, offset, (word & !mask) | shifted);
    }
}

/// Read a byte of configuration space.
pub fn read_u8(location: Location, offset: u8) -> u8 {
    let word = read_u32(location, offset);
    // Config space is little-endian, so byte `offset` lives in bits
    // `8 * (offset % 4)` of the dword the port returned.
    (word >> (8 * (offset & 3))) as u8
}

/// Read a 16-bit word of configuration space.
pub fn read_u16(location: Location, offset: u8) -> u16 {
    let word = read_u32(location, offset);
    (word >> (8 * (offset & 3))) as u16
}

/// One MSI-X capability, as the config-space header described it.
#[derive(Clone, Copy, Debug)]
pub struct MsixCapability {
    /// The config-space offset of the capability's own header; the enable bit
    /// is a word at `offset + 2`.
    pub offset: u8,
    /// How many table entries the device provides.
    pub table_size: u16,
    /// The BAR that holds the message table.
    pub table_bar: u8,
    /// The byte offset of the message table within that BAR.
    pub table_offset: u32,
}

impl MsixCapability {
    /// The MSI-X enable bit, in the Message Control word.
    const ENABLE: u16 = 1 << 15;
}

/// Read the device's MSI-X capability, if it advertises one.
pub fn msix_capability(device: &Device) -> Option<MsixCapability> {
    for capability in device.capabilities() {
        if capability.id != CAP_MSIX {
            continue;
        }
        let control = read_u16(device.location, capability.offset + 2);
        // The table pointer packs a BAR index into its low three bits and the
        // byte offset above them; the offset is dword-aligned by definition.
        let table = read_u32(device.location, capability.offset + 4);
        return Some(MsixCapability {
            offset: capability.offset,
            table_size: (control & 0x7FF) + 1,
            table_bar: (table & 0x7) as u8,
            table_offset: table & !0x3,
        });
    }
    None
}

/// Set the MSI-X enable bit, directing the device's interrupts at the message
/// table the driver programmed.
///
/// # Safety
///
/// Must be called only after every table entry the device will use has been
/// written: an enabled MSI-X device delivers to whatever address/data pair its
/// table holds, and a stale entry points at an arbitrary vector.
pub unsafe fn enable_msix(location: Location, cap_offset: u8) {
    // Read-modify-write: the control word also carries the table size and the
    // function-mask bit, none of which this call means to change.
    let control = read_u16(location, cap_offset + 2);
    // SAFETY: delegated to this function's contract; the caller has
    // programmed the table first.
    unsafe {
        write_u16(location, cap_offset + 2, control | MsixCapability::ENABLE);
    }
}

/// Probe a memory or IO BAR, returning its base and size.
///
/// The probe writes all-ones into the BAR and reads back what the device
/// leaves: the bits it *cannot* drive are the ones that encode its size. The
/// base address comes from the register's **original** value and the size from
/// the **readback** — reading the address from the readback would report the
/// probe pattern as the device's address, and restoring "what is there now"
/// after the write would restore that same pattern, permanently moving the BAR.
/// Both halves are therefore read before the write, and both are written back
/// after, so a probe that runs twice leaves the device exactly as it found it.
///
/// `index` is the BAR *register* index (0-5). A 64-bit BAR occupies two
/// registers; the caller asking about the upper half gets `None`, because that
/// half is not a region of its own.
pub fn probe_bar(location: Location, index: u8) -> Option<BarInfo> {
    if index > 5 {
        return None;
    }
    let register = 0x10 + index * 4;

    // Read the live value first: everything below overwrites it. The high
    // half of a 64-bit BAR is read *before* the low half is probed, because
    // the probe write clobbers it and "restore what is there now" would restore
    // the probe pattern.
    let original = read_u32(location, register);
    let io = original & 1 != 0;
    let kind = (original >> 1) & 0b11;
    let is_64bit = !io && kind == 0b10;

    // An unpopulated BAR reads back as zero. A device that decodes no BAR at
    // this index is not an error — most functions have gaps.
    if original == 0 {
        return None;
    }

    let original_high = if is_64bit {
        read_u32(location, register + 4)
    } else {
        0
    };

    // SAFETY: config-space writes are the only way to probe, the values
    // written are the documented all-ones probe pattern, and every register
    // touched is restored from its pre-probe value before this function
    // returns — including on every early return below.
    let (readback, readback_high) = unsafe {
        write_u32(location, register, 0xFFFF_FFFF);
        let low = read_u32(location, register);
        let high = if is_64bit {
            write_u32(location, register + 4, 0xFFFF_FFFF);
            let value = read_u32(location, register + 4);
            write_u32(location, register + 4, original_high);
            value
        } else {
            0
        };
        write_u32(location, register, original);
        (low, high)
    };

    if readback == 0 || readback == 0xFFFF_FFFF {
        return None;
    }

    // Bring-up diagnostic: every probe, with the raw values it saw. Two probes
    // of the same BAR that disagree is a bug in this function, and without the
    // four raw words there is no way to tell which one moved.
    klog!(
        Level::Info,
        "pci: probe bar{} orig {:#010x}/{:#010x} back {:#010x}/{:#010x} 64={} io={}",
        index,
        original,
        original_high,
        readback,
        readback_high,
        is_64bit,
        io,
    );

    if io {
        // IO BAR: bits 31-2 are the address, bit 1 is reserved, bit 0 is the
        // type bit. The size lives in bits 31-2.
        let masked = original & 0xFFFF_FFFC;
        let size = (!(readback & 0xFFFF_FFFC)).wrapping_add(1) & 0xFFFF_FFFC;
        if size == 0 {
            return None;
        }
        return Some(BarInfo {
            address: masked as u64,
            size: size as u64,
            io: true,
            prefetchable: false,
        });
    }

    // The address is the register's *pre-probe* value; the readback is the
    // size mask. Mixing them — taking the high half from the readback, as an
    // earlier version of this function did — yields `0xFFFFFFFF` in the upper
    // dword for any 64-bit BAR that lives below 4 GiB, and a pointer built
    // from that is non-canonical: the very first access raises #GP, before
    // the device has been touched at all.
    let address = if is_64bit {
        ((original_high as u64) << 32) | (original & 0xFFFF_FFF0) as u64
    } else {
        (original & 0xFFFF_FFF0) as u64
    };

    // The size mask, both halves for a 64-bit BAR: a region large enough to
    // reach into the upper dword would have its size bits there, and a mask
    // taken from the low dword alone would report that region as 4 GiB small.
    let mask = if is_64bit {
        ((readback_high as u64) << 32) | (readback & 0xFFFF_FFF0) as u64
    } else {
        (readback & 0xFFFF_FFF0) as u64
    };
    let size = (!mask).wrapping_add(1);
    if size == 0 {
        return None;
    }

    Some(BarInfo {
        address,
        size,
        io: false,
        prefetchable: original & (1 << 3) != 0,
    })
}

/// The result of one enumeration pass.
pub struct DeviceList {
    devices: [Option<Device>; MAX_DEVICES],
    len: usize,
}

impl DeviceList {
    /// An empty list.
    const fn new() -> Self {
        Self {
            devices: [None; MAX_DEVICES],
            len: 0,
        }
    }

    /// How many functions were recorded.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing was recorded.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The recorded functions, in discovery order.
    pub fn iter(&self) -> impl Iterator<Item = &Device> {
        self.devices[..self.len].iter().flatten()
    }

    /// The first recorded function for which `predicate` holds.
    pub fn find(&self, predicate: impl Fn(&Device) -> bool) -> Option<&Device> {
        self.iter().find(|device| predicate(device))
    }

    /// Record one function, if the list has room.
    fn push(&mut self, device: Device) {
        if self.len < MAX_DEVICES {
            self.devices[self.len] = Some(device);
            self.len += 1;
        }
    }
}

/// Walk bus 0 and every bus a bridge on it leads to, recording each function.
pub fn enumerate() -> DeviceList {
    let mut list = DeviceList::new();
    let mut buses = [0u8; MAX_BUSES];
    let mut bus_count = 0usize;
    let mut cursor = 0usize;

    buses[bus_count] = 0;
    bus_count += 1;

    while cursor < bus_count {
        let bus = buses[cursor];
        cursor += 1;
        scan_bus(bus, &mut list, &mut buses, &mut bus_count);
    }

    // One line per function the walk found. A driver that starts no device has
    // two possible causes — the bus walk saw nothing, or the walk saw the
    // device and the transport refused it — and without this line the two are
    // indistinguishable from the outside.
    klog!(Level::Info, "pci: {} function(s) enumerated", list.len());
    for device in list.iter() {
        klog!(
            Level::Info,
            "pci: {} vendor {:#06x} device {:#06x} class {:#04x}",
            device.location.formatted(),
            device.vendor_id,
            device.device_id,
            device.class_code,
        );
    }

    list
}

/// Scan one bus, recording its functions and queueing the buses its bridges own.
fn scan_bus(bus: u8, list: &mut DeviceList, buses: &mut [u8; MAX_BUSES], bus_count: &mut usize) {
    for slot in 0..SLOTS_PER_BUS {
        let present = read_u32(Location { bus, slot, func: 0 }, 0x00);
        if present == 0xFFFF_FFFF {
            // Slot 0 of an empty function reads all-ones; nothing else is
            // there either. Skipping the rest of the slot is the point of the
            // check — seven more reads for an answer we already have.
            continue;
        }

        // Header type 0x80 means "functions 1-7 exist too". Without this the
        // second virtio disk — a separate function in the same slot — would
        // never be seen.
        let header = read_u8(Location { bus, slot, func: 0 }, 0x0E);
        let multi = header & 0x80 != 0;
        let functions = if multi { FUNCS_PER_SLOT } else { 1 };

        for func in 0..functions {
            let location = Location { bus, slot, func };
            if let Some(device) = read_device(location) {
                if device.is_pci_bridge() {
                    // The secondary bus number is one byte at config 0x19.
                    let secondary = read_u8(location, 0x19);
                    queue_bus(secondary, buses, bus_count);
                }
                list.push(device);
            }
        }
    }
}

/// Add `bus` to the walk's bus list, if it is new and the list has room.
fn queue_bus(bus: u8, buses: &mut [u8; MAX_BUSES], bus_count: &mut usize) {
    if buses[..*bus_count].contains(&bus) {
        return;
    }
    if *bus_count < MAX_BUSES {
        buses[*bus_count] = bus;
        *bus_count += 1;
    }
}

/// Read the standard header of one function, or `None` if it is absent.
fn read_device(location: Location) -> Option<Device> {
    let vendor_device = read_u32(location, 0x00);
    let vendor_id = (vendor_device & 0xFFFF) as u16;
    if vendor_id == 0xFFFF {
        // 0xFFFF is the "nothing decodes this address" pattern: an absent
        // function, not a device with vendor 0xFFFF.
        return None;
    }

    let class_codes = read_u32(location, 0x08);
    let header = read_u8(location, 0x0E);

    Some(Device {
        location,
        vendor_id,
        device_id: (vendor_device >> 16) as u16,
        revision: (class_codes & 0xFF) as u8,
        prog_if: ((class_codes >> 8) & 0xFF) as u8,
        subclass: ((class_codes >> 16) & 0xFF) as u8,
        class_code: ((class_codes >> 24) & 0xFF) as u8,
        header_type: header & 0x7F,
        multi_function: header & 0x80 != 0,
        interrupt_line: read_u8(location, 0x3C),
    })
}
