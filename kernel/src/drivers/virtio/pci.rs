//! The VirtIO 1.0 PCI transport: finding the structures, mapping them, and
//! driving a device through initialization.
//!
//! # The four structures
//!
//! A modern VirtIO device advertises its control interface as a list of
//! vendor-specific capabilities. Each one names a BAR, an offset, and a length,
//! and `cfg_type` says which structure it is:
//!
//! | type | structure | what it is for |
//! | --- | --- | --- |
//! | 1 | common | features, status, queue addresses, queue enable |
//! | 2 | notify  | "look at queue N now" |
//! | 3 | ISR     | interrupt status (unused here: this driver polls) |
//! | 4 | device  | the device's own configuration — a disk's capacity |
//!
//! # The status sequence is checked, not assumed
//!
//! Initialization is `ACKNOWLEDGE → DRIVER → FEATURES_OK → DRIVER_OK`, and
//! every step is read back. Reading back is what turns "the device is wedged"
//! from a hang into an error: a device that does not take `FEATURES_OK` clears
//! it, and this transport reports [`VirtioError::NegotiationFailed`] instead of
//! writing queue addresses a device that is not listening will never use.

use super::{VirtioError, faults};
use crate::drivers::mmio;
use crate::drivers::pci::{
    self, Location, VIRTIO_CAP_COMMON, VIRTIO_CAP_DEVICE, VIRTIO_CAP_ISR, VIRTIO_CAP_NOTIFY,
    VirtioCapability,
};
use crate::klog;
use crate::logger::Level;
use crate::mm::{address_space, direct_map, page_table};

/// Device status: reset. Writing zero is the *only* way to reset a VirtIO
/// device, and reading zero back is how the driver knows the reset finished.
pub const STATUS_RESET: u8 = 0x00;

/// Device status: the device has been acknowledged.
pub const STATUS_ACKNOWLEDGE: u8 = 0x01;

/// Device status: the driver has taken over.
pub const STATUS_DRIVER: u8 = 0x02;

/// Device status: the driver is done initializing.
pub const STATUS_DRIVER_OK: u8 = 0x04;

/// Device status: the negotiated features are acceptable to the device.
pub const STATUS_FEATURES_OK: u8 = 0x08;

/// Device status: the device has irrecoverably failed and needs a reset.
pub const STATUS_FAILED: u8 = 0x80;

/// Common-configuration byte offsets, from the VirtIO 1.0 specification.
mod common {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0C;
    pub const NUM_QUEUES: usize = 0x12;
    pub const DEVICE_STATUS: usize = 0x14;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_ENABLE: usize = 0x1C;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1E;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_AVAIL: usize = 0x28;
    pub const QUEUE_USED: usize = 0x30;

    /// Bytes of common configuration this driver reads. A capability claiming
    /// less than this is a device whose structure predates the specification.
    pub const REQUIRED_BYTES: u32 = 0x38;
}

/// How many times a status readback is retried before the device is declared
/// wedged.
///
/// A reset is supposed to complete in microseconds. The bound is generous
/// because the thing being waited for is *the device*, and the thing being
/// avoided is a boot that never finishes — those are not symmetric.
const STATUS_RETRIES: u32 = 1_000_000;

/// The mapped control structures of one VirtIO device.
pub struct Transport {
    location: Location,
    common: *mut u8,
    notify: *mut u8,
    /// Bytes per step between one queue's notify register and the next.
    notify_multiplier: u32,
    device_config: *mut u8,
    /// The interrupt-status register, read (and thereby cleared) only when a
    /// timeout has to say whether the device signalled at all.
    isr: *mut u8,
    /// The queue-notify offset this device's single queue uses.
    queue_notify_off: u16,
}

