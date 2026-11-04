//! The M7 suites: VirtIO net presence, link state, DNS reachability, and the
//! rejection of malformed network-protocol input.
//!
//! # What these cases prove
//!
//! The driver is live at boot: `drivers::init` takes the NIC over before the
//! harness runs. These cases therefore ask the *boot-time* device, not a fresh
//! one, and drive the service through `service_request` — the same kernel-side
//! path the syscall entry uses.
//!
//! The network itself is QEMU's `user` backend, whose guest/gateway/DNS
//! addresses are fixed at `10.0.2.15`, `10.0.2.2`, and `10.0.2.3`. The link
//! suite checks those facts; the DNS suite checks that a real name reaches the
//! emulated resolver.
//!
//! # What is deliberately *not* claimed
//!
//! This is not a TCP stack, DHCP, IPv6, or a general socket API. The report's
//! M7 wording is "basic network ... phased implementation"; the implemented
//! phase is ARP + IPv4 + UDP + DNS over VirtIO net. Full SMP is likewise not
//! implemented: the kernel is still single-CPU with interrupts masked, and the
//! QEMU command still boots one CPU. Those limits are documented rather than
//! hidden behind a passing suite.
//!
//! # Named failures
//!
//! - `virtio_net_device`: PCI must show a VirtIO network function, exactly one
//!   NIC must be live, and its MAC must not be the all-zero placeholder.
//! - `net_link_query`: the service must report the QEMU user-network address and
//!   gateway, and must refuse an unknown version or nonzero flags.
//! - `net_dns_resolves_example_com`: a DNS query must reach the emulated
//!   resolver and return a non-zero A record.
//! - `net_malformed_input_denied`: malformed request lengths, unknown versions,
//!   nonzero flags, oversized datagrams, and a corrupted IPv4 frame must all be
//!   refused before they are interpreted.

use nova_abi::error::ErrorCode;
use nova_protocols::ProtocolVersion;
use nova_protocols::net::{
    NET_LINK_REPLY_SIZE, NET_LINK_REQUEST_SIZE, NET_MAX_DATAGRAM, NET_MAX_NAME, NET_OP_LINK_QUERY,
    NET_OP_RESOLVE, NET_OP_SEND, NET_RESOLVE_REQUEST_SIZE, NET_SEND_REQUEST_SIZE, NET_VERSION,
};

use super::harness::TestSuite;
use crate::drivers::net as driver;
use crate::drivers::pci;
use crate::drivers::virtio::net::{LINK_ADDRESS, LINK_GATEWAY};
use crate::services::dispatch::{SyscallContext, service_request};
use crate::services::{KernelState, SERVICE_NET};

/// The M7 suites, in registration order.
pub static M7_SUITES: &[TestSuite] = &[
    TestSuite {
        name: "virtio_net_device",
        cases: &[
            (
                "pci_enumeration_finds_a_virtio_net_function",
                pci_enumeration_finds_a_virtio_net_function,
            ),
            ("exactly_one_nic_is_live", exactly_one_nic_is_live),
            ("the_nic_reports_a_mac", the_nic_reports_a_mac),
            (
                "the_nic_reports_the_qemu_user_address",
                the_nic_reports_the_qemu_user_address,
            ),
        ],
    },
    TestSuite {
        name: "net_link_query",
        cases: &[
            (
                "the_link_query_reports_address_and_gateway",
                the_link_query_reports_address_and_gateway,
            ),
            (
                "the_link_query_reports_the_service_version",
                the_link_query_reports_the_service_version,
            ),
            (
                "an_unknown_version_is_refused",
                an_unknown_version_is_refused,
            ),
            ("nonzero_flags_are_refused", nonzero_flags_are_refused),
        ],
    },
    TestSuite {
        name: "net_dns_resolves_example_com",
        cases: &[
            (
                "a_dns_query_returns_a_nonzero_a_record",
                a_dns_query_returns_a_nonzero_a_record,
            ),
            (
                "the_dns_reply_carries_the_service_version",
                the_dns_reply_carries_the_service_version,
            ),
        ],
    },
    TestSuite {
        name: "net_malformed_input_denied",
        cases: &[
            (
                "a_short_link_request_is_refused",
                a_short_link_request_is_refused,
            ),
            (
                "a_resolve_request_with_zero_name_is_refused",
                a_resolve_request_with_zero_name_is_refused,
            ),
            (
                "an_oversized_datagram_length_is_refused",
                an_oversized_datagram_length_is_refused,
            ),
            (
                "a_corrupted_ipv4_checksum_is_not_accepted",
                a_corrupted_ipv4_checksum_is_not_accepted,
            ),
        ],
    },
];

