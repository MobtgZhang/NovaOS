//! The kernel-side network service.
//!
//! # What it serves
//!
//! Four bounded operations: link query, DNS resolve, UDP send, and UDP receive.
//! Each is a fixed `nova-protocols` layout plus at most a payload budget, and
//! each calls the NIC driver through `drivers::net` rather than handing a client
//! a device address.
//!
//! # Version first, then fields
//!
//! An unknown version is refused before any field is interpreted. That matters
//! for a protocol whose variable payload is a DNS name or a datagram: a peer
//! built against a newer version may have moved the header, and interpreting
//! the bytes under the old layout would read a name from the wrong offset.
//!
//! # Errors are service errors, not device errors
//!
//! The driver's `NetError` is mapped to the ABI's `ErrorCode`. A timeout is
//! `E_TIMEOUT`, a malformed request is `E_INVALID`, a missing NIC or an
//! unreachable destination is `E_UNSUPPORTED`, and a buffer too small for the
//! received datagram is `E_BUFFER_SMALL`.

use nova_abi::error::ErrorCode;
use nova_ipc::Message;
use nova_net::Ipv4Addr;
use nova_protocols::negotiate;
use nova_protocols::net::{
    NET_LINK_REPLY_SIZE, NET_LINK_REQUEST_SIZE, NET_MAX_DATAGRAM, NET_MAX_NAME, NET_MAX_RECV,
    NET_OP_LINK_QUERY, NET_OP_RECV, NET_OP_RESOLVE, NET_OP_SEND, NET_RECV_REPLY_SIZE,
    NET_RECV_REQUEST_SIZE, NET_RESOLVE_REPLY_SIZE, NET_RESOLVE_REQUEST_SIZE, NET_SEND_REPLY_SIZE,
    NET_SEND_REQUEST_SIZE, NET_VERSION,
};

use crate::drivers::net as driver;
use crate::drivers::virtio::net::NetError;

/// The network service's state.
///
/// The service itself carries no mutable state: the link facts belong to the
/// driver, and the packet path is bounded by the driver's frames. The type still
/// exists so the kernel's service layer has a named owner and a `reset` hook.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetService;

impl NetService {
    /// A network service.
    pub const fn new() -> Self {
        Self
    }

    /// Reset the service.
    ///
    /// There is nothing to reset; the driver owns the device. Keeping the hook
    /// makes the service layer uniform with the other services.
    pub fn reset(&mut self) {}

    /// Handle one network-protocol request.
    pub fn handle_message(&self, request: &Message) -> Message {
        match request.opcode() {
            NET_OP_LINK_QUERY => self.handle_link(request.payload()),
            NET_OP_RESOLVE => self.handle_resolve(request.payload()),
            NET_OP_SEND => self.handle_send(request.payload()),
            NET_OP_RECV => self.handle_recv(request.payload()),
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

    /// The UDP send.
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
        let destination = Ipv4Addr::new([
            destination[0],
            destination[1],
            destination[2],
            destination[3],
        ]);
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

    /// The UDP receive.
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
}

/// Map a driver error to the ABI's error code.
fn error_code(error: NetError) -> ErrorCode {
    match error {
        NetError::Timeout => ErrorCode::Timeout,
        NetError::BufferSmall => ErrorCode::BufferSmall,
        NetError::FrameTooLong => ErrorCode::Invalid,
        _ => ErrorCode::Unsupported,
    }
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