impl Transport {
    /// Map a device's control structures and select its first queue.
    ///
    /// This does *not* initialize the device: initialization needs a virtqueue
    /// to point it at, so it happens in [`Transport::start_device`] once the
    /// caller has allocated one.
    pub fn open(device: &pci::Device) -> Result<Self, VirtioError> {
        // The device must be allowed to answer memory accesses and to run its
        // DMA engine before anything else: without those bits the BARs are not
        // even mapped by the machine, and a published request is never
        // fetched. Doing it here — before the structures are mapped and before
        // the device is given any status — is the only order that works.
        pci::enable_transport(device.location);

        let capabilities =
            pci::virtio_capabilities(device).ok_or(VirtioError::NoModernTransport)?;

        let common = map_structure(
            device.location,
            &capabilities,
            VIRTIO_CAP_COMMON,
            common::REQUIRED_BYTES,
        )?;
        let notify = map_structure(device.location, &capabilities, VIRTIO_CAP_NOTIFY, 4)?;
        let isr = map_structure(device.location, &capabilities, VIRTIO_CAP_ISR, 4)?;
        let notify_capability = capabilities
            .find(VIRTIO_CAP_NOTIFY)
            .ok_or(VirtioError::MissingCapability(VIRTIO_CAP_NOTIFY))?;
        let device_config = map_structure(device.location, &capabilities, VIRTIO_CAP_DEVICE, 8);

        // The ISR is mapped and kept: this driver polls, but a timeout that
        // has to say *where* the device stopped needs to know whether it
        // signalled at all. Reading it once clears any interrupt the device
        // has already raised, which is the right thing to do with an interrupt
        // nobody is going to service — and the reading happens only in the
        // timeout path, so nothing else consumes it by accident.
        Ok(Self {
            location: device.location,
            common,
            notify,
            notify_multiplier: notify_capability.notify_off_multiplier,
            device_config: device_config.unwrap_or(core::ptr::null_mut()),
            isr,
            queue_notify_off: 0,
        })
    }

    /// The device's interrupt-status register, as it stands now.
    ///
    /// Reading this clears the bits it reports; it is therefore a diagnostic,
    /// not a status check, and is called from the timeout path alone.
    pub fn interrupt_status(&self) -> u8 {
        // SAFETY: `isr` points at the mapped interrupt-status structure, four
        // bytes wide, and the mapping outlives this transport.
        unsafe { mmio::read_u8(self.isr, 0) }
    }

    /// The PCI function this transport drives.
    pub const fn location(&self) -> Location {
        self.location
    }

    /// The device's status register.
    ///
    /// A device whose status reads as lost — [`super::FaultHooks::status_lost`] — is
    /// indistinguishable from a device that was never there, which is exactly
    /// the case initialization has to refuse.
    pub fn status(&self) -> u8 {
        if faults()
            .status_lost
            .load(core::sync::atomic::Ordering::Acquire)
        {
            return STATUS_RESET;
        }
        // SAFETY: `common` points at the mapped common configuration, whose
        // status field is at the specified offset, and the mapping outlives
        // this transport.
        unsafe { mmio::read_u8(self.common, common::DEVICE_STATUS) }
    }

    /// Write the device's status register.
    pub fn set_status(&self, value: u8) {
        // SAFETY: as `status`.
        unsafe { mmio::write_u8(self.common, common::DEVICE_STATUS, value) }
    }

    /// Reset the device and wait for it to acknowledge the reset.
    ///
    /// Returns whether the device confirmed it. A device that never clears its
    /// status is not reset — it is still holding the rings the driver is about
    /// to give back, which is why every caller treats `false` as fatal to the
    /// operation rather than "close enough".
    pub fn reset(&self) -> bool {
        self.set_status(STATUS_RESET);
        mmio::fence();

        for _ in 0..STATUS_RETRIES {
            if self.status() == STATUS_RESET {
                return true;
            }
            core::hint::spin_loop();
        }

        false
    }

    /// Read both feature words from the device.
    pub fn device_features(&self) -> super::Features {
        // SAFETY: as `status`; the select register chooses which half of the
        // 64-bit feature set the read returns.
        unsafe {
            mmio::write_u32(self.common, common::DEVICE_FEATURE_SELECT, 0);
            let low = mmio::read_u32(self.common, common::DEVICE_FEATURE);
            mmio::write_u32(self.common, common::DEVICE_FEATURE_SELECT, 1);
            let high = mmio::read_u32(self.common, common::DEVICE_FEATURE);
            super::Features { low, high }
        }
    }

    /// Publish the driver's accepted feature set.
    ///
    /// # Safety
    ///
    /// The value must be a subset of what the device offered. A feature the
    /// device did not set is one whose behaviour it does not implement, and
    /// claiming it makes the driver read structures that are not there.
    pub unsafe fn set_driver_features(&self, features: super::Features) {
        // SAFETY: delegated to this function's contract, plus `status`'s.
        unsafe {
            mmio::write_u32(self.common, common::DRIVER_FEATURE_SELECT, 0);
            mmio::write_u32(self.common, common::DRIVER_FEATURE, features.low);
            mmio::write_u32(self.common, common::DRIVER_FEATURE_SELECT, 1);
            mmio::write_u32(self.common, common::DRIVER_FEATURE, features.high);
        }
        mmio::fence();
    }

    /// How many queues the device has.
    pub fn num_queues(&self) -> u16 {
        // SAFETY: as `status`.
        unsafe { mmio::read_u16(self.common, common::NUM_QUEUES) }
    }