/// The kernel's own service state, reset for this case.
fn fresh_state() -> &'static mut KernelState {
    // SAFETY: suites run one at a time in ring 0 with interrupts masked.
    unsafe {
        let state = crate::services::test_state();
        state.reset();
        state
    }
}

/// Create one task in the fresh state and return its slot.
fn task_slot(state: &mut KernelState) -> Result<usize, &'static str> {
    let id = state.create_task().ok_or("the task pool was empty")?;
    state.slot_of_task(id).ok_or("the new task has no slot")
}

/// Ask the network service one request and return the reply length.
fn ask(
    state: &mut KernelState,
    slot: usize,
    opcode: u64,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, &'static str> {
    let context = SyscallContext::new(slot, 0);
    match service_request(state, &context, SERVICE_NET, opcode, request, reply) {
        Ok(length) => Ok(length),
        Err(error) => Err(error.name()),
    }
}

/// A status word from a reply, at offset 4.
fn reply_status(reply: &[u8]) -> Option<i32> {
    let bytes = reply.get(4..8)?;
    Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// A protocol version from a reply's first four bytes.
fn reply_version(reply: &[u8]) -> Option<ProtocolVersion> {
    let major = reply.get(0..2)?;
    let minor = reply.get(2..4)?;
    Some(ProtocolVersion::new(
        u16::from_le_bytes([major[0], major[1]]),
        u16::from_le_bytes([minor[0], minor[1]]),
    ))
}

/// Build a fixed link request.
fn link_request(version: ProtocolVersion, flags: u32) -> [u8; NET_LINK_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_LINK_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes
}

/// Build a resolve request with a name.
fn resolve_request(
    version: ProtocolVersion,
    flags: u32,
    name: &[u8],
) -> [u8; NET_RESOLVE_REQUEST_SIZE + NET_MAX_NAME] {
    let mut bytes = [0u8; NET_RESOLVE_REQUEST_SIZE + NET_MAX_NAME];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&(name.len() as u16).to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    bytes[NET_RESOLVE_REQUEST_SIZE..NET_RESOLVE_REQUEST_SIZE + name.len()].copy_from_slice(name);
    bytes
}

// ---------------------------------------------------------------------------
// virtio_net_device
// ---------------------------------------------------------------------------

fn pci_enumeration_finds_a_virtio_net_function() -> Result<(), &'static str> {
    let devices = pci::enumerate();
    if !devices.iter().any(|device| device.is_virtio_net()) {
        return Err("PCI enumeration found no VirtIO network function");
    }
    Ok(())
}

fn exactly_one_nic_is_live() -> Result<(), &'static str> {
    if driver::device_count() != 1 {
        return Err("the kernel did not start exactly one NIC");
    }
    Ok(())
}

fn the_nic_reports_a_mac() -> Result<(), &'static str> {
    let info = driver::link_info().ok_or("the NIC has no link state")?;
    if info.mac.octets() == [0u8; 6] {
        return Err("the NIC reported an all-zero MAC");
    }
    Ok(())
}

fn the_nic_reports_the_qemu_user_address() -> Result<(), &'static str> {
    let info = driver::link_info().ok_or("the NIC has no link state")?;
    if info.address != LINK_ADDRESS {
        return Err("the NIC did not report the QEMU guest address");
    }
    if info.gateway != LINK_GATEWAY {
        return Err("the NIC did not report the QEMU gateway");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_link_query
// ---------------------------------------------------------------------------

fn the_link_query_reports_address_and_gateway() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = link_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];

    let length = ask(state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the link reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the link query returned a nonzero status");
    }

    let address = &reply[8..12];
    if address != LINK_ADDRESS.octets() {
        return Err("the reply address was not the QEMU guest address");
    }
    let gateway = &reply[12..16];
    if gateway != LINK_GATEWAY.octets() {
        return Err("the reply gateway was not the QEMU gateway");
    }
    Ok(())
}

fn the_link_query_reports_the_service_version() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = link_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];

    let length = ask(state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the link reply was not the protocol's fixed size");
    }
    if reply_version(&reply) != Some(NET_VERSION) {
        return Err("the link reply carried the wrong version");
    }
    Ok(())
}

