//! The network service protocol.
//!
//! # A versioned service, not a socket API hidden in the kernel
//!
//! A client asks a kernel-owned service by operation number, and the reply is
//! a fixed layout plus bounded payload bytes. The protocol does not expose a
//! NIC address, a DMA buffer, or an ambient network handle: it names a service
//! and asks the service to act on the caller's behalf. That is what keeps the
//! network a service rather than kernel authority.
//!
//! # Bounded payloads
//!
//! The IPC message's inline payload is [`nova_abi::ipc::MAX_INLINE_PAYLOAD`]
//! bytes. The fixed request header plus variable data must fit inside it:
//!
//! | operation | header | data | per call |
//! | --- | --- | --- | --- |
//! | `link_query` | 8 | none | 8 |
//! | `resolve` | 12 | DNS name | 52 |
//! | `send` | 16 | datagram | 48 |
//! | `recv` | 12 | none (reply carries data) | 12 |
//!
//! A name or datagram longer than the budget is refused by the service rather
//! than silently truncated. A truncated DNS name is a different name, and a
//! truncated datagram is a different datagram; both would be worse than an
//! explicit refusal.
//!
//! # Layout is a contract
//!
//! Every fixed layout is `#[repr(C)]` with compile-time size and offset
//! assertions. The service and its clients are separate binaries, so a field
//! that moved would be read as a different message rather than caught by the
//! compiler.

use crate::ProtocolVersion;

/// The network protocol version this implementation speaks.
pub const NET_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// Ask for the current link state.
pub const NET_OP_LINK_QUERY: u64 = 1;

/// Resolve a DNS name to one IPv4 address.
pub const NET_OP_RESOLVE: u64 = 2;

/// Send one UDP datagram to an IPv4 destination.
pub const NET_OP_SEND: u64 = 3;

/// Receive one UDP datagram.
pub const NET_OP_RECV: u64 = 4;

/// Open a bounded socket.
pub const NET_OP_SOCKET_OPEN: u64 = 5;

/// Connect a TCP socket.
pub const NET_OP_SOCKET_CONNECT: u64 = 6;

/// Send bytes on a socket.
pub const NET_OP_SOCKET_SEND: u64 = 7;

/// Receive bytes from a socket.
pub const NET_OP_SOCKET_RECV: u64 = 8;

/// Close a socket.
pub const NET_OP_SOCKET_CLOSE: u64 = 9;

/// Send an ICMP echo request.
pub const NET_OP_ICMP_ECHO: u64 = 10;

/// Acquire an address from DHCP.
pub const NET_OP_DHCP: u64 = 11;

/// Socket kinds.
pub const NET_SOCKET_TCP: u32 = 1;
pub const NET_SOCKET_UDP: u32 = 2;
pub const NET_SOCKET_ICMP: u32 = 3;

/// Maximum bytes one socket send or receive can carry.
pub const NET_MAX_SOCKET_PAYLOAD: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SOCKET_SEND_REQUEST_SIZE;

/// Maximum bytes one socket receive reply can carry.
pub const NET_MAX_SOCKET_RECV: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SOCKET_RECV_REPLY_SIZE;

/// Maximum bytes one ICMP request can carry.
pub const NET_MAX_ICMP_PAYLOAD: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_ICMP_REQUEST_SIZE;

/// Maximum bytes one ICMP reply can carry.
pub const NET_MAX_ICMP_REPLY: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_ICMP_REPLY_SIZE;

/// A request to open a bounded socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketOpenRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// One of [`NET_SOCKET_TCP`] or [`NET_SOCKET_UDP`].
    pub kind: u32,
}

/// Size of a socket-open request, in bytes.
pub const NET_SOCKET_OPEN_REQUEST_SIZE: usize = 12;

/// A reply to a socket open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketOpenReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The socket's bounded identifier.
    pub socket: u16,
    /// The local port the socket was assigned.
    pub local_port: u16,
}

/// Size of a socket-open reply, in bytes.
pub const NET_SOCKET_OPEN_REPLY_SIZE: usize = 12;

/// A request to connect a socket to a remote IPv4 endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketConnectRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// The socket to connect.
    pub socket: u16,
    /// The remote port.
    pub port: u16,
    /// The remote IPv4 address.
    pub destination: [u8; 4],
}

