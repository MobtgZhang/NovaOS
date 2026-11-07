//! The M7 suites: VirtIO net presence, link state, DNS reachability, TCP,
//! ICMP, DHCP, the socket API, and the rejection of malformed network input.
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
//! emulated resolver. The TCP suite uses QEMU's `guestfwd` endpoint at
//! `10.0.2.100:80`, which gives a deterministic TCP peer. This proves the
//! handshake, stream bytes, and close path without asserting that an arbitrary
//! external host is reachable.
//!
//! # What is deliberately *not* claimed
//!
//! IPv6, TLS, connection multiplexing, or unbounded stream payloads are not
//! implemented. The service's inline payload is bounded by the IPC ABI, and
//! larger buffers must use shared memory objects. The SMP foundation is
//! implemented, but service and driver state remain bootstrap-owned: the APs run
//! their own kernel-only idle threads rather than concurrent network handlers.
//! Those limits are documented rather than hidden behind a passing suite.
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
//! - `net_tcp_handshake`: a TCP socket must connect to the forwarded server,
//!   carry stream bytes, and close cleanly.
//! - `net_icmp_echo`: an ICMP echo must reach the gateway and return the same
//!   identifier, sequence, and payload.
//! - `net_dhcp_lease`: DHCP must return a QEMU user-network address and an ACK.
//! - `net_socket_api`: a UDP socket must send and receive, sockets must be owned
//!   by the task that opened them, and malformed socket requests must be refused.

use nova_abi::error::ErrorCode;
use nova_protocols::ProtocolVersion;
use nova_protocols::net::{
    NET_DHCP_REPLY_SIZE, NET_DHCP_REQUEST_SIZE, NET_ICMP_REPLY_SIZE, NET_ICMP_REQUEST_SIZE,
    NET_LINK_REPLY_SIZE, NET_LINK_REQUEST_SIZE, NET_MAX_DATAGRAM, NET_MAX_ICMP_PAYLOAD,
    NET_MAX_NAME, NET_MAX_SOCKET_PAYLOAD, NET_MAX_SOCKET_RECV, NET_OP_DHCP, NET_OP_ICMP_ECHO,
    NET_OP_LINK_QUERY, NET_OP_RESOLVE, NET_OP_SEND, NET_OP_SOCKET_CLOSE, NET_OP_SOCKET_CONNECT,
    NET_OP_SOCKET_OPEN, NET_OP_SOCKET_RECV, NET_OP_SOCKET_SEND, NET_RESOLVE_REQUEST_SIZE,
    NET_SEND_REQUEST_SIZE, NET_SOCKET_CLOSE_REPLY_SIZE, NET_SOCKET_CLOSE_REQUEST_SIZE,
    NET_SOCKET_CONNECT_REPLY_SIZE, NET_SOCKET_CONNECT_REQUEST_SIZE, NET_SOCKET_OPEN_REPLY_SIZE,
    NET_SOCKET_OPEN_REQUEST_SIZE, NET_SOCKET_RECV_REPLY_SIZE, NET_SOCKET_RECV_REQUEST_SIZE,
    NET_SOCKET_SEND_REPLY_SIZE, NET_SOCKET_SEND_REQUEST_SIZE, NET_SOCKET_TCP, NET_SOCKET_UDP,
    NET_VERSION,
};

