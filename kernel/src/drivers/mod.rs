//! The kernel's device drivers, and the one place they are brought up.
//!
//! # The M5 route
//!
//! `reports/NovaOS.md` §13.2 describes the driver route as PCI → VirtIO →
//! block → VFS, and that is the module order here:
//!
//! ```text
//! pci        find functions, read config space, probe BARs
//! virtio     take a device over: transport, queue, block or network
//! block      the disks that were found, a sector cache, and the counters
//! net        the NIC that was found, and the bounded packet path
//! ```
//!
//! What this module adds on top is the *join to the filesystem*: a disk is
//! useless to the rest of the kernel until something can open a path on it, so
//! [`init`] mounts each disk into the file server as well as starting it. The
//! network device is joined to the kernel-owned network service instead.
//!
//! # Capability discipline
//!
//! The plan is explicit that ordinary applications do not get arbitrary MMIO,
//! port, or IRQ authority (`reports/NovaOS.md` §13.1). Nothing in this module
//! is reachable from user space: the ports live in `pci`, the mappings in
//! `virtio`, and the only thing the file server hands a client is a path scope
//! over a mount — which is checked per request, exactly like the RAM
//! filesystem's own scopes.

pub mod block;
pub mod display;
pub mod input;
pub mod mmio;
pub mod net;
pub mod pci;
pub mod virtio;

use crate::klog;
use crate::logger::Level;

/// The directory the writable test disk is mounted at.
///
/// `/media/*` is where the plan puts disks (`reports/NovaOS.md` §14.5), and the
/// two mount points are named here so the driver and the M5 suites cannot
/// drift apart — a suite that mounted at `/mnt` would be testing a namespace
/// the kernel never creates.
pub const MOUNT_TEST: &[u8] = b"/media/test";

/// The directory the read-only disk is mounted at.
pub const MOUNT_RO: &[u8] = b"/media/ro";

/// The parent of both mount points.
///
/// Created in the RAM filesystem so that `/media` stats as a directory. The
/// mounts themselves are routed *before* the RAM filesystem sees a path, so
/// they do not appear in a listing of `/media`; that is a known gap, recorded
/// here rather than hidden, and it is what a VFS layer above both stores would
/// close.
const MOUNT_PARENT: &[u8] = b"/media";

/// Bring up every device the kernel can drive, and mount what they hold.
///
/// # Safety
///
/// Must run once, from the boot path, with interrupts masked, after `mm::init`
/// (the drivers allocate frames and map BARs) and after `services::init` (they
/// mount into the file server). It is called from `main` between the two.
pub unsafe fn init() {
    // The PS/2 controller is a device the input service needs a source for.
    // Bringing it up before the block path means a headless machine without
    // disks still has a working input driver.
    //
    // SAFETY: the boot path runs once with interrupts masked and with IO-port
    // access initialised by the architecture. See `input::init`.
    unsafe { input::init() };

    let disks = block::init();

    // The NIC is brought up even if no disk exists: a headless machine with a
    // network device still has a network service, and a machine with neither is
    // simply a kernel with no device services.
    let _nics = net::init();

    if disks == 0 {
        return;
    }

    // SAFETY: delegated to this function's contract; `services::init` has
    // installed the state, and nothing has dispatched against it yet.
    let state = unsafe { crate::services::state() };

    // A missing `/media` would make every mount unreachable through a client
    // that stat'd its way down the path first. The failure is ignored because
    // an existing directory is not an error worth stopping for.
    let _ = state.files.fs_mut().create_dir(MOUNT_PARENT);

    // The first disk is mounted read-write. Its writability is the device's to
    // refuse — a device that reports itself read-only fails here, loudly,
    // rather than accepting writes that will never reach the medium.
    if disks > 0 {
        match state.files.mount_disk(0, MOUNT_TEST, false) {
            Ok(()) => klog!(Level::Info, "vfs: disk 0 mounted rw at /media/test"),
            Err(error) => klog!(
                Level::Warn,
                "vfs: disk 0 could not be mounted at /media/test: {}",
                error
            ),
        }
    }

    // The second disk is mounted read-only *and* is expected to report itself
    // read-only. Both are needed: the device flag is what stops the bytes, the
    // mount flag is what tells a caller why, and a mount that trusted only the
    // device would answer a write with a device error instead of an authority
    // refusal.
    if disks > 1 {
        match state.files.mount_disk(1, MOUNT_RO, true) {
            Ok(()) => klog!(Level::Info, "vfs: disk 1 mounted ro at /media/ro"),
            Err(error) => klog!(
                Level::Warn,
                "vfs: disk 1 could not be mounted at /media/ro: {}",
                error
            ),
        }
    }
}