/// Size of a socket-connect request, in bytes.
pub const NET_SOCKET_CONNECT_REQUEST_SIZE: usize = 16;

/// A reply to a socket connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketConnectReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The socket that was connected.
    pub socket: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a socket-connect reply, in bytes.
pub const NET_SOCKET_CONNECT_REPLY_SIZE: usize = 12;

/// A request to send bytes on a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketSendRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// The socket to send on.
    pub socket: u16,
    /// How many payload bytes follow the header.
    pub length: u16,
}

/// Size of a socket-send request header, in bytes.
pub const NET_SOCKET_SEND_REQUEST_SIZE: usize = 12;

/// A reply to a socket send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketSendReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// How many bytes were sent.
    pub sent: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a socket-send reply, in bytes.
pub const NET_SOCKET_SEND_REPLY_SIZE: usize = 12;

/// A request to receive bytes from a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketRecvRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// The socket to receive from.
    pub socket: u16,
    /// Maximum bytes the caller's reply buffer can hold.
    pub max_len: u16,
}

/// Size of a socket-receive request header, in bytes.
pub const NET_SOCKET_RECV_REQUEST_SIZE: usize = 12;

/// A reply carrying bytes from a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketRecvReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// How many payload bytes follow the header.
    pub length: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a socket-receive reply header, in bytes.
pub const NET_SOCKET_RECV_REPLY_SIZE: usize = 12;

/// A request to close a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketCloseRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// The socket to close.
    pub socket: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a socket-close request, in bytes.
pub const NET_SOCKET_CLOSE_REQUEST_SIZE: usize = 12;

/// A reply to a socket close.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSocketCloseReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The socket that was closed.
    pub socket: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a socket-close reply, in bytes.
pub const NET_SOCKET_CLOSE_REPLY_SIZE: usize = 12;

/// A request to send an ICMP echo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetIcmpRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// The IPv4 destination.
    pub destination: [u8; 4],
    /// The echo identifier.
    pub identifier: u16,
    /// The echo sequence number.
    pub sequence: u16,
    /// How many payload bytes follow the header.
    pub length: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of an ICMP request header, in bytes.
pub const NET_ICMP_REQUEST_SIZE: usize = 20;

/// A reply carrying an ICMP echo result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetIcmpReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The echo identifier.
    pub identifier: u16,
    /// The echo sequence number.
    pub sequence: u16,
    /// How many payload bytes follow the header.
    pub length: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of an ICMP reply header, in bytes.
pub const NET_ICMP_REPLY_SIZE: usize = 16;

/// A request to acquire an address from DHCP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetDhcpRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
}

/// Size of a DHCP request, in bytes.
pub const NET_DHCP_REQUEST_SIZE: usize = 8;

/// A reply from DHCP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetDhcpReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The address the server assigned.
    pub address: [u8; 4],
    /// The DHCP server's address, or zero when it was not reported.
    pub server: [u8; 4],
    /// The DHCP message type.
    pub message_type: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a DHCP reply, in bytes.
pub const NET_DHCP_REPLY_SIZE: usize = 20;

/// Maximum DNS name bytes that fit beside the resolve header.
pub const NET_MAX_NAME: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_RESOLVE_REQUEST_SIZE;

/// Maximum datagram bytes that fit beside the send header.
pub const NET_MAX_DATAGRAM: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SEND_REQUEST_SIZE;

/// Maximum datagram bytes one receive reply can carry.
pub const NET_MAX_RECV: usize = nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_RECV_REPLY_SIZE;

/// Flag: the resolve request asks only for an A record.
///
/// `0` is the only defined value in v1. The field exists so a future revision
/// can add record-family selection without moving the rest of the layout.
pub const NET_FLAG_A_ONLY: u32 = 0;

/// A request for the link state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetLinkRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
}

/// Size of a link request, in bytes.
pub const NET_LINK_REQUEST_SIZE: usize = 8;

/// A reply carrying the link state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetLinkReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The guest IPv4 address, when the link is up.
    pub address: [u8; 4],
    /// The gateway IPv4 address, when the link is up.
    pub gateway: [u8; 4],
    /// The NIC's Ethernet address.
    pub mac: [u8; 6],
    /// Whether the device reports the link as up.
    pub link_up: u16,
}