use super::harness::TestSuite;
use crate::drivers::net as driver;
use crate::drivers::pci;
use crate::drivers::virtio::net::{DNS_SERVER, LINK_ADDRESS, LINK_GATEWAY};
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
    TestSuite {
        name: "net_tcp_handshake",
        cases: &[
            (
                "a_tcp_socket_reaches_the_forwarded_server",
                a_tcp_socket_reaches_the_forwarded_server,
            ),
            (
                "a_tcp_socket_carries_a_request",
                a_tcp_socket_carries_a_request,
            ),
            ("a_tcp_socket_closes_cleanly", a_tcp_socket_closes_cleanly),
        ],
    },
    TestSuite {
        name: "net_icmp_echo",
        cases: &[
            (
                "an_icmp_echo_reaches_the_gateway",
                an_icmp_echo_reaches_the_gateway,
            ),
            (
                "an_icmp_echo_reports_the_service_version",
                an_icmp_echo_reports_the_service_version,
            ),
        ],
    },
    TestSuite {
        name: "net_dhcp_lease",
        cases: &[
            ("dhcp_returns_a_guest_address", dhcp_returns_a_guest_address),
            (
                "dhcp_returns_an_acknowledgement",
                dhcp_returns_an_acknowledgement,
            ),
        ],
    },
    TestSuite {
        name: "net_socket_api",
        cases: &[
            (
                "a_udp_socket_send_and_recv_round_trip",
                a_udp_socket_send_and_recv_round_trip,
            ),
            (
                "a_socket_is_not_usable_by_another_task",
                a_socket_is_not_usable_by_another_task,
            ),
            (
                "malformed_socket_requests_are_refused",
                malformed_socket_requests_are_refused,
            ),
        ],
    },
];

/// The kernel's own service state, reset for this case.
fn fresh_state() -> crate::sync::SpinLockGuard<'static, crate::services::KernelState> {
    // SAFETY: suites run one at a time in ring 0 with interrupts masked.
    let state = crate::services::test_state();
    let mut state = state;
    state.reset();
    state
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

/// Build a fixed socket-open request.
fn socket_open_request(
    version: ProtocolVersion,
    flags: u32,
    kind: u32,
) -> [u8; NET_SOCKET_OPEN_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_SOCKET_OPEN_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..12].copy_from_slice(&kind.to_le_bytes());
    bytes
}

/// Build a fixed socket-connect request.
fn socket_connect_request(
    version: ProtocolVersion,
    flags: u32,
    socket: u16,
    port: u16,
    destination: [u8; 4],
) -> [u8; NET_SOCKET_CONNECT_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_SOCKET_CONNECT_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&socket.to_le_bytes());
    bytes[10..12].copy_from_slice(&port.to_le_bytes());
    bytes[12..16].copy_from_slice(&destination);
    bytes
}

/// Build a variable-length socket-send request.
fn socket_send_request(
    version: ProtocolVersion,
    flags: u32,
    socket: u16,
    data: &[u8],
) -> [u8; NET_SOCKET_SEND_REQUEST_SIZE + NET_MAX_SOCKET_PAYLOAD] {
    let mut bytes = [0u8; NET_SOCKET_SEND_REQUEST_SIZE + NET_MAX_SOCKET_PAYLOAD];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&socket.to_le_bytes());
    bytes[10..12].copy_from_slice(&(data.len() as u16).to_le_bytes());
    bytes[NET_SOCKET_SEND_REQUEST_SIZE..NET_SOCKET_SEND_REQUEST_SIZE + data.len()]
        .copy_from_slice(data);
    bytes
}

/// Build a fixed socket-receive request.
fn socket_recv_request(
    version: ProtocolVersion,
    flags: u32,
    socket: u16,
    max_len: u16,
) -> [u8; NET_SOCKET_RECV_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_SOCKET_RECV_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&socket.to_le_bytes());
    bytes[10..12].copy_from_slice(&max_len.to_le_bytes());
    bytes
}

/// Build a fixed socket-close request.
fn socket_close_request(
    version: ProtocolVersion,
    flags: u32,
    socket: u16,
) -> [u8; NET_SOCKET_CLOSE_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_SOCKET_CLOSE_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..10].copy_from_slice(&socket.to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    bytes
}

/// Build a variable-length ICMP echo request.
fn icmp_request(
    version: ProtocolVersion,
    flags: u32,
    destination: [u8; 4],
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> [u8; NET_ICMP_REQUEST_SIZE + NET_MAX_ICMP_PAYLOAD] {
    let mut bytes = [0u8; NET_ICMP_REQUEST_SIZE + NET_MAX_ICMP_PAYLOAD];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..12].copy_from_slice(&destination);
    bytes[12..14].copy_from_slice(&identifier.to_le_bytes());
    bytes[14..16].copy_from_slice(&sequence.to_le_bytes());
    bytes[16..18].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    bytes[18..20].copy_from_slice(&0u16.to_le_bytes());
    bytes[NET_ICMP_REQUEST_SIZE..NET_ICMP_REQUEST_SIZE + payload.len()].copy_from_slice(payload);
    bytes
}

