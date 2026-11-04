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

All payloads live inside `nova_abi::ipc::MAX_INLINE_PAYLOAD`. A name or
datagram that would exceed the budget is refused with `E_INVALID`; it is never
truncated. A truncated DNS name is a different name, and a truncated datagram is
a different datagram.

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

## Driver limits

The driver negotiates `VIRTIO_F_VERSION_1`, `VIRTIO_NET_F_MAC`, and
`VIRTIO_NET_F_STATUS`. It uses the 12-byte VirtIO 1.0 net header, one receive
queue and one transmit queue, and a bounded frame staging area. Each request
posts the receive buffer before sending, because QEMU's slirp backend answers
ARP and DNS synchronously and drops a reply when no receive buffer is available.

The QEMU `user` backend supplies the fixed `10.0.2.0/24` network:

- guest `10.0.2.15`
- gateway `10.0.2.2`
- DNS `10.0.2.3`

These are constants of the emulated environment, not DHCP results.

## What is not implemented

No TCP, DHCP, IPv6, general socket API, blocking socket semantics, or real
hardware networking. The M7 suite proves ARP, IPv4, UDP, and DNS A-record
resolution on the emulated NIC; it does not prove a full network stack.