/// Size of a link reply, in bytes.
pub const NET_LINK_REPLY_SIZE: usize = 24;

/// A request to resolve a DNS name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetResolveRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// How many payload bytes carry the DNS name.
    pub name_len: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a resolve request header, in bytes.
pub const NET_RESOLVE_REQUEST_SIZE: usize = 12;

/// A reply carrying one IPv4 address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetResolveReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// The resolved IPv4 address.
    pub address: [u8; 4],
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a resolve reply, in bytes.
pub const NET_RESOLVE_REPLY_SIZE: usize = 16;

/// A request to send one UDP datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSendRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// Destination IPv4 address.
    pub destination: [u8; 4],
    /// Destination UDP port.
    pub port: u16,
    /// How many payload bytes carry the datagram.
    pub length: u16,
}

/// Size of a send request header, in bytes.
pub const NET_SEND_REQUEST_SIZE: usize = 16;

/// A reply to a send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetSendReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// How many datagram bytes were sent.
    pub sent: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a send reply, in bytes.
pub const NET_SEND_REPLY_SIZE: usize = 12;

/// A request to receive one UDP datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetRecvRequest {
    /// The protocol version the sender speaks.
    pub version: ProtocolVersion,
    /// Reserved flags. Must be zero in v1.
    pub flags: u32,
    /// UDP port to receive from.
    pub port: u16,
    /// Maximum datagram bytes the caller's reply buffer can hold.
    pub max_len: u16,
}

/// Size of a receive request header, in bytes.
pub const NET_RECV_REQUEST_SIZE: usize = 12;

/// A reply carrying one datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetRecvReply {
    /// The protocol version the service speaks.
    pub version: ProtocolVersion,
    /// Status code: zero for success, or a service error code.
    pub status: i32,
    /// How many payload bytes carry the datagram.
    pub length: u16,
    /// Reserved. Must be zero.
    pub reserved: u16,
}

/// Size of a receive reply header, in bytes.
pub const NET_RECV_REPLY_SIZE: usize = 12;

const _: () = assert!(core::mem::size_of::<NetLinkRequest>() == NET_LINK_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetLinkReply>() == NET_LINK_REPLY_SIZE);
const _: () = assert!(core::mem::size_of::<NetResolveRequest>() == NET_RESOLVE_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetResolveReply>() == NET_RESOLVE_REPLY_SIZE);
const _: () = assert!(core::mem::size_of::<NetSendRequest>() == NET_SEND_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetSendReply>() == NET_SEND_REPLY_SIZE);
const _: () = assert!(core::mem::size_of::<NetRecvRequest>() == NET_RECV_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetRecvReply>() == NET_RECV_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(NetLinkReply, address) == 8);
const _: () = assert!(core::mem::offset_of!(NetLinkReply, gateway) == 12);
const _: () = assert!(core::mem::offset_of!(NetLinkReply, mac) == 16);
const _: () = assert!(core::mem::offset_of!(NetLinkReply, link_up) == 22);
const _: () = assert!(core::mem::offset_of!(NetResolveRequest, name_len) == 8);
const _: () = assert!(core::mem::offset_of!(NetSendRequest, destination) == 8);
const _: () = assert!(core::mem::offset_of!(NetSendRequest, port) == 12);
const _: () = assert!(core::mem::offset_of!(NetRecvRequest, port) == 8);
const _: () = assert!(core::mem::offset_of!(NetRecvRequest, max_len) == 10);

const _: () = assert!(
    core::mem::size_of::<NetSocketOpenRequest>() == NET_SOCKET_OPEN_REQUEST_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketOpenReply>() == NET_SOCKET_OPEN_REPLY_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketConnectRequest>() == NET_SOCKET_CONNECT_REQUEST_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketConnectReply>() == NET_SOCKET_CONNECT_REPLY_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketSendRequest>() == NET_SOCKET_SEND_REQUEST_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketSendReply>() == NET_SOCKET_SEND_REPLY_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketRecvRequest>() == NET_SOCKET_RECV_REQUEST_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketRecvReply>() == NET_SOCKET_RECV_REPLY_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketCloseRequest>() == NET_SOCKET_CLOSE_REQUEST_SIZE
);
const _: () = assert!(
    core::mem::size_of::<NetSocketCloseReply>() == NET_SOCKET_CLOSE_REPLY_SIZE
);
const _: () = assert!(core::mem::size_of::<NetIcmpRequest>() == NET_ICMP_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetIcmpReply>() == NET_ICMP_REPLY_SIZE);
const _: () = assert!(core::mem::size_of::<NetDhcpRequest>() == NET_DHCP_REQUEST_SIZE);
const _: () = assert!(core::mem::size_of::<NetDhcpReply>() == NET_DHCP_REPLY_SIZE);