/// Build a fixed DHCP request.
fn dhcp_request(version: ProtocolVersion, flags: u32) -> [u8; NET_DHCP_REQUEST_SIZE] {
    let mut bytes = [0u8; NET_DHCP_REQUEST_SIZE];
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes
}

/// The socket identifier a successful socket-open reply carries.
fn reply_socket(reply: &[u8]) -> Option<u16> {
    let bytes = reply.get(8..10)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// The local port a successful socket-open reply carries.
fn reply_local_port(reply: &[u8]) -> Option<u16> {
    let bytes = reply.get(10..12)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = link_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];

    let length = ask(&mut state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = link_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];

    let length = ask(&mut state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the link reply was not the protocol's fixed size");
    }
    if reply_version(&reply) != Some(NET_VERSION) {
        return Err("the link reply carried the wrong version");
    }
    Ok(())
}

fn an_unknown_version_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = link_request(ProtocolVersion::new(2, 0), 0);
    let mut reply = [0u8; 64];

    let length = ask(&mut state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Unsupported.code()) {
        return Err("an unknown version was not refused as unsupported");
    }
    Ok(())
}

fn nonzero_flags_are_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = link_request(NET_VERSION, 1);
    let mut reply = [0u8; 64];

    let length = ask(&mut state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;

    let name = b"example.com";
    let request = resolve_request(NET_VERSION, 0, name);
    let mut reply = [0u8; 64];

    let length = ask(
        &mut state,
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;

    let name = b"example.com";
    let request = resolve_request(NET_VERSION, 0, name);
    let mut reply = [0u8; 64];

    let length = ask(
        &mut state,
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = [0u8; NET_LINK_REQUEST_SIZE - 1];
    let mut reply = [0u8; 64];

    let length = ask(&mut state, slot, NET_OP_LINK_QUERY, &request, &mut reply)?;
    if length != NET_LINK_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(ErrorCode::Invalid.code()) {
        return Err("a short link request was not refused as invalid");
    }
    Ok(())
}

fn a_resolve_request_with_zero_name_is_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = resolve_request(NET_VERSION, 0, b"");
    let mut reply = [0u8; 64];

    let length = ask(
        &mut state,
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
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;

    let mut request = [0u8; NET_SEND_REQUEST_SIZE];
    request[0..2].copy_from_slice(&NET_VERSION.major.to_le_bytes());
    request[2..4].copy_from_slice(&NET_VERSION.minor.to_le_bytes());
    request[4..8].copy_from_slice(&0u32.to_le_bytes());
    request[8..12].copy_from_slice(&LINK_GATEWAY.octets());
    request[12..14].copy_from_slice(&53u16.to_le_bytes());
    request[14..16].copy_from_slice(&(NET_MAX_DATAGRAM as u16 + 1).to_le_bytes());

    let mut reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_SEND, &request, &mut reply)?;
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

// ---------------------------------------------------------------------------
// net_tcp_handshake
// ---------------------------------------------------------------------------

/// The deterministic TCP peer QEMU forwards through `guestfwd`.
const TCP_TEST_SERVER: [u8; 4] = [10, 0, 2, 100];
const TCP_TEST_PORT: u16 = 80;

/// Open one TCP socket and return `(socket, local_port)`.
///
/// `reserved_slots` UDP sockets are opened first so the TCP socket lands in a
/// different service slot and therefore gets a different local port. Each TCP
/// case then uses a fresh 4-tuple instead of reusing a port that QEMU's
/// user-network stack may still hold from the previous case.
fn open_tcp_socket(
    state: &mut KernelState,
    slot: usize,
    reserved_slots: usize,
) -> Result<(u16, u16), &'static str> {
    let mut reply = [0u8; 64];
    for _ in 0..reserved_slots {
        let dummy = socket_open_request(NET_VERSION, 0, NET_SOCKET_UDP);
        let length = ask(&mut *state, slot, NET_OP_SOCKET_OPEN, &dummy, &mut reply)?;
        if length != NET_SOCKET_OPEN_REPLY_SIZE || reply_status(&reply) != Some(0) {
            return Err("a reserved socket did not open");
        }
    }

    let request = socket_open_request(NET_VERSION, 0, NET_SOCKET_TCP);
    let mut reply = [0u8; 64];
    let length = ask(&mut *state, slot, NET_OP_SOCKET_OPEN, &request, &mut reply)?;
    if length != NET_SOCKET_OPEN_REPLY_SIZE {
        return Err("the socket-open reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the TCP socket did not open");
    }
    let socket = reply_socket(&reply).ok_or("the socket-open reply had no identifier")?;
    let port = reply_local_port(&reply).ok_or("the socket-open reply had no local port")?;
    Ok((socket, port))
}

/// Open and connect one TCP socket to the forwarded server.
fn connect_forwarded_tcp(
    state: &mut KernelState,
    slot: usize,
    reserved_slots: usize,
) -> Result<(u16, u16), &'static str> {
    let (socket, port) = open_tcp_socket(state, slot, reserved_slots)?;
    let request = socket_connect_request(NET_VERSION, 0, socket, TCP_TEST_PORT, TCP_TEST_SERVER);
    let mut reply = [0u8; 64];
    let length = ask(state, slot, NET_OP_SOCKET_CONNECT, &request, &mut reply)?;
    if length != NET_SOCKET_CONNECT_REPLY_SIZE {
        return Err("the socket-connect reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the TCP handshake did not reach the forwarded server");
    }
    if reply_socket(&reply) != Some(socket) {
        return Err("the socket-connect reply named a different socket");
    }
    Ok((socket, port))
}

