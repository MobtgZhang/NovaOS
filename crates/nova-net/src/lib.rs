//! NovaOS network wire formats.
//!
//! This crate is deliberately *not* a socket API and not a device driver. It is
//! the pure, bounded description of the frames that travel between the kernel's
//! VirtIO-net driver and the network service: Ethernet, ARP, IPv4, UDP, and DNS.
//! Keeping that description separate means the kernel driver can be tested
//! against a real device while the packet arithmetic is tested on the host.
//!
//! # Why no allocations
//!
//! Every builder writes into a caller-supplied byte buffer and returns the
//! number of bytes written. Every parser returns a borrowed slice. A network
//! stack that allocated per packet would make the kernel's bounded-memory
//! argument false, and the IPC payload limit is 64 bytes, so a protocol that
//! copied packets into heap buffers would also be the wrong shape for the
//! service layer.
//!
//! # What is deliberately absent
//!
//! TCP, DNS caching, IPv6, and fragmentation are not here. v0.5.0's network gate
//! is "basic reachability": a DNS query over UDP, not a general transport stack.
//! A parser that silently accepted a fragmented IPv4 packet would be claiming a
//! reassembly feature this crate does not implement, so a fragment is refused.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

/// Bytes in an Ethernet header: destination, source, and ethertype.
pub const ETHERNET_HEADER_BYTES: usize = 14;

/// Bytes in an IPv4 header without options.
pub const IPV4_HEADER_BYTES: usize = 20;

/// Bytes in a UDP header.
pub const UDP_HEADER_BYTES: usize = 8;

/// Bytes in a DNS message header.
pub const DNS_HEADER_BYTES: usize = 12;

/// Ethertype for IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;

/// Ethertype for ARP.
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// ARP request opcode.
pub const ARP_REQUEST: u16 = 1;

/// ARP reply opcode.
pub const ARP_REPLY: u16 = 2;

/// IP protocol number for UDP.
pub const IP_PROTOCOL_UDP: u8 = 17;

/// DNS record type for an IPv4 address.
pub const DNS_TYPE_A: u16 = 1;

/// DNS class for Internet records.
pub const DNS_CLASS_IN: u16 = 1;

/// The Ethernet broadcast address.
pub const BROADCAST_MAC: MacAddr = MacAddr([0xff; 6]);

/// A six-byte Ethernet address.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// Construct an address from six octets.
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    /// The six octets, in wire order.
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

/// A four-byte IPv4 address.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    /// Construct an address from four octets.
    pub const fn new(octets: [u8; 4]) -> Self {
        Self(octets)
    }

    /// The four octets, in wire order.
    pub const fn octets(self) -> [u8; 4] {
        self.0
    }

    /// The address as one big-endian word.
    pub fn as_u32(self) -> u32 {
        ((self.0[0] as u32) << 24)
            | ((self.0[1] as u32) << 16)
            | ((self.0[2] as u32) << 8)
            | (self.0[3] as u32)
    }

    /// The address as one big-endian word, from a wire value.
    pub const fn from_u32(value: u32) -> Self {
        Self([
            (value >> 24) as u8,
            (value >> 16) as u8,
            (value >> 8) as u8,
            value as u8,
        ])
    }
}

/// One parsed Ethernet frame.
#[derive(Clone, Copy, Debug)]
pub struct EthernetFrame<'a> {
    /// Destination MAC.
    pub destination: MacAddr,
    /// Source MAC.
    pub source: MacAddr,
    /// The ethertype.
    pub ethertype: u16,
    /// The payload after the fourteen-byte header.
    pub payload: &'a [u8],
}

/// Build an Ethernet frame into `out`.
///
/// Returns the number of bytes written, or `None` when the buffer is too
/// small. The caller owns the buffer; this function never allocates.
pub fn build_ethernet(
    destination: MacAddr,
    source: MacAddr,
    ethertype: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = ETHERNET_HEADER_BYTES + payload.len();
    if total > out.len() {
        return None;
    }

    out[0..6].copy_from_slice(&destination.0);
    out[6..12].copy_from_slice(&source.0);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    out[14..total].copy_from_slice(payload);
    Some(total)
}

