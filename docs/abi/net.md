# NovaOS network service

The network is a versioned kernel-side service, not an ambient socket API.
A client asks for one bounded operation; the service reaches the NIC on the
caller's behalf and never hands the caller a device address, a DMA buffer, or a
raw network handle.

## Owner

`crates/nova-protocols/src/net.rs` owns every wire number and layout. The
kernel-side handler is `kernel/src/services/net.rs`; the driver is
`kernel/src/drivers/virtio/net.rs`; the host-testable wire formats are
`crates/nova-net/src/lib.rs`.

## Version

`NET_VERSION` is `1.0`. The service applies the same rule as the other
protocols: the major version must match, and an offered minor must not be newer
than the implementation's minor. An unknown version is refused before any
field is read.

## Operations

| Opcode | Operation | Header bytes | Variable data | Reply bytes |
| --- | --- | --- | --- | --- |
| `1` | link query | 8 | none | 24 |
| `2` | DNS resolve | 12 | name | 16 |
| `3` | UDP send | 16 | datagram | 12 |
| `4` | UDP receive | 12 | none | 12 + datagram |
| `5` | socket open | 12 | none | 12 |
| `6` | TCP connect | 16 | none | 12 |
| `7` | socket send | 12 | payload | 12 |
| `8` | socket receive | 12 | none | 12 + payload |
| `9` | socket close | 12 | none | 12 |
| `10` | ICMP echo | 20 | payload | 16 + payload |
| `11` | DHCP acquire | 8 | none | 20 |

All payloads live inside `nova_abi::ipc::MAX_INLINE_PAYLOAD`. A name, datagram,
socket payload, or ICMP payload that would exceed the budget is refused with
`E_INVALID`; it is never truncated. A truncated DNS name is a different name, and
a truncated datagram is a different datagram.

## Layouts

The fixed request and reply layouts are `#[repr(C)]` structs, and the crate
asserts their sizes and offsets at compile time. The implementation uses
`ProtocolVersion` in the first four bytes, a little-endian `i32` status at
offset 4, and then the operation-specific fields.

- `NetLinkRequest`: version, flags. Flags must be zero in v1.
- `NetLinkReply`: version, status, guest address, gateway, MAC, link-up word.
- `NetResolveRequest`: version, flags, name length, reserved. Both flags and
  reserved must be zero.
- `NetResolveReply`: version, status, resolved address, reserved.
- `NetSendRequest`: version, flags, destination address, destination port,
  datagram length.
- `NetSendReply`: version, status, sent length, reserved.
- `NetRecvRequest`: version, flags, source port, caller's maximum reply length.
- `NetRecvReply`: version, status, received length, reserved, then the
  datagram bytes.
- `NetSocketOpenRequest`: version, flags, socket kind. `NET_SOCKET_TCP` and
  `NET_SOCKET_UDP` are the defined kinds.
- `NetSocketOpenReply`: version, status, socket identifier, assigned local port.
- `NetSocketConnectRequest`: version, flags, socket, remote port, remote IPv4
  address.
- `NetSocketConnectReply`: version, status, socket, reserved.
- `NetSocketSendRequest`: version, flags, socket, payload length, then payload.
- `NetSocketSendReply`: version, status, sent length, reserved.
- `NetSocketRecvRequest`: version, flags, socket, caller's maximum reply length.
- `NetSocketRecvReply`: version, status, received length, reserved, then payload.
- `NetSocketCloseRequest`: version, flags, socket, reserved.
- `NetSocketCloseReply`: version, status, socket, reserved.
- `NetIcmpRequest`: version, flags, destination, identifier, sequence,
  payload length, reserved, then payload.
- `NetIcmpReply`: version, status, identifier, sequence, payload length,
  reserved, then payload.
- `NetDhcpRequest`: version, flags.
- `NetDhcpReply`: version, status, assigned address, server address, DHCP
  message type, reserved.

## Socket ownership

A socket identifier is not ambient authority. The service stores at most eight
sockets in a fixed pool, and each slot records the task that opened it. A send,
receive, or close for a socket is accepted only when the calling task slot owns
that socket; a mismatched caller receives `E_INVALID` rather than access to the
socket. Closing a socket releases the slot.

## Driver limits

The driver negotiates `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`, and
`VIRTIO_NET_F_STATUS`. It uses the 12-byte VirtIO 1.0 net header, one receive
queue and one transmit queue, and a bounded frame staging area. Each request
posts the receive buffer before sending, because QEMU's slirp backend answers
ARP and DNS synchronously and drops a reply when no receive buffer is available.
TCP uses the same receive priming: the handshake, request, and response paths
prime the queue before transmitting, and the TCP pseudo-header checksum uses the
protocol word `0x0006`. Off-link destinations fall back to the gateway MAC when
ARP does not resolve them. Driver timeouts unwind the request rather than
leaving the device in a stuck state.

TCP verification is against the QEMU `guestfwd` endpoint
`10.0.2.100:80`, not against arbitrary external hosts. DHCP verification is
against the QEMU user-network DHCP server. ICMP echo is verified against the
emulated gateway.

The QEMU `user` backend supplies the fixed `10.0.2.0/24` network:

- guest `10.0.2.15`
- gateway `10.0.2.2`
- DNS `10.0.2.3`

These are constants of the emulated environment, not DHCP results.

## What is not implemented

No IPv6, blocking socket semantics, arbitrary external-host TCP reachability, or
real-hardware networking. The M7 suite proves ARP, IPv4, UDP, DNS A-record
resolution, TCP handshake/data/close against a `guestfwd` peer, ICMP echo, DHCP
lease acquisition, and bounded socket ownership on the emulated NIC. It does not
prove a full network stack or general Internet reachability.
