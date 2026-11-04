//! The VirtIO 1.0 driver: PCI transport, virtqueue, and block device.
//!
//! # Why only the modern transport
//!
//! VirtIO has two PCI transports. The *legacy* one is a fixed 40-byte I/O BAR
//! whose layout predates the specification and differs per device type; the
//! *modern* one is a capability list pointing at four structures (common,
//! notify, ISR, device configuration) whose layout is specified once and does
//! not vary. This kernel implements the modern transport only.
//!
//! The cost of that choice is that a legacy-only device is not driven — and on
//! the reference machine there is no such device: QEMU's `virtio-blk-pci` is
//! *transitional*, meaning it keeps the legacy device ID so old guests find it
//! *and* advertises the modern capability list. The driver matches on the
//! capability list, not the device ID, precisely because the ID cannot tell the
//! two apart.
//!
//! The benefit is that every field the driver reads is a field the
//! specification defines. A legacy transport would mean guessing at offsets
//! that QEMU happens to implement, which is the shape of bug that only appears
//! on a machine nobody is testing.
//!
//! # Fault injection
//!
//! [`faults`] exposes four switches that make a device misbehave on demand.
//! They exist because the failure paths a block driver must have — a device
//! that never answers, a device that answers wrongly, a device that is taken
//! away mid-transfer — cannot be produced by a working device on purpose, and
//! a recovery path that has never run is a recovery path that does not work.
//! They are always compiled (a test-only path would be a second
//! implementation), cost one relaxed load each, and are armed by the M5 suites
//! and by nothing else.

use core::sync::atomic::{AtomicBool, Ordering};

pub mod blk;
pub mod net;
pub mod pci;
pub mod queue;

/// The four ways this kernel asks a device to misbehave.
pub struct FaultHooks {
    /// The device's status register reads as zero, so initialization cannot
    /// confirm it took. Exercises the "device is not really there" path.
    pub status_lost: AtomicBool,
    /// Completions never appear, however long the driver waits. Exercises the
    /// timeout, device reset, and reinitialization path.
    pub suppress_completions: AtomicBool,
    /// The device reports a completion naming a descriptor the queue does not
    /// have. Exercises the "untrusted device input must not become an index"
    /// path.
    pub corrupt_completion: AtomicBool,
    /// The owner of an in-flight request goes away. Exercises the shutdown
    /// path that has to stop the device before returning buffers.
    pub owner_exited: AtomicBool,
}

impl FaultHooks {
    /// Clear every hook, returning the device to well-behaved.
    pub fn clear(&self) {
        self.status_lost.store(false, Ordering::Release);
        self.suppress_completions.store(false, Ordering::Release);
        self.corrupt_completion.store(false, Ordering::Release);
        self.owner_exited.store(false, Ordering::Release);
    }
}

/// The kernel-wide fault hooks.
static FAULTS: FaultHooks = FaultHooks {
    status_lost: AtomicBool::new(false),
    suppress_completions: AtomicBool::new(false),
    corrupt_completion: AtomicBool::new(false),
    owner_exited: AtomicBool::new(false),
};

/// The fault hooks, for the suites that arm them.
pub fn faults() -> &'static FaultHooks {
    &FAULTS
}

/// Whether any fault hook is armed.
///
/// The hooks make the *device* misbehave, so they are only meaningful to code
/// that actually reaches the device. A cache hit never does: the M5 suites arm
/// a hook and read a sector the block layer has already served once, and a
/// cache that answered from memory would report the device's injected failure
/// as a success — the fault proven only to be bypassed. The block layer
/// consults this before it looks at a line, so an armed hook always costs a
/// real round trip.
pub fn faults_armed() -> bool {
    let hooks = &FAULTS;
    hooks.status_lost.load(Ordering::Acquire)
        || hooks.suppress_completions.load(Ordering::Acquire)
        || hooks.corrupt_completion.load(Ordering::Acquire)
        || hooks.owner_exited.load(Ordering::Acquire)
}

/// Whether an owner has abandoned its in-flight request.
///
/// Read from inside a poll loop, which is the only place a cancellation can be
/// observed: the request is already submitted, so "cancel" cannot mean "do not
/// send it" — it means "stop waiting, stop the device, and give the buffers
/// back safely".
pub fn owner_exited() -> bool {
    FAULTS.owner_exited.load(Ordering::Acquire)
}