/// Parse an Ethernet frame.
pub fn parse_ethernet(frame: &[u8]) -> Option<EthernetFrame<'_>> {
    if frame.len() < ETHERNET_HEADER_BYTES {
        return None;
    }

    let mut destination = [0u8; 6];
    let mut source = [0u8; 6];
    destination.copy_from_slice(&frame[0..6]);
    source.copy_from_slice(&frame[6..12]);
    let ethertype = read_u16(frame, 12)?;
    Some(EthernetFrame {
        destination: MacAddr(destination),
        source: MacAddr(source),
        ethertype,
        payload: &frame[ETHERNET_HEADER_BYTES..],
    })
}

/// Build an ARP request for `target`.
///
/// The Ethernet destination is the broadcast address; an ARP request that
/// named a unicast destination would be asking a host that does not yet know
/// the answer to answer it.
pub fn build_arp_request(
    source_mac: MacAddr,
    source_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
    out: &mut [u8],
) -> Option<usize> {
    let mut arp = [0u8; 28];
    arp[0..2].copy_from_slice(&1u16.to_be_bytes()); // hardware type: Ethernet
    arp[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // protocol type
    arp[4] = 6; // hardware address length
    arp[5] = 4; // protocol address length
    arp[6..8].copy_from_slice(&ARP_REQUEST.to_be_bytes());
    arp[8..14].copy_from_slice(&source_mac.0);
    arp[14..18].copy_from_slice(&source_ip.0);
    // The target hardware address is zero: that is what is being asked for.
    arp[24..28].copy_from_slice(&target_ip.0);

    build_ethernet(BROADCAST_MAC, source_mac, ETHERTYPE_ARP, &arp, out)
}

/// Parse an ARP reply and return the sender's MAC and IP.
pub fn parse_arp_reply(frame: &[u8]) -> Option<(MacAddr, Ipv4Addr)> {
    let ethernet = parse_ethernet(frame)?;
    if ethernet.ethertype != ETHERTYPE_ARP || ethernet.payload.len() < 28 {
        return None;
    }
    let arp = ethernet.payload;
    if read_u16(arp, 6)? != ARP_REPLY {
        return None;
    }
    let mut mac = [0u8; 6];
    let mut ip = [0u8; 4];
    mac.copy_from_slice(&arp[8..14]);
    ip.copy_from_slice(&arp[14..18]);
    Some((MacAddr(mac), Ipv4Addr(ip)))
}

/// One parsed IPv4 packet.
#[derive(Clone, Copy, Debug)]
pub struct Ipv4Packet<'a> {
    /// Source address.
    pub source: Ipv4Addr,
    /// Destination address.
    pub destination: Ipv4Addr,
    /// Protocol number.
    pub protocol: u8,
    /// Payload after the header.
    pub payload: &'a [u8],
}

/// Build an IPv4 packet with a UDP payload, computing the header checksum.
pub fn build_ipv4(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    protocol: u8,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = IPV4_HEADER_BYTES + payload.len();
    if total > out.len() || total > 65535 {
        return None;
    }

    out[0] = 0x45; // version 4, header length 5 words
    out[1] = 0; // TOS
    out[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    out[4..6].copy_from_slice(&0u16.to_be_bytes()); // identification
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // flags and fragment offset
    out[8] = 64; // TTL
    out[9] = protocol;
    out[10..12].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[12..16].copy_from_slice(&source.0);
    out[16..20].copy_from_slice(&destination.0);
    let checksum = checksum(&out[0..IPV4_HEADER_BYTES]);
    out[10..12].copy_from_slice(&checksum.to_be_bytes());
    out[IPV4_HEADER_BYTES..total].copy_from_slice(payload);
    Some(total)
}

/// Parse an IPv4 packet, refusing fragments and options.
pub fn parse_ipv4(packet: &[u8]) -> Option<Ipv4Packet<'_>> {
    if packet.len() < IPV4_HEADER_BYTES {
        return None;
    }
    let version_and_length = packet[0];
    if version_and_length != 0x45 {
        // Only version 4 with a five-word header is implemented. An option
        // header or an IPv6 packet is refused rather than reinterpreted.
        return None;
    }
    let total = read_u16(packet, 2)? as usize;
    if total < IPV4_HEADER_BYTES || total > packet.len() {
        return None;
    }
    if read_u16(packet, 6)? & 0x1fff != 0 {
        return None;
    }
    if checksum(&packet[0..IPV4_HEADER_BYTES]) != 0 {
        return None;
    }

    let mut source = [0u8; 4];
    let mut destination = [0u8; 4];
    source.copy_from_slice(&packet[12..16]);
    destination.copy_from_slice(&packet[16..20]);
    Some(Ipv4Packet {
        source: Ipv4Addr(source),
        destination: Ipv4Addr(destination),
        protocol: packet[9],
        payload: &packet[IPV4_HEADER_BYTES..total],
    })
}