/// Close one socket and verify the fixed close reply.
fn close_socket(state: &mut KernelState, slot: usize, socket: u16) -> Result<(), &'static str> {
    let close = socket_close_request(NET_VERSION, 0, socket);
    let mut reply = [0u8; 64];
    let length = ask(state, slot, NET_OP_SOCKET_CLOSE, &close, &mut reply)?;
    if length != NET_SOCKET_CLOSE_REPLY_SIZE {
        return Err("the socket-close reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the TCP socket did not close");
    }
    Ok(())
}

fn a_tcp_socket_reaches_the_forwarded_server() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let (socket, _port) = connect_forwarded_tcp(&mut state, slot, 0)?;
    close_socket(&mut state, slot, socket)?;
    Ok(())
}

fn a_tcp_socket_carries_a_request() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let (socket, _port) = connect_forwarded_tcp(&mut state, slot, 1)?;

    let payload = b"nova-tcp";
    let send = socket_send_request(NET_VERSION, 0, socket, payload);
    let mut send_reply = [0u8; 64];
    let length = ask(
        &mut state,
        slot,
        NET_OP_SOCKET_SEND,
        &send[..NET_SOCKET_SEND_REQUEST_SIZE + payload.len()],
        &mut send_reply,
    )?;
    if length != NET_SOCKET_SEND_REPLY_SIZE {
        return Err("the socket-send reply was not the protocol's fixed size");
    }
    if reply_status(&send_reply) != Some(0) {
        return Err("the TCP request was not sent");
    }
    let sent = send_reply
        .get(8..10)
        .map(|b| u16::from_le_bytes([b[0], b[1]]));
    if sent != Some(payload.len() as u16) {
        return Err("the socket-send reply did not report the payload length");
    }

    // The forwarded `/bin/cat` echoes the stream bytes back to the socket.
    let recv = socket_recv_request(NET_VERSION, 0, socket, NET_MAX_SOCKET_RECV as u16);
    let mut recv_reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_SOCKET_RECV, &recv, &mut recv_reply)?;
    if length < NET_SOCKET_RECV_REPLY_SIZE {
        return Err("the socket-receive reply was shorter than its header");
    }
    if reply_status(&recv_reply) != Some(0) {
        return Err("the TCP socket did not receive the echoed request");
    }
    let payload_length = recv_reply
        .get(8..10)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    if payload_length != payload.len() as u16 {
        return Err("the echoed payload had the wrong length");
    }
    let echoed = &recv_reply
        [NET_SOCKET_RECV_REPLY_SIZE..NET_SOCKET_RECV_REPLY_SIZE + payload_length as usize];
    if echoed != payload {
        return Err("the echoed payload was not the request bytes");
    }

    // Release the forwarded connection for the next case.
    close_socket(&mut state, slot, socket)?;
    Ok(())
}