const _: () = assert!(core::mem::offset_of!(NetSocketOpenRequest, kind) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketOpenReply, socket) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketOpenReply, local_port) == 10);
const _: () = assert!(core::mem::offset_of!(NetSocketConnectRequest, socket) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketConnectRequest, port) == 10);
const _: () = assert!(core::mem::offset_of!(NetSocketConnectRequest, destination) == 12);
const _: () = assert!(core::mem::offset_of!(NetSocketSendRequest, socket) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketSendRequest, length) == 10);
const _: () = assert!(core::mem::offset_of!(NetSocketRecvRequest, socket) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketRecvRequest, max_len) == 10);
const _: () = assert!(core::mem::offset_of!(NetSocketRecvReply, length) == 8);
const _: () = assert!(core::mem::offset_of!(NetSocketCloseRequest, socket) == 8);
const _: () = assert!(core::mem::offset_of!(NetIcmpRequest, destination) == 8);
const _: () = assert!(core::mem::offset_of!(NetIcmpRequest, identifier) == 12);
const _: () = assert!(core::mem::offset_of!(NetIcmpRequest, sequence) == 14);
const _: () = assert!(core::mem::offset_of!(NetIcmpRequest, length) == 16);
const _: () = assert!(core::mem::offset_of!(NetIcmpReply, identifier) == 8);
const _: () = assert!(core::mem::offset_of!(NetIcmpReply, length) == 12);
const _: () = assert!(core::mem::offset_of!(NetDhcpReply, address) == 8);
const _: () = assert!(core::mem::offset_of!(NetDhcpReply, server) == 12);
const _: () = assert!(core::mem::offset_of!(NetDhcpReply, message_type) == 16);

#[cfg(test)]
mod tests {
    use super::{
        NET_DHCP_REPLY_SIZE, NET_DHCP_REQUEST_SIZE, NET_ICMP_REPLY_SIZE, NET_ICMP_REQUEST_SIZE,
        NET_LINK_REPLY_SIZE, NET_LINK_REQUEST_SIZE, NET_MAX_DATAGRAM, NET_MAX_ICMP_PAYLOAD,
        NET_MAX_ICMP_REPLY, NET_MAX_NAME, NET_MAX_RECV, NET_MAX_SOCKET_PAYLOAD,
        NET_MAX_SOCKET_RECV, NET_RECV_REPLY_SIZE, NET_RECV_REQUEST_SIZE, NET_RESOLVE_REPLY_SIZE,
        NET_RESOLVE_REQUEST_SIZE, NET_SEND_REPLY_SIZE, NET_SEND_REQUEST_SIZE,
        NET_SOCKET_CLOSE_REPLY_SIZE, NET_SOCKET_CLOSE_REQUEST_SIZE, NET_SOCKET_CONNECT_REPLY_SIZE,
        NET_SOCKET_CONNECT_REQUEST_SIZE, NET_SOCKET_OPEN_REPLY_SIZE,
        NET_SOCKET_OPEN_REQUEST_SIZE, NET_SOCKET_RECV_REPLY_SIZE, NET_SOCKET_RECV_REQUEST_SIZE,
        NET_SOCKET_SEND_REPLY_SIZE, NET_SOCKET_SEND_REQUEST_SIZE, NET_VERSION,
        NetDhcpReply, NetDhcpRequest, NetIcmpReply, NetIcmpRequest, NetLinkReply, NetLinkRequest,
        NetRecvReply, NetRecvRequest, NetResolveReply, NetResolveRequest, NetSendReply,
        NetSendRequest, NetSocketCloseReply, NetSocketCloseRequest, NetSocketConnectReply,
        NetSocketConnectRequest, NetSocketOpenReply, NetSocketOpenRequest, NetSocketRecvReply,
        NetSocketRecvRequest, NetSocketSendReply, NetSocketSendRequest,
    };
    use crate::ProtocolVersion;