/// Report that a device did not confirm its reset.
///
/// Teardown continues either way — the frames have to go back — but a device
/// that never acknowledges a reset may still be DMAing into them, and the
/// difference between "stopped" and "maybe still moving" is exactly the kind
/// of thing a serial log should carry when something later goes wrong.
pub(crate) fn klog_reset_refused() {
    crate::klog!(
        crate::logger::Level::Warn,
        "virtio: device did not confirm reset; teardown continues anyway"
    );
}

/// Why the transport would not come up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VirtioError {
    /// The function advertises no VirtIO capability list — a legacy-only
    /// device, which this driver does not implement.
    NoModernTransport,
    /// A capability the driver requires is missing from the list.
    MissingCapability(u8),
    /// The structure's BAR is an IO BAR; this driver maps memory only.
    NotMemoryMapped,
    /// The BAR is smaller than the structure the capability points at.
    BarTooSmall,
    /// The capability's offset falls outside its BAR.
    OffsetOutOfRange,
    /// The kernel could not map the BAR into its address space.
    MapFailed,
    /// The device cleared `FEATURES_OK`: it cannot offer the features this
    /// driver requires.
    NegotiationFailed,
    /// The device set `DEVICE_NEEDS_RESET` instead of acknowledging the driver.
    DeviceNeedsReset,
    /// The queue index the driver selected does not exist.
    NoQueue,
    /// Writing the queue's addresses or enabling it did not stick.
    QueueSetupFailed,
}

impl VirtioError {
    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            VirtioError::NoModernTransport => "V_NO_MODERN_TRANSPORT",
            VirtioError::MissingCapability(_) => "V_MISSING_CAPABILITY",
            VirtioError::NotMemoryMapped => "V_NOT_MEMORY_MAPPED",
            VirtioError::BarTooSmall => "V_BAR_TOO_SMALL",
            VirtioError::OffsetOutOfRange => "V_OFFSET_OUT_OF_RANGE",
            VirtioError::MapFailed => "V_MAP_FAILED",
            VirtioError::NegotiationFailed => "V_NEGOTIATION_FAILED",
            VirtioError::DeviceNeedsReset => "V_DEVICE_NEEDS_RESET",
            VirtioError::NoQueue => "V_NO_QUEUE",
            VirtioError::QueueSetupFailed => "V_QUEUE_SETUP_FAILED",
        }
    }
}

/// A negotiated feature set, as two 32-bit words.
///
/// Features are 64 bits split across two selects, and this kernel keeps them
/// split because every consumer below asks about one word at a time: block
/// features live in word 0, and `VIRTIO_F_VERSION_1` — the only thing this
/// driver requires from word 1 — is the first bit of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Features {
    /// Features 0-31.
    pub low: u32,
    /// Features 32-63.
    pub high: u32,
}

impl Features {
    /// Whether a word-0 feature is present.
    pub const fn low_has(self, bit: u32) -> bool {
        self.low & (1 << bit) != 0
    }

    /// Whether a word-1 feature is present (bit 0 of this word is feature 32).
    pub const fn high_has(self, bit: u32) -> bool {
        self.high & (1 << bit) != 0
    }

    /// The intersection with a wanted set.
    pub const fn intersect(self, wanted: Features) -> Features {
        Features {
            low: self.low & wanted.low,
            high: self.high & wanted.high,
        }
    }
}

/// `VIRTIO_F_VERSION_1`: the device speaks VirtIO 1.0, with the modern
/// structure layouts and no legacy quirks.
///
/// Bit 32 of the 64-bit feature word, so bit 0 of word 1. This driver *requires*
/// it: without it, "modern" means nothing, and the field offsets this code
/// uses are not the ones the device implements.
pub const FEATURE_VERSION_1: u32 = 0;

/// `VIRTIO_BLK_F_RO`: the device is read-only.
///
/// A device that sets this refuses writes at the spec level, and this driver
/// reports them as [`nova_fs::diskfs::DeviceError::ReadOnly`] rather than
/// submitting a request the device will reject.
pub const FEATURE_BLK_RO: u32 = 5;

/// `VIRTIO_BLK_F_FLUSH`: the device implements the flush request.
///
/// Without it, `flush` is not a request the device can honour, and reporting a
/// flush as successful would be a durability claim the device never made.
pub const FEATURE_BLK_FLUSH: u32 = 9;