/// One parsed UDP datagram.
#[derive(Clone, Copy, Debug)]
pub struct UdpDatagram<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub destination_port: u16,
    /// The datagram's bytes.
    pub payload: &'a [u8],
}

/// Build a UDP datagram.
pub fn build_udp(
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = UDP_HEADER_BYTES + payload.len();
    if total > out.len() || total > 65535 {
        return None;
    }
    out[0..2].copy_from_slice(&source_port.to_be_bytes());
    out[2..4].copy_from_slice(&destination_port.to_be_bytes());
    out[4..6].copy_from_slice(&(total as u16).to_be_bytes());
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // checksum is optional in IPv4
    out[UDP_HEADER_BYTES..total].copy_from_slice(payload);
    Some(total)
}

/// Parse a UDP datagram.
pub fn parse_udp(datagram: &[u8]) -> Option<UdpDatagram<'_>> {
    if datagram.len() < UDP_HEADER_BYTES {
        return None;
    }
    let length = read_u16(datagram, 4)? as usize;
    if length < UDP_HEADER_BYTES || length > datagram.len() {
        return None;
    }
    Some(UdpDatagram {
        source_port: read_u16(datagram, 0)?,
        destination_port: read_u16(datagram, 2)?,
        payload: &datagram[UDP_HEADER_BYTES..length],
    })
}

/// Build a DNS query for `name`, asking for an A record.
///
/// `name` is a byte string such as `example.com`. Dots separate labels; an
/// empty label, a label longer than 63 bytes, or a total name longer than 255
/// bytes is refused. The caller's buffer must be large enough for the header,
/// the encoded name, and the four-byte type/class suffix.
pub fn build_dns_query(name: &[u8], id: u16, out: &mut [u8]) -> Option<usize> {
    if name.is_empty() || name.len() > 255 || out.len() < DNS_HEADER_BYTES + 5 {
        return None;
    }

    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // recursion desired
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // one question
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // no answers
    out[8..10].copy_from_slice(&0u16.to_be_bytes()); // no authority records
    out[10..12].copy_from_slice(&0u16.to_be_bytes()); // no additional records

    let mut position = DNS_HEADER_BYTES;
    let mut label_start = 0usize;
    for index in 0..name.len() {
        if name[index] == b'.' {
            let label_length = index - label_start;
            if label_length == 0 || label_length > 63 {
                return None;
            }
            if position + 1 + label_length > out.len() {
                return None;
            }
            out[position] = label_length as u8;
            position += 1;
            out[position..position + label_length].copy_from_slice(&name[label_start..index]);
            position += label_length;
            label_start = index + 1;
        }
    }

    let final_length = name.len() - label_start;
    if final_length == 0 || final_length > 63 || position + 1 + final_length > out.len() {
        return None;
    }
    out[position] = final_length as u8;
    position += 1;
    out[position..position + final_length].copy_from_slice(&name[label_start..]);
    position += final_length;

    // Root terminator, then QTYPE and QCLASS.
    if position + 5 > out.len() {
        return None;
    }
    out[position] = 0;
    position += 1;
    out[position..position + 2].copy_from_slice(&DNS_TYPE_A.to_be_bytes());
    position += 2;
    out[position..position + 2].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
    position += 2;
    Some(position)
}