    #[test]
    fn the_fixed_layouts_are_the_documented_sizes() {
        assert_eq!(
            core::mem::size_of::<NetLinkRequest>(),
            NET_LINK_REQUEST_SIZE
        );
        assert_eq!(core::mem::size_of::<NetLinkReply>(), NET_LINK_REPLY_SIZE);
        assert_eq!(
            core::mem::size_of::<NetResolveRequest>(),
            NET_RESOLVE_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetResolveReply>(),
            NET_RESOLVE_REPLY_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSendRequest>(),
            NET_SEND_REQUEST_SIZE
        );
        assert_eq!(core::mem::size_of::<NetSendReply>(), NET_SEND_REPLY_SIZE);
        assert_eq!(
            core::mem::size_of::<NetRecvRequest>(),
            NET_RECV_REQUEST_SIZE
        );
        assert_eq!(core::mem::size_of::<NetRecvReply>(), NET_RECV_REPLY_SIZE);
    }

    #[test]
    fn variable_data_fits_beside_the_header() {
        assert_eq!(
            NET_MAX_NAME,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_RESOLVE_REQUEST_SIZE
        );
        assert_eq!(
            NET_MAX_DATAGRAM,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SEND_REQUEST_SIZE
        );
        assert_eq!(
            NET_MAX_RECV,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_RECV_REPLY_SIZE
        );
        assert!(NET_MAX_NAME > 0 && NET_MAX_DATAGRAM > 0 && NET_MAX_RECV > 0);
    }

    #[test]
    fn a_name_can_be_longer_than_one_label() {
        // The budget is a byte budget, not a label budget: `example.com` must fit.
        assert!(b"example.com".len() <= NET_MAX_NAME);
    }

    #[test]
    fn v1_is_the_only_version_this_protocol_defines() {
        assert_eq!(NET_VERSION, ProtocolVersion::new(1, 0));
    }

    #[test]
    fn socket_icmp_and_dhcp_layouts_are_the_documented_sizes() {
        assert_eq!(
            core::mem::size_of::<NetSocketOpenRequest>(),
            NET_SOCKET_OPEN_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketOpenReply>(),
            NET_SOCKET_OPEN_REPLY_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketConnectRequest>(),
            NET_SOCKET_CONNECT_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketConnectReply>(),
            NET_SOCKET_CONNECT_REPLY_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketSendRequest>(),
            NET_SOCKET_SEND_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketSendReply>(),
            NET_SOCKET_SEND_REPLY_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketRecvRequest>(),
            NET_SOCKET_RECV_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketRecvReply>(),
            NET_SOCKET_RECV_REPLY_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketCloseRequest>(),
            NET_SOCKET_CLOSE_REQUEST_SIZE
        );
        assert_eq!(
            core::mem::size_of::<NetSocketCloseReply>(),
            NET_SOCKET_CLOSE_REPLY_SIZE
        );
        assert_eq!(core::mem::size_of::<NetIcmpRequest>(), NET_ICMP_REQUEST_SIZE);
        assert_eq!(core::mem::size_of::<NetIcmpReply>(), NET_ICMP_REPLY_SIZE);
        assert_eq!(core::mem::size_of::<NetDhcpRequest>(), NET_DHCP_REQUEST_SIZE);
        assert_eq!(core::mem::size_of::<NetDhcpReply>(), NET_DHCP_REPLY_SIZE);
    }

    #[test]
    fn every_variable_payload_fits_the_inline_budget() {
        assert_eq!(
            NET_MAX_SOCKET_PAYLOAD,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SOCKET_SEND_REQUEST_SIZE
        );
        assert_eq!(
            NET_MAX_SOCKET_RECV,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_SOCKET_RECV_REPLY_SIZE
        );
        assert_eq!(
            NET_MAX_ICMP_PAYLOAD,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_ICMP_REQUEST_SIZE
        );
        assert_eq!(
            NET_MAX_ICMP_REPLY,
            nova_abi::ipc::MAX_INLINE_PAYLOAD - NET_ICMP_REPLY_SIZE
        );
        assert!(NET_MAX_SOCKET_PAYLOAD > 0 && NET_MAX_ICMP_PAYLOAD > 0);
    }
}