    /// Point queue `index` at `desc`, `avail`, and `used`, and enable it.
    ///
    /// The returned value is the queue's notify offset, which the caller keeps
    /// for [`Transport::notify`].
    pub fn setup_queue(
        &mut self,
        index: u16,
        size: u16,
        desc_phys: u64,
        avail_phys: u64,
        used_phys: u64,
    ) -> Result<u16, VirtioError> {
        // SAFETY: as `status`.
        unsafe {
            mmio::write_u16(self.common, common::QUEUE_SELECT, index);

            let offered = mmio::read_u16(self.common, common::QUEUE_SIZE);
            if offered == 0 {
                return Err(VirtioError::NoQueue);
            }
            // The driver may shrink a queue but not grow it. Taking the
            // smaller of the two is the spec's rule; the driver's own bound is
            // what keeps the ring frames one page each.
            let chosen = size.min(offered).min(super::queue::MAX_QUEUE_SIZE);
            if chosen == 0 {
                return Err(VirtioError::NoQueue);
            }
            mmio::write_u16(self.common, common::QUEUE_SIZE, chosen);

            mmio::write_u64(self.common, common::QUEUE_DESC, desc_phys);
            mmio::write_u64(self.common, common::QUEUE_AVAIL, avail_phys);
            mmio::write_u64(self.common, common::QUEUE_USED, used_phys);
            mmio::fence();

            mmio::write_u16(self.common, common::QUEUE_ENABLE, 1);

            let notify_off = mmio::read_u16(self.common, common::QUEUE_NOTIFY_OFF);
            self.queue_notify_off = notify_off;

            // Read every write back. A queue that was configured but whose
            // configuration did not *stick* is indistinguishable from a queue
            // the device ignored, unless the values are read again from the
            // device's own registers — and the difference decides whether the
            // next thing to check is the notify or the queue setup.
            let size_back = mmio::read_u16(self.common, common::QUEUE_SIZE);
            let enable_back = mmio::read_u16(self.common, common::QUEUE_ENABLE);
            let desc_back = mmio::read_u64(self.common, common::QUEUE_DESC);
            let avail_back = mmio::read_u64(self.common, common::QUEUE_AVAIL);
            let used_back = mmio::read_u64(self.common, common::QUEUE_USED);
            klog!(
                Level::Info,
                "virtio: queue {index} readback size {size_back} enable {enable_back} desc {desc_back:#x} (wrote {desc_phys:#x}) avail {avail_back:#x} (wrote {avail_phys:#x}) used {used_back:#x} (wrote {used_phys:#x})"
            );

            // The queue is now live, and the notify offset is what decides
            // whether a later kick reaches it: the device derives the queue
            // from `notify_off * multiplier` inside the notify structure, so a
            // value that is wrong by even one unit notifies a queue that does
            // not exist — and the symptom is a request that never completes,
            // with no error anywhere. Recording the pair once, here, is what
            // makes that failure distinguishable from a silent device.
            klog!(
                Level::Info,
                "virtio: queue {index} size {chosen} notify_off {notify_off} multiplier {}",
                self.notify_multiplier
            );
        }
        mmio::fence();

        Ok(self.queue_notify_off)
    }

    /// Tell the device to look at queue 0.
    pub fn notify(&self) {
        let offset = self.queue_notify_off as u32 * self.notify_multiplier;
        // SAFETY: the notify structure was mapped with room for at least one
        // register, and the computed offset is the one the capability's
        // multiplier defines. The value written is the queue index, which is 0:
        // this driver runs one queue per device.
        unsafe { mmio::write_u32(self.notify, offset as usize, 0) };
        mmio::fence();
    }

    /// The device's own configuration structure, if it advertised one.
    pub fn device_config(&self) -> Option<*mut u8> {
        if self.device_config.is_null() {
            None
        } else {
            Some(self.device_config)
        }
    }