fn a_tcp_socket_closes_cleanly() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let (socket, _port) = connect_forwarded_tcp(&mut state, slot, 2)?;

    let close = socket_close_request(NET_VERSION, 0, socket);
    let mut close_reply = [0u8; 64];
    let length = ask(
        &mut state,
        slot,
        NET_OP_SOCKET_CLOSE,
        &close,
        &mut close_reply,
    )?;
    if length != NET_SOCKET_CLOSE_REPLY_SIZE {
        return Err("the socket-close reply was not the protocol's fixed size");
    }
    if reply_status(&close_reply) != Some(0) {
        return Err("the TCP socket did not close");
    }
    if reply_socket(&close_reply) != Some(socket) {
        return Err("the socket-close reply named a different socket");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_icmp_echo
// ---------------------------------------------------------------------------

fn an_icmp_echo_reaches_the_gateway() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let payload = b"nova";
    let request = icmp_request(NET_VERSION, 0, LINK_GATEWAY.octets(), 7, 3, payload);

    let mut reply = [0u8; 64];
    let length = ask(
        &mut state,
        slot,
        NET_OP_ICMP_ECHO,
        &request[..NET_ICMP_REQUEST_SIZE + payload.len()],
        &mut reply,
    )?;
    if length < NET_ICMP_REPLY_SIZE {
        return Err("the ICMP reply was shorter than its header");
    }
    if reply_status(&reply) != Some(0) {
        return Err("the ICMP echo did not reach the gateway");
    }
    if reply.get(8..10) != Some(&7u16.to_le_bytes()) {
        return Err("the ICMP reply carried the wrong identifier");
    }
    if reply.get(10..12) != Some(&3u16.to_le_bytes()) {
        return Err("the ICMP reply carried the wrong sequence");
    }
    let reply_length = reply.get(12..14).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if reply_length != Some(payload.len() as u16) {
        return Err("the ICMP reply carried the wrong payload length");
    }
    if reply[NET_ICMP_REPLY_SIZE..NET_ICMP_REPLY_SIZE + payload.len()] != *payload {
        return Err("the ICMP reply payload was not the echo payload");
    }
    Ok(())
}

fn an_icmp_echo_reports_the_service_version() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let payload = b"nova";
    let request = icmp_request(NET_VERSION, 0, LINK_GATEWAY.octets(), 7, 3, payload);

    let mut reply = [0u8; 64];
    let length = ask(
        &mut state,
        slot,
        NET_OP_ICMP_ECHO,
        &request[..NET_ICMP_REQUEST_SIZE + payload.len()],
        &mut reply,
    )?;
    if length < NET_ICMP_REPLY_SIZE {
        return Err("the ICMP reply was shorter than its header");
    }
    if reply_version(&reply) != Some(NET_VERSION) {
        return Err("the ICMP reply carried the wrong version");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_dhcp_lease
// ---------------------------------------------------------------------------

fn dhcp_returns_a_guest_address() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = dhcp_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_DHCP, &request, &mut reply)?;

    if length != NET_DHCP_REPLY_SIZE {
        return Err("the DHCP reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("DHCP did not return a lease");
    }
    let address = [reply[8], reply[9], reply[10], reply[11]];
    if address[0] != 10 || address[1] != 0 || address[2] != 2 {
        return Err("DHCP returned an address outside the QEMU user network");
    }
    Ok(())
}

