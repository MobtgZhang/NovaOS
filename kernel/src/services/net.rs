//! The kernel-side network service.
//!
//! # What it serves
//!
//! The service exposes a bounded link query, DNS resolve, UDP send/receive,
//! TCP and UDP sockets, ICMP echo, and DHCP acquisition. Each operation is a
//! fixed `nova-protocols` layout plus at most an inline payload budget, and
//! each calls the NIC driver through `drivers::net` rather than handing a
//! client a device address.
//!
//! # Version first, then fields
//!
//! An unknown version is refused before any field is interpreted. That matters
//! for a protocol whose variable payload is a DNS name, a datagram, or stream
//! bytes: a peer built against a newer version may have moved the header, and
//! interpreting the bytes under the old layout would read data from the wrong
//! offset.
//!
//! # Socket ownership
//!
//! A socket is not a capability. It is a bounded service slot owned by the
//! task that opened it. The service records the caller's task slot and refuses
//! every operation on a socket another task owns, so a socket identifier is not
//! ambient authority. The service's reply carries the socket identifier, but
//! that identifier is only usable by the task that opened it.
//!
//! # Errors are service errors, not device errors
//!
//! The driver's `NetError` is mapped to the ABI's `ErrorCode`. A timeout is
//! `E_TIMEOUT`, a malformed request is `E_INVALID`, a missing NIC or an
//! unreachable destination is `E_UNSUPPORTED`, and a buffer too small for the
//! received data is `E_BUFFER_SMALL`. A socket owned by another task is
//! `E_DENIED`.

use nova_abi::error::ErrorCode;
use nova_ipc::Message;
use nova_net::Ipv4Addr;
use nova_protocols::negotiate;
use nova_protocols::net::{
    NET_DHCP_REPLY_SIZE, NET_DHCP_REQUEST_SIZE, NET_ICMP_REPLY_SIZE, NET_ICMP_REQUEST_SIZE,
    NET_LINK_REPLY_SIZE, NET_LINK_REQUEST_SIZE, NET_MAX_DATAGRAM, NET_MAX_ICMP_PAYLOAD,
    NET_MAX_ICMP_REPLY, NET_MAX_NAME, NET_MAX_RECV, NET_MAX_SOCKET_PAYLOAD,
    NET_MAX_SOCKET_RECV, NET_OP_DHCP, NET_OP_ICMP_ECHO, NET_OP_LINK_QUERY, NET_OP_RECV,
    NET_OP_RESOLVE, NET_OP_SEND, NET_OP_SOCKET_CLOSE, NET_OP_SOCKET_CONNECT,
    NET_OP_SOCKET_OPEN, NET_OP_SOCKET_RECV, NET_OP_SOCKET_SEND, NET_RECV_REPLY_SIZE,
    NET_RECV_REQUEST_SIZE, NET_RESOLVE_REPLY_SIZE, NET_RESOLVE_REQUEST_SIZE,
    NET_SEND_REPLY_SIZE, NET_SEND_REQUEST_SIZE, NET_SOCKET_CLOSE_REPLY_SIZE,
    NET_SOCKET_CLOSE_REQUEST_SIZE, NET_SOCKET_CONNECT_REPLY_SIZE,
    NET_SOCKET_CONNECT_REQUEST_SIZE, NET_SOCKET_OPEN_REPLY_SIZE,
    NET_SOCKET_OPEN_REQUEST_SIZE, NET_SOCKET_RECV_REPLY_SIZE, NET_SOCKET_RECV_REQUEST_SIZE,
    NET_SOCKET_SEND_REPLY_SIZE, NET_SOCKET_SEND_REQUEST_SIZE, NET_SOCKET_TCP, NET_SOCKET_UDP,
    NET_VERSION,
};

use crate::drivers::net as driver;
use crate::drivers::virtio::net::{NetError, TcpConnection};

/// Maximum number of sockets the service tracks.
///
/// A fixed pool because the service has no allocator, and because a socket is
/// kernel state whose lifetime must be bounded. Eight is enough for the
/// desktop's handful of clients and small enough that the pool is a single
/// static array.
pub const MAX_SOCKETS: usize = 8;

/// First local port the service assigns to a socket.
///
/// The DNS and legacy UDP paths use ports below this, so a socket's port cannot
/// collide with them. The offset per slot keeps each socket distinct.
pub const SOCKET_BASE_PORT: u16 = 50000;