    /// Run the initialization sequence up to `DRIVER_OK`, negotiating
    /// `wanted`.
    ///
    /// `wanted` is what the driver is willing to accept. A feature the device
    /// does not offer is simply not negotiated; a feature the driver *requires*
    /// must be in `wanted`'s `high` word as `VERSION_1`, and its absence is
    /// reported as [`VirtioError::NegotiationFailed`].
    pub fn start_device(&self, wanted: super::Features) -> Result<super::Features, VirtioError> {
        if !self.reset() {
            return Err(VirtioError::DeviceNeedsReset);
        }

        self.set_status(STATUS_ACKNOWLEDGE);
        if self.status() & STATUS_ACKNOWLEDGE == 0 {
            return Err(VirtioError::DeviceNeedsReset);
        }

        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        if self.status() & STATUS_DRIVER == 0 {
            return Err(VirtioError::DeviceNeedsReset);
        }

        let offered = self.device_features();
        let accepted = offered.intersect(wanted);

        // `VERSION_1` is required: without it the device is speaking the
        // legacy layout under a modern capability list, and every offset this
        // transport uses would be wrong.
        if !accepted.high_has(super::FEATURE_VERSION_1) {
            self.set_status(STATUS_FAILED);
            return Err(VirtioError::NegotiationFailed);
        }

        // SAFETY: `accepted` is the intersection of what the device offered and
        // what the driver wants, which is exactly the contract.
        unsafe { self.set_driver_features(accepted) };
        mmio::fence();

        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if self.status() & STATUS_FEATURES_OK == 0 {
            // The device rejected the feature set. `FAILED` says so to the
            // device as well as to the caller.
            self.set_status(STATUS_FAILED);
            return Err(VirtioError::NegotiationFailed);
        }

        Ok(accepted)
    }

    /// Mark the driver live. Called after the queues are set up, because a
    /// device told `DRIVER_OK` before its queues exist has nothing to answer
    /// with.
    pub fn driver_ok(&self) {
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
        mmio::fence();
        // Read it back: `DRIVER_OK` that did not stick means the device will
        // never look at the queue, and the symptom — a request published to a
        // device that is not running — is the same as a broken notify.
        klog!(
            Level::Info,
            "virtio: status after DRIVER_OK {:#x}",
            self.status()
        );
    }
}

/// Map one advertised structure, checking it against the BAR it claims.
fn map_structure(
    location: Location,
    capabilities: &pci::VirtioCapabilities,
    cfg_type: u8,
    required_bytes: u32,
) -> Result<*mut u8, VirtioError> {
    let capability: VirtioCapability = capabilities
        .find(cfg_type)
        .ok_or(VirtioError::MissingCapability(cfg_type))?;

    if capability.length < required_bytes {
        return Err(VirtioError::BarTooSmall);
    }

    let bar = pci::probe_bar(location, capability.bar).ok_or(VirtioError::BarTooSmall)?;
    if bar.io {
        return Err(VirtioError::NotMemoryMapped);
    }
    if capability.offset as u64 + required_bytes as u64 > bar.size {
        return Err(VirtioError::OffsetOutOfRange);
    }

    let physical = bar.address + capability.offset as u64;
    let virtual_address = direct_map::phys_to_virt(physical);

    // Bring-up diagnostic: which structure, out of which BAR, at what address.
    // The first failure this driver ever saw was a non-canonical mapping that
    // faulted before a single register was read, and the only evidence of it
    // was an instruction pointer — one line naming the BAR and the computed
    // pair would have located it immediately.
    klog!(
        Level::Info,
        "virtio: cap type {} bar {} phys {:#x} size {:#x} virt {:#x}",
        cfg_type,
        capability.bar,
        physical,
        bar.size,
        virtual_address as u64
    );

    // Follow the APIC's convention: a kernel mapping named explicitly rather
    // than borrowed from the bootloader's direct map. If the direct map already
    // covers this address the write is the same leaf it already holds; if it
    // does not — a BAR above the memory the bootloader chose to map — this is
    // what makes the device reachable at all.
    address_space::map_kernel_mmio(virtual_address as u64, physical)
        .map_err(|_| VirtioError::MapFailed)?;

    // Verification: walk the mapping through *both* the page tables CR3 is
    // using and the kernel space's own. A mapping installed in a hierarchy the
    // active CR3 does not share is invisible to every access the CPU makes —
    // which is exactly the failure mode this check exists to catch. It is
    // silent when it passes: the check is the point, and sixteen four-line
    // dumps of a value that is always the same are a log nobody reads.
    let active = crate::arch::x86_64::paging::current_page_directory();
    let kernel = address_space::kernel_space().pml4_phys();
    let through_active =
        page_table::translate(&page_table::PageTable::at(active), virtual_address as u64);
    let through_kernel =
        page_table::translate(&page_table::PageTable::at(kernel), virtual_address as u64);
    if active != kernel || through_active != Some(physical) || through_kernel != Some(physical) {
        klog!(
            Level::Warn,
            "virtio: cap type {cfg_type} map mismatch: active pml4 {active:#x} kernel pml4 {kernel:#x} leaf(active)={through_active:#x?} leaf(kernel)={through_kernel:#x?} expected {physical:#x}"
        );
    }

    Ok(virtual_address)
}