fn dhcp_returns_an_acknowledgement() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let request = dhcp_request(NET_VERSION, 0);
    let mut reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_DHCP, &request, &mut reply)?;

    if length != NET_DHCP_REPLY_SIZE {
        return Err("the DHCP reply was not the protocol's fixed size");
    }
    if reply_status(&reply) != Some(0) {
        return Err("DHCP did not return a lease");
    }
    let message_type = reply.get(16..18).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if message_type != Some(5) {
        return Err("DHCP did not return an ACK");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// net_socket_api
// ---------------------------------------------------------------------------

fn a_udp_socket_send_and_recv_round_trip() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;

    let open = socket_open_request(NET_VERSION, 0, NET_SOCKET_UDP);
    let mut reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_SOCKET_OPEN, &open, &mut reply)?;
    if length != NET_SOCKET_OPEN_REPLY_SIZE || reply_status(&reply) != Some(0) {
        return Err("the UDP socket did not open");
    }
    let socket = reply_socket(&reply).ok_or("the socket-open reply had no identifier")?;

    let connect = socket_connect_request(NET_VERSION, 0, socket, 53, DNS_SERVER.octets());
    let length = ask(
        &mut state,
        slot,
        NET_OP_SOCKET_CONNECT,
        &connect,
        &mut reply,
    )?;
    if length != NET_SOCKET_CONNECT_REPLY_SIZE || reply_status(&reply) != Some(0) {
        return Err("the UDP socket did not connect to the DNS server");
    }

    // `localhost` answers with a short A record (a fixed 127.0.0.1 in the
    // QEMU user network), so the reply fits the bounded socket budget. A
    // longer public name like `example.com` can carry additional records that
    // exceed `NET_MAX_SOCKET_RECV`.
    let mut query = [0u8; 512];
    let query_length = nova_net::build_dns_query(b"localhost", 0x1234, &mut query)
        .ok_or("the DNS query did not fit")?;
    let send = socket_send_request(NET_VERSION, 0, socket, &query[..query_length]);
    let length = ask(
        &mut state,
        slot,
        NET_OP_SOCKET_SEND,
        &send[..NET_SOCKET_SEND_REQUEST_SIZE + query_length],
        &mut reply,
    )?;
    if length != NET_SOCKET_SEND_REPLY_SIZE || reply_status(&reply) != Some(0) {
        return Err("the UDP socket did not send the DNS query");
    }

    let recv = socket_recv_request(NET_VERSION, 0, socket, NET_MAX_SOCKET_RECV as u16);
    let mut recv_reply = [0u8; 64];
    let length = ask(&mut state, slot, NET_OP_SOCKET_RECV, &recv, &mut recv_reply)?;
    if length < NET_SOCKET_RECV_REPLY_SIZE {
        return Err("the socket-receive reply was shorter than its header");
    }
    if reply_status(&recv_reply) != Some(0) {
        return Err("the UDP socket did not receive a reply");
    }
    let payload_length = recv_reply
        .get(8..10)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    if payload_length == 0 {
        return Err("the UDP socket received an empty reply");
    }
    let payload = &recv_reply
        [NET_SOCKET_RECV_REPLY_SIZE..NET_SOCKET_RECV_REPLY_SIZE + payload_length as usize];
    let address =
        nova_net::parse_dns_a_record(payload).ok_or("the socket reply was not a DNS A record")?;
    if address.as_u32() == 0 {
        return Err("the socket reply carried a zero address");
    }
    Ok(())
}

