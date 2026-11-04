//! The kernel's network layer: the one NIC the kernel drives, and the bounded
//! packet path the network service uses.
//!
//! # What this layer is for
//!
//! The wire arithmetic lives in `nova-net`, where it is host-tested. The VirtIO
//! device lives in `drivers::virtio::net`. This module is the join: it finds the
//! NIC at boot, keeps it in one static, and exposes packet operations as
//! closures so no caller retains a mutable reference to the device.
//!
//! # No user-space authority
//!
//! Nothing here hands a client an MMIO address, a queue address, or a raw NIC
//! handle. A client asks `SERVICE_NET`, and the service calls these functions on
//! the caller's behalf. The packet path is bounded by the driver's staging frames
//! and the protocol's payload budget.
//!
//! # Limitations stated in code
//!
//! This is not DHCP, TCP, IPv6, or a general socket stack. The QEMU user network
//! supplies fixed guest/gateway/DNS addresses, and the driver uses them. That is
//! enough to prove link reachability through an emulated NIC; it is not a claim
//! that NovaOS implements a full internet stack.

use core::cell::UnsafeCell;

use nova_net::Ipv4Addr;

use crate::drivers::pci;
use crate::drivers::virtio::net::{LinkInfo, NetError, VirtioNet};
use crate::klog;
use crate::logger::Level;

/// NICs the kernel keeps. One is what the reference machine exposes.
pub const MAX_NET_DEVICES: usize = 1;

/// The devices the kernel found.
struct NetState {
    devices: [Option<VirtioNet>; MAX_NET_DEVICES],
    len: usize,
}

impl NetState {
    const fn new() -> Self {
        Self {
            devices: [None],
            len: 0,
        }
    }
}

/// The network state, behind an `UnsafeCell`.
struct SingleNetState {
    inner: UnsafeCell<NetState>,
}

// SAFETY: the kernel mutates this state in place from code running on one CPU
// with interrupts masked, exactly like the block and service state.
unsafe impl Sync for SingleNetState {}

static STATE: SingleNetState = SingleNetState {
    inner: UnsafeCell::new(NetState::new()),
};

/// The network state.
///
/// # Safety
///
/// The caller must be kernel code running with interrupts masked on the boot
/// CPU, which is every caller in this tree.
unsafe fn state() -> &'static mut NetState {
    // SAFETY: delegated to this function's contract.
    unsafe { &mut *STATE.inner.get() }
}

/// Find the VirtIO network device and take it over.
///
/// Returns how many NICs are live. A device that fails to initialize is skipped
/// with a log line: a missing network is a missing service, not a kernel that
/// will not start.
pub fn init() -> usize {
    let devices = pci::enumerate();

    let mut found = 0usize;
    for device in devices.iter() {
        if found == MAX_NET_DEVICES {
            break;
        }
        if !device.is_virtio_net() {
            continue;
        }
        match VirtioNet::open(device) {
            Ok(net_device) => {
                let info = net_device.link_info();
                klog!(
                    Level::Info,
                    "virtio-net: {} at {} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link {}",
                    device.device_id,
                    device.location.formatted(),
                    info.mac.0[0],
                    info.mac.0[1],
                    info.mac.0[2],
                    info.mac.0[3],
                    info.mac.0[4],
                    info.mac.0[5],
                    if info.link_up { "up" } else { "down" },
                );
                // SAFETY: called once from boot with interrupts masked, before
                // any reader of the table exists.
                let state = unsafe { state() };
                state.devices[state.len] = Some(net_device);
                state.len += 1;
                found += 1;
            }
            Err(error) => {
                klog!(
                    Level::Warn,
                    "virtio-net: {} at {} refused to start: {}",
                    device.device_id,
                    device.location.formatted(),
                    error,
                );
            }
        }
    }

    if found == 0 {
        klog!(Level::Warn, "virtio-net: no network device was started");
    }

    found
}

/// How many NICs are live.
pub fn device_count() -> usize {
    // SAFETY: read-only access from kernel code with interrupts masked.
    unsafe { state().len }
}

/// Run `f` with the NIC.
///
/// A closure rather than a long-lived reference: the device table is one
/// static, and two mutable references to it would be aliases.
pub fn with_net<T>(f: impl FnOnce(&mut VirtioNet) -> T) -> Result<T, NetError> {
    // SAFETY: as `device_count`; the borrow ends where `f` does.
    let state = unsafe { state() };
    let device = state
        .devices
        .get_mut(0)
        .and_then(|slot| slot.as_mut())
        .ok_or(NetError::NoDevice)?;
    Ok(f(device))
}

/// The link state, if a NIC exists.
pub fn link_info() -> Option<LinkInfo> {
    // SAFETY: read-only access from kernel code with interrupts masked.
    unsafe {
        let state = state();
        state.devices[0].as_ref().map(|device| device.link_info())
    }
}

/// Resolve a DNS name to one IPv4 address.
pub fn resolve_dns(name: &[u8]) -> Result<Ipv4Addr, NetError> {
    // The outer `?` is the "is there a NIC" error; the inner result is the
    // driver's own answer.
    let result = with_net(|device| device.resolve_dns(name))?;
    result
}

/// Send one UDP datagram.
pub fn send_udp(destination: Ipv4Addr, port: u16, payload: &[u8]) -> Result<(), NetError> {
    let result = with_net(|device| device.send_udp(destination, port, payload))?;
    result
}

/// Receive one UDP datagram whose source port is `port`.
pub fn receive_udp(port: u16, out: &mut [u8]) -> Result<usize, NetError> {
    let result = with_net(|device| device.receive_udp(port, out))?;
    result
}

/// Stop every NIC and give its frames and rings back.
///
/// Nothing in the boot path calls this — the machine is about to be turned off —
/// but the suites do, to prove that a driver shut down cleanly leaves the frame
/// allocator exactly as it found it.
pub fn shutdown_all() {
    // SAFETY: as `device_count`. Each device is taken out of the table so a
    // second call cannot shut down a released device.
    let state = unsafe { state() };
    for slot in state.devices.iter_mut() {
        if let Some(mut device) = slot.take() {
            device.shutdown();
        }
    }
    state.len = 0;
}