/// Parse a DNS response and return the first A record's IPv4 address.
///
/// The parser walks the header, skips the echoed question, and reads answer
/// records. A record whose type is not A, whose class is not IN, or whose data
/// is not four bytes is skipped rather than treated as an address.
pub fn parse_dns_a_record(response: &[u8]) -> Option<Ipv4Addr> {
    if response.len() < DNS_HEADER_BYTES {
        return None;
    }
    let question_count = read_u16(response, 4)? as usize;
    let answer_count = read_u16(response, 6)? as usize;
    if question_count == 0 || answer_count == 0 {
        return None;
    }

    let mut position = DNS_HEADER_BYTES;
    for _ in 0..question_count {
        position = skip_dns_name(response, position)?;
        if position + 4 > response.len() {
            return None;
        }
        position += 4; // QTYPE and QCLASS
    }

    for _ in 0..answer_count {
        position = skip_dns_name(response, position)?;
        if position + 10 > response.len() {
            return None;
        }
        let record_type = read_u16(response, position)?;
        let record_class = read_u16(response, position + 2)?;
        let data_length = read_u16(response, position + 8)? as usize;
        position += 10;
        if position + data_length > response.len() {
            return None;
        }

        if record_type == DNS_TYPE_A && record_class == DNS_CLASS_IN && data_length == 4 {
            let mut address = [0u8; 4];
            address.copy_from_slice(&response[position..position + 4]);
            return Some(Ipv4Addr(address));
        }
        position += data_length;
    }

    None
}

/// Skip a DNS name at `position`, returning the offset after it.
fn skip_dns_name(data: &[u8], position: usize) -> Option<usize> {
    if position >= data.len() {
        return None;
    }
    let first = data[position];
    if first & 0xc0 == 0xc0 {
        // A compression pointer is two bytes and does not itself advance the
        // label stream; the parser only needs to skip it.
        if position + 2 > data.len() {
            return None;
        }
        return Some(position + 2);
    }

    let mut cursor = position;
    while cursor < data.len() {
        let label_length = data[cursor] as usize;
        if label_length == 0 {
            return Some(cursor + 1);
        }
        if label_length & 0xc0 != 0 {
            return None;
        }
        cursor += 1;
        if cursor + label_length > data.len() {
            return None;
        }
        cursor += label_length;
    }
    None
}