fn an_unknown_version_is_refused() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = link_request(ProtocolVersion::new(2, 0), 0);
    let mut reply = [0u8; 64];

    let length = ask(state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Unsupported.code()) {
        return Err("an unknown version was not refused as unsupported");
    }
    Ok(())
}

fn nonzero_flags_are_refused() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = link_request(NET_VERSION, 1);
    let mut reply = [0u8; 64];

    let length = ask(state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Invalid.code()) {
        return Err("nonzero flags were not refused as invalid");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_dns_resolves_example_com
// ---------------------------------------------------------------------------

fn a_dns_query_returns_a_nonzero_a_record() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;

    let name = b"example.com";
    let request = resolve_request(NET_VERSION, 0, name);
    let mut reply = [0u8; 64];

    let length = ask(
        state,
        slot,
        NET_OP_RESOLVE,
        &request[..NET_RESOLVE_REQUEST_SIZE + name.len()],
        &mut reply,
    )?;
    if length != nova_protocols::net::NET_RESOLVE_REPLY_SIZE {
        return Err("the DNS reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the DNS query did not resolve");
    }
    let address = &reply[8..12];
    if address == [0, 0, 0, 0] {
        return Err("the DNS reply carried a zero address");
    }
    Ok(())
}

fn the_dns_reply_carries_the_service_version() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;

    let name = b"example.com";
    let request = resolve_request(NET_VERSION, 0, name);
    let mut reply = [0u8; 64];

    let length = ask(
        state,
        slot,
        NET_OP_RESOLVE,
        &request[..NET_RESOLVE_REQUEST_SIZE + name.len()],
        &mut reply,
    )?;
    if length != nova_protocols::net::NET_RESOLVE_REPLY_SIZE {
        return Err("the DNS reply was not the protocol's fixed size");
    }
    if reply_version(&reply) != Some(NET_VERSION) {
        return Err("the DNS reply carried the wrong version");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_malformed_input_denied
// ---------------------------------------------------------------------------

fn a_short_link_request_is_refused() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = [0u8; NET_LINK_REQUEST_SIZE - 1];
    let mut reply = [0u8; 64];

    let length = ask(state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Invalid.code()) {
        return Err("a short link request was not refused as invalid");
    }
    Ok(())
}

fn a_resolve_request_with_zero_name_is_refused() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;
    let request = resolve_request(NET_VERSION, 0, b"");
    let mut reply = [0u8; 64];

    let length = ask(
        state,
        slot,
        NET_OP_RESOLVE,
        &request[..NET_RESOLVE_REQUEST_SIZE],
        &mut reply,
    )?;
    if length != nova_protocols::net::NET_RESOLVE_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Invalid.code()) {
        return Err("a zero-length name was not refused as invalid");
    }
    Ok(())
}

fn an_oversized_datagram_length_is_refused() -> Result<(), &'static str> {
    let state = fresh_state();
    let slot = task_slot(state)?;

    let mut request = [0u8; NET_SEND_REQUEST_SIZE];
    request[0..2].copy_from_slice(&NET_VERSION.major.to_le_bytes());
    request[2..4].copy_from_slice(&NET_VERSION.minor.to_le_bytes());
    request[4..8].copy_from_slice(&0u32.to_le_bytes());
    request[8..12].copy_from_slice(&LINK_GATEWAY.octets());
    request[12..14].copy_from_slice(&53u16.to_le_bytes());
    request[14..16].copy_from_slice(&(NET_MAX_DATAGRAM as u16 + 1).to_le_bytes());

    let mut reply = [0u8; 64];
    let length = ask(state, slot, NET_OP_SEND, &request, &mut reply)?;
    if length != nova_protocols::net::NET_SEND_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Invalid.code()) {
        return Err("an oversized datagram length was not refused as invalid");
    }
    Ok(())
}

fn a_corrupted_ipv4_checksum_is_not_accepted() -> Result<(), &'static str> {
    use nova_net::{build_ipv4, parse_ipv4};

    let source = LINK_ADDRESS;
    let destination = LINK_GATEWAY;
    let mut frame = [0u8; 64];
    let length = build_ipv4(source, destination, 17, &[1, 2, 3], &mut frame)
        .ok_or("the packet did not fit the test buffer")?;

    // Flip one checksum byte. A parser that ignored the checksum would accept
    // this packet, and the network driver would forward corrupted data.
    frame[10] ^= 0xff;
    if parse_ipv4(&frame[..length]).is_some() {
        return Err("a corrupted IPv4 checksum was accepted");
    }
    Ok(())
}