/// One bounded socket slot.
#[derive(Clone, Copy, Debug)]
struct SocketSlot {
    /// The task that opened the socket.
    task_slot: usize,
    /// TCP or UDP.
    kind: u32,
    /// The local port assigned to this socket.
    local_port: u16,
    /// The live TCP connection state, when connected.
    tcp: Option<TcpConnection>,
    /// The UDP destination, when connected.
    udp_destination: Option<Ipv4Addr>,
    /// The UDP remote port, when connected.
    udp_remote_port: u16,
}

/// The network service's state.
#[derive(Debug)]
pub struct NetService {
    sockets: [Option<SocketSlot>; MAX_SOCKETS],
}

impl NetService {
    /// A network service with no sockets.
    pub const fn new() -> Self {
        Self {
            sockets: [None; MAX_SOCKETS],
        }
    }

    /// Reset the service.
    ///
    /// The driver owns the device; the service owns the bounded socket slots,
    /// so this releases every slot. It is the bridge between test suites.
    pub fn reset(&mut self) {
        self.sockets = [None; MAX_SOCKETS];
    }

    /// Handle one network-protocol request on behalf of `task_slot`.
    pub fn handle_message(&mut self, request: &Message, task_slot: usize) -> Message {
        match request.opcode() {
            NET_OP_LINK_QUERY => self.handle_link(request.payload()),
            NET_OP_RESOLVE => self.handle_resolve(request.payload()),
            NET_OP_SEND => self.handle_send(request.payload()),
            NET_OP_RECV => self.handle_recv(request.payload()),
            NET_OP_SOCKET_OPEN => self.handle_socket_open(request.payload(), task_slot),
            NET_OP_SOCKET_CONNECT => self.handle_socket_connect(request.payload(), task_slot),
            NET_OP_SOCKET_SEND => self.handle_socket_send(request.payload(), task_slot),
            NET_OP_SOCKET_RECV => self.handle_socket_recv(request.payload(), task_slot),
            NET_OP_SOCKET_CLOSE => self.handle_socket_close(request.payload(), task_slot),
            NET_OP_ICMP_ECHO => self.handle_icmp(request.payload()),
            NET_OP_DHCP => self.handle_dhcp(request.payload()),
            _ => Message::with_payload(0, &[]).unwrap_or_else(|_| Message::new(0)),
        }
    }

    /// The link query.
    fn handle_link(&self, payload: &[u8]) -> Message {
        if payload.len() != NET_LINK_REQUEST_SIZE {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Invalid);
        }

        let Some(info) = driver::link_info() else {
            return reply_error(NET_LINK_REPLY_SIZE, ErrorCode::Unsupported);
        };