/// The one's-complement checksum of a byte region.
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut index = 0usize;
    while index + 1 < data.len() {
        sum += ((data[index] as u32) << 8) | (data[index + 1] as u32);
        index += 2;
    }
    if index < data.len() {
        sum += (data[index] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Read a big-endian `u16` at `offset`.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(((data[offset] as u16) << 8) | (data[offset + 1] as u16))
}

#[cfg(test)]
mod tests {
    use super::{
        ARP_REPLY, BROADCAST_MAC, DNS_CLASS_IN, DNS_TYPE_A, ETHERNET_HEADER_BYTES, ETHERTYPE_ARP,
        ETHERTYPE_IPV4, IPV4_HEADER_BYTES, Ipv4Addr, MacAddr, build_arp_request, build_dns_query,
        build_ethernet, build_ipv4, build_udp, parse_arp_reply, parse_dns_a_record, parse_ethernet,
        parse_ipv4, parse_udp,
    };

    fn ip(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr::new([a, b, c, d])
    }

    fn mac(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8) -> MacAddr {
        MacAddr::new([a, b, c, d, e, f])
    }

    #[test]
    fn ethernet_round_trips() {
        let source = mac(0x52, 0x54, 0x00, 0x12, 0x34, 0x56);
        let destination = mac(0x52, 0x54, 0x00, 0x12, 0x35, 0x00);
        let payload = [1u8, 2, 3, 4];
        let mut buffer = [0u8; 64];

        let length = build_ethernet(destination, source, ETHERTYPE_IPV4, &payload, &mut buffer)
            .expect("the frame fits");
        assert_eq!(length, ETHERNET_HEADER_BYTES + payload.len());

        let frame = parse_ethernet(&buffer[..length]).expect("the frame parses");
        assert_eq!(frame.destination, destination);
        assert_eq!(frame.source, source);
        assert_eq!(frame.ethertype, ETHERTYPE_IPV4);
        assert_eq!(frame.payload, &payload);
    }

    #[test]
    fn an_arp_request_is_broadcast_and_names_the_target() {
        let source = mac(0x52, 0x54, 0x00, 0x12, 0x34, 0x56);
        let source_ip = ip(10, 0, 2, 15);
        let target_ip = ip(10, 0, 2, 2);
        let mut buffer = [0u8; 64];

        let length =
            build_arp_request(source, source_ip, target_ip, &mut buffer).expect("the request fits");
        let frame = parse_ethernet(&buffer[..length]).expect("the request parses");
        assert_eq!(frame.destination, BROADCAST_MAC);
        assert_eq!(frame.ethertype, ETHERTYPE_ARP);
        assert_eq!(frame.payload[6..8], [0, 1], "the request opcode is one");
        assert_eq!(frame.payload[14..18], source_ip.octets());
        assert_eq!(frame.payload[24..28], target_ip.octets());
    }

    #[test]
    fn an_arp_reply_returns_the_senders_identity() {
        let target = mac(0x52, 0x54, 0x00, 0x12, 0x35, 0x00);
        let target_ip = ip(10, 0, 2, 2);
        let mut buffer = [0u8; 64];

        let mut arp = [0u8; 28];
        arp[0..2].copy_from_slice(&1u16.to_be_bytes());
        arp[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        arp[4] = 6;
        arp[5] = 4;
        arp[6..8].copy_from_slice(&ARP_REPLY.to_be_bytes());
        arp[8..14].copy_from_slice(&target.0);
        arp[14..18].copy_from_slice(&target_ip.0);

        let length = build_ethernet(BROADCAST_MAC, target, ETHERTYPE_ARP, &arp, &mut buffer)
            .expect("the reply fits");
        let (sender, sender_ip) = parse_arp_reply(&buffer[..length]).expect("the reply parses");
        assert_eq!(sender, target);
        assert_eq!(sender_ip, target_ip);
    }

    #[test]
    fn ipv4_headers_have_a_valid_checksum() {
        let source = ip(10, 0, 2, 15);
        let destination = ip(10, 0, 2, 3);
        let payload = [0xaa, 0xbb, 0xcc];
        let mut buffer = [0u8; 64];

        let length =
            build_ipv4(source, destination, 17, &payload, &mut buffer).expect("the packet fits");
        assert_eq!(length, IPV4_HEADER_BYTES + payload.len());

        let packet = parse_ipv4(&buffer[..length]).expect("the packet parses");
        assert_eq!(packet.source, source);
        assert_eq!(packet.destination, destination);
        assert_eq!(packet.protocol, 17);
        assert_eq!(packet.payload, &payload);
    }

    #[test]
    fn malformed_and_fragmented_ipv4_packets_are_refused() {
        let source = ip(10, 0, 2, 15);
        let destination = ip(10, 0, 2, 3);
        let mut buffer = [0u8; 64];
        let length =
            build_ipv4(source, destination, 17, &[1, 2], &mut buffer).expect("the packet fits");

        // Wrong version/header length.
        let mut bad = buffer;
        bad[0] = 0x46;
        assert!(parse_ipv4(&bad[..length]).is_none());

        // A fragment offset is refused rather than reassembled.
        let mut fragmented = buffer;
        fragmented[6] = 0;
        fragmented[7] = 1;
        assert!(parse_ipv4(&fragmented[..length]).is_none());

        // A corrupted checksum is refused.
        let mut corrupt = buffer;
        corrupt[10] ^= 0xff;
        assert!(parse_ipv4(&corrupt[..length]).is_none());
    }

    #[test]
    fn udp_round_trips_and_checks_its_length() {
        let payload = [0x01, 0x02, 0x03];
        let mut buffer = [0u8; 32];
        let length = build_udp(12345, 53, &payload, &mut buffer).expect("the datagram fits");
        let datagram = parse_udp(&buffer[..length]).expect("the datagram parses");
        assert_eq!(datagram.source_port, 12345);
        assert_eq!(datagram.destination_port, 53);
        assert_eq!(datagram.payload, &payload);

        let mut truncated = buffer;
        truncated[4] = 0;
        truncated[5] = 100;
        assert!(parse_udp(&truncated[..length]).is_none());
    }

    #[test]
    fn dns_query_encodes_labels_and_a_suffix() {
        let mut buffer = [0u8; 64];
        let length = build_dns_query(b"example.com", 0x1234, &mut buffer).expect("the query fits");
        assert_eq!(buffer[0..2], 0x1234u16.to_be_bytes());
        assert_eq!(buffer[2..4], 0x0100u16.to_be_bytes());
        assert_eq!(buffer[4..6], [0, 1]);
        assert_eq!(buffer[12], 7);
        assert_eq!(&buffer[13..20], b"example");
        assert_eq!(buffer[20], 3);
        assert_eq!(&buffer[21..24], b"com");
        assert_eq!(buffer[24], 0);
        assert_eq!(buffer[25..27], DNS_TYPE_A.to_be_bytes());
        assert_eq!(buffer[27..29], DNS_CLASS_IN.to_be_bytes());
        assert_eq!(length, 29);
    }

    #[test]
    fn empty_and_oversized_dns_labels_are_refused() {
        let mut buffer = [0u8; 64];
        assert_eq!(build_dns_query(b"", 1, &mut buffer), None);
        assert_eq!(build_dns_query(b"example..com", 1, &mut buffer), None);
        assert_eq!(build_dns_query(b".example.com", 1, &mut buffer), None);
    }

    #[test]
    fn a_dns_response_yields_the_first_a_record() {
        let mut response = [0u8; 64];
        response[0..2].copy_from_slice(&0x1234u16.to_be_bytes());
        response[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
        response[4..6].copy_from_slice(&1u16.to_be_bytes());
        response[6..8].copy_from_slice(&1u16.to_be_bytes());

        // Echoed question: example.com, type A, class IN.
        let mut position = 12;
        response[position] = 7;
        position += 1;
        response[position..position + 7].copy_from_slice(b"example");
        position += 7;
        response[position] = 3;
        position += 1;
        response[position..position + 3].copy_from_slice(b"com");
        position += 3;
        response[position] = 0;
        position += 1;
        response[position..position + 2].copy_from_slice(&DNS_TYPE_A.to_be_bytes());
        position += 2;
        response[position..position + 2].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
        position += 2;

        // Answer with a compression pointer, type A, class IN, four bytes.
        response[position] = 0xc0;
        response[position + 1] = 0x0c;
        position += 2;
        response[position..position + 2].copy_from_slice(&DNS_TYPE_A.to_be_bytes());
        position += 2;
        response[position..position + 2].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
        position += 2;
        response[position..position + 4].copy_from_slice(&30u32.to_be_bytes()); // TTL
        position += 4;
        response[position..position + 2].copy_from_slice(&4u16.to_be_bytes());
        position += 2;
        response[position..position + 4].copy_from_slice(&[93, 184, 216, 34]);
        position += 4;

        let address = parse_dns_a_record(&response[..position]).expect("an A record exists");
        assert_eq!(address, ip(93, 184, 216, 34));
    }

    #[test]
    fn non_a_dns_records_are_skipped() {
        let mut response = [0u8; 48];
        response[0..2].copy_from_slice(&1u16.to_be_bytes());
        response[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
        response[4..6].copy_from_slice(&1u16.to_be_bytes());
        response[6..8].copy_from_slice(&1u16.to_be_bytes());

        let mut position = 12;
        response[position] = 1;
        response[position + 1] = b'x';
        position += 2;
        response[position] = 0;
        position += 1;
        response[position..position + 2].copy_from_slice(&DNS_TYPE_A.to_be_bytes());
        position += 2;
        response[position..position + 2].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
        position += 2;

        response[position] = 0xc0;
        response[position + 1] = 0x0c;
        position += 2;
        response[position..position + 2].copy_from_slice(&16u16.to_be_bytes()); // CNAME
        position += 2;
        response[position..position + 2].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
        position += 2;
        response[position..position + 4].copy_from_slice(&0u32.to_be_bytes());
        position += 4;
        response[position..position + 2].copy_from_slice(&0u16.to_be_bytes());
        position += 2;

        assert_eq!(parse_dns_a_record(&response[..position]), None);
    }
}