fn a_socket_is_not_usable_by_another_task() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let first = task_slot(&mut state)?;
    let second = task_slot(&mut state)?;

    let open = socket_open_request(NET_VERSION, 0, NET_SOCKET_TCP);
    let mut reply = [0u8; 64];
    let length = ask(&mut state, first, NET_OP_SOCKET_OPEN, &open, &mut reply)?;
    if length != NET_SOCKET_OPEN_REPLY_SIZE || reply_status(&reply) != Some(0) {
        return Err("the first task could not open a socket");
    }
    let socket = reply_socket(&reply).ok_or("the socket-open reply had no identifier")?;

    let connect = socket_connect_request(NET_VERSION, 0, socket, 80, LINK_GATEWAY.octets());
    let mut foreign_reply = [0u8; 64];
    let length = ask(
        &mut state,
        second,
        NET_OP_SOCKET_CONNECT,
        &connect,
        &mut foreign_reply,
    )?;
    if length != NET_SOCKET_CONNECT_REPLY_SIZE {
        return Err("the refusal reply was not the protocol's fixed size");
    }
    if reply_status(&foreign_reply) != Some(ErrorCode::BadHandle.code()) {
        return Err("another task was allowed to use the socket");
    }
    Ok(())
}

fn malformed_socket_requests_are_refused() -> Result<(), &'static str> {
    let mut state = fresh_state();
    let mut state = &mut *state;
    let slot = task_slot(&mut state)?;
    let mut reply = [0u8; 64];

    // Unknown protocol version.
    let open = socket_open_request(ProtocolVersion::new(2, 0), 0, NET_SOCKET_TCP);
    let length = ask(&mut state, slot, NET_OP_SOCKET_OPEN, &open, &mut reply)?;
    if length != NET_SOCKET_OPEN_REPLY_SIZE
        || reply_status(&reply) != Some(ErrorCode::Unsupported.code())
    {
        return Err("an unknown socket version was not refused");
    }

    // Socket kind the service does not implement.
    let open = socket_open_request(NET_VERSION, 0, 3);
    let length = ask(&mut state, slot, NET_OP_SOCKET_OPEN, &open, &mut reply)?;
    if length != NET_SOCKET_OPEN_REPLY_SIZE
        || reply_status(&reply) != Some(ErrorCode::Unsupported.code())
    {
        return Err("an unsupported socket kind was not refused");
    }

    // Zero socket identifier.
    let connect = socket_connect_request(NET_VERSION, 0, 0, 80, LINK_GATEWAY.octets());
    let length = ask(
        &mut state,
        slot,
        NET_OP_SOCKET_CONNECT,
        &connect,
        &mut reply,
    )?;
    if length != NET_SOCKET_CONNECT_REPLY_SIZE
        || reply_status(&reply) != Some(ErrorCode::Invalid.code())
    {
        return Err("a zero socket identifier was not refused");
    }

    // Receive limit above the protocol's budget.
    let recv = socket_recv_request(NET_VERSION, 0, 1, NET_MAX_SOCKET_RECV as u16 + 1);
    let length = ask(&mut state, slot, NET_OP_SOCKET_RECV, &recv, &mut reply)?;
    if length != NET_SOCKET_RECV_REPLY_SIZE
        || reply_status(&reply) != Some(ErrorCode::Invalid.code())
    {
        return Err("an oversized receive limit was not refused");
    }

    // Send length above the protocol's budget, with no payload bytes.
    let mut send = [0u8; NET_SOCKET_SEND_REQUEST_SIZE];
    send[0..2].copy_from_slice(&NET_VERSION.major.to_le_bytes());
    send[2..4].copy_from_slice(&NET_VERSION.minor.to_le_bytes());
    send[8..10].copy_from_slice(&1u16.to_le_bytes());
    send[10..12].copy_from_slice(&(NET_MAX_SOCKET_PAYLOAD as u16 + 1).to_le_bytes());
    let length = ask(&mut state, slot, NET_OP_SOCKET_SEND, &send, &mut reply)?;
    if length != NET_SOCKET_SEND_REPLY_SIZE
        || reply_status(&reply) != Some(ErrorCode::Invalid.code())
    {
        return Err("an oversized socket send was not refused");
    }

    Ok(())
}