        let mut bytes = [0u8; NET_LINK_REPLY_SIZE];
        write_version(&mut bytes, NET_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&info.address.octets());
        bytes[12..16].copy_from_slice(&info.gateway.octets());
        bytes[16..22].copy_from_slice(&info.mac.octets());
        bytes[22..24].copy_from_slice(&(if info.link_up { 1u16 } else { 0u16 }).to_le_bytes());
        Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
    }

    /// The DNS resolve.
    fn handle_resolve(&self, payload: &[u8]) -> Message {
        if payload.len() < NET_RESOLVE_REQUEST_SIZE {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(name_len) = read_u16(payload, 8) else {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(reserved) = read_u16(payload, 10) else {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 || reserved != 0 {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        }

        let name_len = name_len as usize;
        if name_len == 0
            || name_len > NET_MAX_NAME
            || payload.len() != NET_RESOLVE_REQUEST_SIZE + name_len
        {
            return reply_error(NET_RESOLVE_REPLY_SIZE, ErrorCode::Invalid);
        }

        let name = &payload[NET_RESOLVE_REQUEST_SIZE..NET_RESOLVE_REQUEST_SIZE + name_len];
        match driver::resolve_dns(name) {
            Ok(address) => {
                let mut bytes = [0u8; NET_RESOLVE_REPLY_SIZE];
                write_version(&mut bytes, NET_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..12].copy_from_slice(&address.octets());
                bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
                Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
            }
            Err(error) => reply_error(NET_RESOLVE_REPLY_SIZE, error_code(error)),
        }
    }

    /// The legacy UDP send.
    fn handle_send(&self, payload: &[u8]) -> Message {
        if payload.len() < NET_SEND_REQUEST_SIZE {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(destination) = read_bytes(payload, 8, 4) else {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(port) = read_u16(payload, 12) else {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(length) = read_u16(payload, 14) else {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }

        let length = length as usize;
        if length > NET_MAX_DATAGRAM || payload.len() != NET_SEND_REQUEST_SIZE + length {
            return reply_error(NET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }

        let data = &payload[NET_SEND_REQUEST_SIZE..NET_SEND_REQUEST_SIZE + length];
        let destination = ipv4(destination);
        match driver::send_udp(destination, port, data) {
            Ok(()) => {
                let mut bytes = [0u8; NET_SEND_REPLY_SIZE];
                write_version(&mut bytes, NET_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..10].copy_from_slice(&(length as u16).to_le_bytes());
                bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
                Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
            }
            Err(error) => reply_error(NET_SEND_REPLY_SIZE, error_code(error)),
        }
    }

    /// The legacy UDP receive.
    fn handle_recv(&self, payload: &[u8]) -> Message {
        if payload.len() != NET_RECV_REQUEST_SIZE {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(port) = read_u16(payload, 8) else {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(max_len) = read_u16(payload, 10) else {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 || max_len as usize > NET_MAX_RECV {
            return reply_error(NET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        }

        let mut data = [0u8; NET_MAX_RECV];
        match driver::receive_udp(port, &mut data[..max_len as usize]) {
            Ok(length) => {
                let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
                write_version(&mut bytes, NET_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..10].copy_from_slice(&(length as u16).to_le_bytes());
                bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
                bytes[NET_RECV_REPLY_SIZE..NET_RECV_REPLY_SIZE + length]
                    .copy_from_slice(&data[..length]);
                Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
            }
            Err(error) => reply_error(NET_RECV_REPLY_SIZE, error_code(error)),
        }
    }

    /// The socket open.
    fn handle_socket_open(&mut self, payload: &[u8], task_slot: usize) -> Message {
        if payload.len() != NET_SOCKET_OPEN_REQUEST_SIZE {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(kind) = read_u32(payload, 8) else {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Invalid);
        }
        if kind != NET_SOCKET_TCP && kind != NET_SOCKET_UDP {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Unsupported);
        }

        let slot = self.sockets.iter().position(|entry| entry.is_none());
        let Some(slot) = slot else {
            return reply_error(NET_SOCKET_OPEN_REPLY_SIZE, ErrorCode::Quota);
        };

        let local_port = SOCKET_BASE_PORT.saturating_add(slot as u16);
        self.sockets[slot] = Some(SocketSlot {
            task_slot,
            kind,
            local_port,
            tcp: None,
            udp_destination: None,
            udp_remote_port: 0,
        });

        let socket_id = (slot + 1) as u16;
        let mut bytes = [0u8; NET_SOCKET_OPEN_REPLY_SIZE];
        write_version(&mut bytes, NET_VERSION);
        bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
        bytes[8..10].copy_from_slice(&socket_id.to_le_bytes());
        bytes[10..12].copy_from_slice(&local_port.to_le_bytes());
        Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
    }

    /// The socket connect.
    fn handle_socket_connect(&mut self, payload: &[u8], task_slot: usize) -> Message {
        if payload.len() != NET_SOCKET_CONNECT_REQUEST_SIZE {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(socket_id) = read_u16(payload, 8) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(port) = read_u16(payload, 10) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(destination) = read_bytes(payload, 12, 4) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Invalid);
        }

        let Some(slot) = self.socket_slot(socket_id, task_slot) else {
            return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, socket_error(socket_id, task_slot));
        };

        let destination = ipv4(destination);
        match self.sockets[slot].as_mut() {
            Some(socket) => {
                if socket.kind == NET_SOCKET_TCP {
                    if socket.tcp.is_some() {
                        return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Busy);
                    }
                    match driver::connect_tcp_from(destination, port, socket.local_port) {
                        Ok(connection) => {
                            socket.tcp = Some(connection);
                            socket_reply(NET_SOCKET_CONNECT_REPLY_SIZE, socket_id)
                        }
                        Err(error) => {
                            reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, error_code(error))
                        }
                    }
                } else if socket.kind == NET_SOCKET_UDP {
                    if socket.udp_destination.is_some() {
                        return reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Busy);
                    }
                    socket.udp_destination = Some(destination);
                    socket.udp_remote_port = port;
                    socket_reply(NET_SOCKET_CONNECT_REPLY_SIZE, socket_id)
                } else {
                    reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::Unsupported)
                }
            }
            None => reply_error(NET_SOCKET_CONNECT_REPLY_SIZE, ErrorCode::BadHandle),
        }
    }

    /// The socket send.
    fn handle_socket_send(&mut self, payload: &[u8], task_slot: usize) -> Message {
        if payload.len() < NET_SOCKET_SEND_REQUEST_SIZE {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(socket_id) = read_u16(payload, 8) else {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(length) = read_u16(payload, 10) else {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }

        let length = length as usize;
        if length > NET_MAX_SOCKET_PAYLOAD
            || payload.len() != NET_SOCKET_SEND_REQUEST_SIZE + length
        {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Invalid);
        }

        let Some(slot) = self.socket_slot(socket_id, task_slot) else {
            return reply_error(NET_SOCKET_SEND_REPLY_SIZE, socket_error(socket_id, task_slot));
        };

        let data = &payload[NET_SOCKET_SEND_REQUEST_SIZE..NET_SOCKET_SEND_REQUEST_SIZE + length];
        match self.sockets[slot].as_mut() {
            Some(socket) => {
                if socket.kind == NET_SOCKET_TCP {
                    let Some(connection) = socket.tcp.as_mut() else {
                        return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Closed);
                    };
                    match driver::send_tcp(connection, data) {
                        Ok(()) => socket_sent_reply(NET_SOCKET_SEND_REPLY_SIZE, length as u16),
                        Err(error) => reply_error(NET_SOCKET_SEND_REPLY_SIZE, error_code(error)),
                    }
                } else if socket.kind == NET_SOCKET_UDP {
                    let Some(destination) = socket.udp_destination else {
                        return reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Closed);
                    };
                    match driver::send_udp_from(
                        socket.local_port,
                        destination,
                        socket.udp_remote_port,
                        data,
                    ) {
                        Ok(()) => socket_sent_reply(NET_SOCKET_SEND_REPLY_SIZE, length as u16),
                        Err(error) => reply_error(NET_SOCKET_SEND_REPLY_SIZE, error_code(error)),
                    }
                } else {
                    reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::Unsupported)
                }
            }
            None => reply_error(NET_SOCKET_SEND_REPLY_SIZE, ErrorCode::BadHandle),
        }
    }

    /// The socket receive.
    fn handle_socket_recv(&mut self, payload: &[u8], task_slot: usize) -> Message {
        if payload.len() != NET_SOCKET_RECV_REQUEST_SIZE {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(socket_id) = read_u16(payload, 8) else {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(max_len) = read_u16(payload, 10) else {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 || max_len as usize > NET_MAX_SOCKET_RECV {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Invalid);
        }

        let Some(slot) = self.socket_slot(socket_id, task_slot) else {
            return reply_error(NET_SOCKET_RECV_REPLY_SIZE, socket_error(socket_id, task_slot));
        };

        let mut data = [0u8; NET_MAX_SOCKET_RECV];
        match self.sockets[slot].as_mut() {
            Some(socket) => {
                if socket.kind == NET_SOCKET_TCP {
                    let Some(connection) = socket.tcp.as_mut() else {
                        return reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Closed);
                    };
                    match driver::receive_tcp(connection, &mut data[..max_len as usize]) {
                        Ok(length) => socket_recv_reply(length as u16, &data[..length]),
                        Err(error) => reply_error(NET_SOCKET_RECV_REPLY_SIZE, error_code(error)),
                    }
                } else if socket.kind == NET_SOCKET_UDP {
                    match driver::receive_udp_at(socket.local_port, &mut data[..max_len as usize]) {
                        Ok(length) => socket_recv_reply(length as u16, &data[..length]),
                        Err(error) => reply_error(NET_SOCKET_RECV_REPLY_SIZE, error_code(error)),
                    }
                } else {
                    reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::Unsupported)
                }
            }
            None => reply_error(NET_SOCKET_RECV_REPLY_SIZE, ErrorCode::BadHandle),
        }
    }

    /// The socket close.
    fn handle_socket_close(&mut self, payload: &[u8], task_slot: usize) -> Message {
        if payload.len() != NET_SOCKET_CLOSE_REQUEST_SIZE {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(socket_id) = read_u16(payload, 8) else {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(reserved) = read_u16(payload, 10) else {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 || reserved != 0 {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, ErrorCode::Invalid);
        }

        let Some(slot) = self.socket_slot(socket_id, task_slot) else {
            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, socket_error(socket_id, task_slot));
        };

        if let Some(socket) = self.sockets[slot].as_mut() {
            if socket.kind == NET_SOCKET_TCP {
                if let Some(connection) = socket.tcp.as_mut() {
                    match driver::close_tcp(connection) {
                        Ok(()) => {}
                        Err(error) => {
                            return reply_error(NET_SOCKET_CLOSE_REPLY_SIZE, error_code(error));
                        }
                    }
                }
            }
        }
        self.sockets[slot] = None;
        socket_reply(NET_SOCKET_CLOSE_REPLY_SIZE, socket_id)
    }

    /// An ICMP echo.
    fn handle_icmp(&self, payload: &[u8]) -> Message {
        if payload.len() < NET_ICMP_REQUEST_SIZE {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(destination) = read_bytes(payload, 8, 4) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(identifier) = read_u16(payload, 12) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(sequence) = read_u16(payload, 14) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(length) = read_u16(payload, 16) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        let Some(reserved) = read_u16(payload, 18) else {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 || reserved != 0 {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        }

        let length = length as usize;
        if length > NET_MAX_ICMP_PAYLOAD || payload.len() != NET_ICMP_REQUEST_SIZE + length {
            return reply_error(NET_ICMP_REPLY_SIZE, ErrorCode::Invalid);
        }

        let data = &payload[NET_ICMP_REQUEST_SIZE..NET_ICMP_REQUEST_SIZE + length];
        let mut reply = [0u8; NET_MAX_ICMP_REPLY];
        match driver::icmp_echo(
            ipv4(destination),
            identifier,
            sequence,
            data,
            &mut reply[..NET_MAX_ICMP_REPLY],
        ) {
            Ok(reply_length) => {
                let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
                write_version(&mut bytes, NET_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..10].copy_from_slice(&identifier.to_le_bytes());
                bytes[10..12].copy_from_slice(&sequence.to_le_bytes());
                bytes[12..14].copy_from_slice(&(reply_length as u16).to_le_bytes());
                bytes[14..16].copy_from_slice(&0u16.to_le_bytes());
                bytes[NET_ICMP_REPLY_SIZE..NET_ICMP_REPLY_SIZE + reply_length]
                    .copy_from_slice(&reply[..reply_length]);
                Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
            }
            Err(error) => reply_error(NET_ICMP_REPLY_SIZE, error_code(error)),
        }
    }

    /// A DHCP acquisition.
    fn handle_dhcp(&self, payload: &[u8]) -> Message {
        if payload.len() != NET_DHCP_REQUEST_SIZE {
            return reply_error(NET_DHCP_REPLY_SIZE, ErrorCode::Invalid);
        }
        let Some(version) = read_version(payload, 0) else {
            return reply_error(NET_DHCP_REPLY_SIZE, ErrorCode::Invalid);
        };
        if negotiate(NET_VERSION, version).is_err() {
            return reply_error(NET_DHCP_REPLY_SIZE, ErrorCode::Unsupported);
        }
        let Some(flags) = read_u32(payload, 4) else {
            return reply_error(NET_DHCP_REPLY_SIZE, ErrorCode::Invalid);
        };
        if flags != 0 {
            return reply_error(NET_DHCP_REPLY_SIZE, ErrorCode::Invalid);
        }

        match driver::dhcp_acquire() {
            Ok(lease) => {
                let mut bytes = [0u8; NET_DHCP_REPLY_SIZE];
                write_version(&mut bytes, NET_VERSION);
                bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
                bytes[8..12].copy_from_slice(&lease.address.octets());
                let server = lease.server.unwrap_or(Ipv4Addr::new([0, 0, 0, 0]));
                bytes[12..16].copy_from_slice(&server.octets());
                bytes[16..18].copy_from_slice(&(lease.message_type as u16).to_le_bytes());
                bytes[18..20].copy_from_slice(&0u16.to_le_bytes());
                Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
            }
            Err(error) => reply_error(NET_DHCP_REPLY_SIZE, error_code(error)),
        }
    }

    /// The slot a socket identifier names, or `None`.
    ///
    /// The identifier is one-based, and the slot is zero-based. The caller's
    /// task slot is checked here rather than at every use, so ownership is one
    /// rule instead of five.
    fn socket_slot(&self, socket_id: u16, task_slot: usize) -> Option<usize> {
        if socket_id == 0 {
            return None;
        }
        let slot = (socket_id as usize) - 1;
        self.sockets.get(slot).and_then(|entry| entry.as_ref()).and_then(|socket| {
            if socket.task_slot == task_slot {
                Some(slot)
            } else {
                None
            }
        })
    }
}

/// A reply carrying `socket_id` and a success status.
fn socket_reply(size: usize, socket_id: u16) -> Message {
    let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
    write_version(&mut bytes, NET_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..10].copy_from_slice(&socket_id.to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    Message::with_payload(0, &bytes[..size]).unwrap_or_else(|_| Message::new(0))
}

/// A socket-send reply carrying the number of bytes sent.
fn socket_sent_reply(size: usize, sent: u16) -> Message {
    let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
    write_version(&mut bytes, NET_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..10].copy_from_slice(&sent.to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    Message::with_payload(0, &bytes[..size]).unwrap_or_else(|_| Message::new(0))
}

/// A socket-receive reply carrying `data`.
fn socket_recv_reply(length: u16, data: &[u8]) -> Message {
    let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
    write_version(&mut bytes, NET_VERSION);
    bytes[4..8].copy_from_slice(&0i32.to_le_bytes());
    bytes[8..10].copy_from_slice(&length.to_le_bytes());
    bytes[10..12].copy_from_slice(&0u16.to_le_bytes());
    bytes[NET_SOCKET_RECV_REPLY_SIZE..NET_SOCKET_RECV_REPLY_SIZE + data.len()]
        .copy_from_slice(data);
    Message::with_payload(0, &bytes).unwrap_or_else(|_| Message::new(0))
}

/// Map a driver error to the ABI's error code.
fn error_code(error: NetError) -> ErrorCode {
    match error {
        NetError::Timeout => ErrorCode::Timeout,
        NetError::BufferSmall => ErrorCode::BufferSmall,
        NetError::FrameTooLong => ErrorCode::Invalid,
        NetError::Queue(_) | NetError::BadResponse => ErrorCode::Invalid,
        _ => ErrorCode::Unsupported,
    }
}

/// The error to report when a socket identifier is unusable for this caller.
fn socket_error(socket_id: u16, task_slot: usize) -> ErrorCode {
    if socket_id == 0 {
        return ErrorCode::Invalid;
    }
    let slot = (socket_id as usize) - 1;
    if slot >= MAX_SOCKETS {
        return ErrorCode::BadHandle;
    }
    let _ = task_slot;
    ErrorCode::BadHandle
}

/// A reply carrying only a status, padded to the operation's reply size.
fn reply_error(size: usize, code: ErrorCode) -> Message {
    let mut bytes = [0u8; nova_abi::ipc::MAX_INLINE_PAYLOAD];
    if size > bytes.len() || size < 8 {
        return Message::new(0);
    }
    bytes[..size].fill(0);
    write_version(&mut bytes, NET_VERSION);
    bytes[4..8].copy_from_slice(&code.code().to_le_bytes());
    Message::with_payload(0, &bytes[..size]).unwrap_or_else(|_| Message::new(0))
}

fn write_version(bytes: &mut [u8], version: nova_protocols::ProtocolVersion) {
    bytes[0..2].copy_from_slice(&version.major.to_le_bytes());
    bytes[2..4].copy_from_slice(&version.minor.to_le_bytes());
}

fn read_version(payload: &[u8], offset: usize) -> Option<nova_protocols::ProtocolVersion> {
    let major = read_u16(payload, offset)?;
    let minor = read_u16(payload, offset.checked_add(2)?)?;
    Some(nova_protocols::ProtocolVersion::new(major, minor))
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let bytes = payload.get(offset..end)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let bytes = payload.get(offset..end)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_bytes(payload: &[u8], offset: usize, length: usize) -> Option<[u8; 4]> {
    let end = offset.checked_add(length)?;
    let bytes = payload.get(offset..end)?;
    let mut out = [0u8; 4];
    out[..length].copy_from_slice(bytes);
    Some(out)
}

fn ipv4(bytes: [u8; 4]) -> Ipv4Addr {
    Ipv4Addr::new([bytes[0], bytes[1], bytes[2], bytes[3]])
}
