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
    let total = ETHERNET_HEADER_BYTES.checked_add(payload.len())?;
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
    let total = IPV4_HEADER_BYTES.checked_add(payload.len())?;
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
    let total = UDP_HEADER_BYTES.checked_add(payload.len())?;
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
            let label_length = index.checked_sub(label_start)?;
            if label_length == 0 || label_length > 63 {
                return None;
            }
            let label_end = position.checked_add(1)?.checked_add(label_length)?;
            if label_end > out.len() {
                return None;
            }
            out[position] = label_length as u8;
            position = position.checked_add(1)?;
            out[position..label_end].copy_from_slice(&name[label_start..index]);
            position = label_end;
            label_start = index.checked_add(1)?;
        }
    }

    let final_length = name.len().checked_sub(label_start)?;
    let final_end = position.checked_add(1)?.checked_add(final_length)?;
    if final_length == 0 || final_length > 63 || final_end > out.len() {
        return None;
    }
    out[position] = final_length as u8;
    position = position.checked_add(1)?;
    out[position..final_end].copy_from_slice(&name[label_start..]);
    position = final_end;

    // Root terminator, then QTYPE and QCLASS.
    let suffix_end = position.checked_add(1)?.checked_add(4)?;
    if suffix_end > out.len() {
        return None;
    }
    out[position] = 0;
    position = position.checked_add(1)?;
    out[position..position.checked_add(2)?].copy_from_slice(&DNS_TYPE_A.to_be_bytes());
    position = position.checked_add(2)?;
    out[position..position.checked_add(2)?].copy_from_slice(&DNS_CLASS_IN.to_be_bytes());
    position = position.checked_add(2)?;
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
        let question_end = position.checked_add(4)?;
        if question_end > response.len() {
            return None;
        }
        position = question_end; // QTYPE and QCLASS
    }

    for _ in 0..answer_count {
        position = skip_dns_name(response, position)?;
        let record_end = position.checked_add(10)?;
        if record_end > response.len() {
            return None;
        }
        let record_type = read_u16(response, position)?;
        let record_class = read_u16(response, position.checked_add(2)?)?;
        let data_length = read_u16(response, position.checked_add(8)?)? as usize;
        position = record_end;
        let data_end = position.checked_add(data_length)?;
        if data_end > response.len() {
            return None;
        }

        if record_type == DNS_TYPE_A && record_class == DNS_CLASS_IN && data_length == 4 {
            let mut address = [0u8; 4];
            address.copy_from_slice(&response[position..position.checked_add(4)?]);
            return Some(Ipv4Addr(address));
        }
        position = data_end;
    }

    None
}

/// IP protocol number for TCP.
pub const IP_PROTOCOL_TCP: u8 = 6;

/// IP protocol number for ICMP.
pub const IP_PROTOCOL_ICMP: u8 = 1;

/// Bytes in a TCP header without options.
pub const TCP_HEADER_BYTES: usize = 20;

/// Bytes in an ICMP echo header.
pub const ICMP_HEADER_BYTES: usize = 8;

/// TCP control flags. The flags field is six bits; the data offset is the high
/// nibble of the same 16-bit word.
pub const TCP_FIN: u16 = 0x01;
pub const TCP_SYN: u16 = 0x02;
pub const TCP_RST: u16 = 0x04;
pub const TCP_PSH: u16 = 0x08;
pub const TCP_ACK: u16 = 0x10;

/// DHCP wire constants.
pub const DHCP_MAGIC_COOKIE: u32 = 0x6382_5363;
pub const DHCP_CLIENT_PORT: u16 = 68;
pub const DHCP_SERVER_PORT: u16 = 67;

/// One parsed TCP segment.
#[derive(Clone, Copy, Debug)]
pub struct TcpSegment<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgement: u32,
    pub flags: u16,
    pub payload: &'a [u8],
}

impl TcpSegment<'_> {
    /// Whether the segment carries `flag`.
    pub const fn has(self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

/// Build a TCP segment into `out`.
///
/// The checksum covers the IPv4 pseudo-header (source, destination, protocol,
/// and segment length) plus the TCP header and payload. The caller supplies the
/// source and destination addresses because the pseudo-header is not part of
/// the segment itself.
///
/// The argument count is the header's own shape — two addresses, two ports,
/// two 32-bit sequence fields, flags, payload, buffer — and a builder cannot
/// group them without making the wire order less obvious, so the length lint
/// is allowed here.
#[allow(clippy::too_many_arguments)]
pub fn build_tcp(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = TCP_HEADER_BYTES.checked_add(payload.len())?;
    if total > out.len() || total > 65535 {
        return None;
    }

    out[0..2].copy_from_slice(&source_port.to_be_bytes());
    out[2..4].copy_from_slice(&destination_port.to_be_bytes());
    out[4..8].copy_from_slice(&sequence.to_be_bytes());
    out[8..12].copy_from_slice(&acknowledgement.to_be_bytes());
    out[12..14].copy_from_slice(&((5u16 << 12) | (flags & 0x3f)).to_be_bytes());
    out[14..16].copy_from_slice(&0xffffu16.to_be_bytes());
    out[16..18].copy_from_slice(&0u16.to_be_bytes());
    out[18..20].copy_from_slice(&0u16.to_be_bytes());
    out[TCP_HEADER_BYTES..total].copy_from_slice(payload);

    let checksum = pseudo_checksum(source, destination, IP_PROTOCOL_TCP, &out[..total]);
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    Some(total)
}

/// Parse a TCP segment and verify its pseudo-header checksum.
///
/// A segment is accepted with TCP options: the data offset in the header says
/// how many 32-bit words precede the payload. The checksum covers the options
/// too, so skipping them is a parse of the same bytes, not a truncation.
/// Options are not interpreted, only skipped. That matters because a SYN-ACK
/// from a real stack commonly carries an MSS option, and a parser that insists
/// on a bare 20-byte header would reject the reply without saying why.
pub fn parse_tcp(
    segment: &[u8],
    source: Ipv4Addr,
    destination: Ipv4Addr,
) -> Option<TcpSegment<'_>> {
    if segment.len() < TCP_HEADER_BYTES {
        return None;
    }
    let offset_and_flags = read_u16(segment, 12)?;
    let header_bytes = (offset_and_flags >> 12) as usize * 4;
    if header_bytes < TCP_HEADER_BYTES || header_bytes > segment.len() {
        return None;
    }
    if pseudo_checksum(source, destination, IP_PROTOCOL_TCP, segment) != 0 {
        return None;
    }

    Some(TcpSegment {
        source_port: read_u16(segment, 0)?,
        destination_port: read_u16(segment, 2)?,
        sequence: read_u32(segment, 4)?,
        acknowledgement: read_u32(segment, 8)?,
        flags: offset_and_flags & 0x3f,
        payload: &segment[header_bytes..],
    })
}

/// One parsed ICMP echo reply.
#[derive(Clone, Copy, Debug)]
pub struct IcmpEchoReply<'a> {
    pub identifier: u16,
    pub sequence: u16,
    pub payload: &'a [u8],
}

/// Build an ICMP echo request.
pub fn build_icmp_echo_request(
    identifier: u16,
    sequence: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let total = ICMP_HEADER_BYTES.checked_add(payload.len())?;
    if total > out.len() || total > 65535 {
        return None;
    }
    out[0] = 8; // echo request
    out[1] = 0;
    out[2..4].copy_from_slice(&0u16.to_be_bytes());
    out[4..6].copy_from_slice(&identifier.to_be_bytes());
    out[6..8].copy_from_slice(&sequence.to_be_bytes());
    out[ICMP_HEADER_BYTES..total].copy_from_slice(payload);
    let checksum = checksum(&out[..total]);
    out[2..4].copy_from_slice(&checksum.to_be_bytes());
    Some(total)
}

/// Parse an ICMP echo reply.
pub fn parse_icmp_echo_reply(packet: &[u8]) -> Option<IcmpEchoReply<'_>> {
    if packet.len() < ICMP_HEADER_BYTES {
        return None;
    }
    if packet[0] != 0 || packet[1] != 0 {
        return None;
    }
    if checksum(packet) != 0 {
        return None;
    }
    Some(IcmpEchoReply {
        identifier: read_u16(packet, 4)?,
        sequence: read_u16(packet, 6)?,
        payload: &packet[ICMP_HEADER_BYTES..],
    })
}

/// One parsed DHCP reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DhcpReply {
    pub address: Ipv4Addr,
    pub server: Option<Ipv4Addr>,
    pub message_type: u8,
}

/// Build a DHCP DISCOVER message.
pub fn build_dhcp_discover(mac: MacAddr, xid: u32, out: &mut [u8]) -> Option<usize> {
    build_dhcp_base(mac, xid, out)?;
    // The fixed header ends at 240; the message-type option and the end
    // marker bring the total to 244, which the length below guarantees.
    if out.len() < 244 {
        return None;
    }
    // Message type 53 = DISCOVER (1).
    out[240] = 53;
    out[241] = 1;
    out[242] = 1;
    out[243] = 255; // end
    Some(244)
}

/// Build a DHCP REQUEST message for `requested` and `server`.
pub fn build_dhcp_request(
    mac: MacAddr,
    xid: u32,
    requested: Ipv4Addr,
    server: Ipv4Addr,
    out: &mut [u8],
) -> Option<usize> {
    build_dhcp_base(mac, xid, out)?;
    // Header, message-type option, two four-byte options, end marker: 256.
    if out.len() < 256 {
        return None;
    }
    // Message type 53 = REQUEST (3).
    out[240] = 53;
    out[241] = 1;
    out[242] = 3;
    // Requested IP address, option 50.
    out[243] = 50;
    out[244] = 4;
    out[245..249].copy_from_slice(&requested.octets());
    // Server identifier, option 54.
    out[249] = 54;
    out[250] = 4;
    out[251..255].copy_from_slice(&server.octets());
    out[255] = 255;
    Some(256)
}

/// Build the fixed DHCP header and magic cookie.
fn build_dhcp_base(mac: MacAddr, xid: u32, out: &mut [u8]) -> Option<usize> {
    if out.len() < 240 {
        return None;
    }
    out[0] = 1; // BOOTREQUEST
    out[1] = 1; // Ethernet
    out[2] = 6; // hardware address length
    out[3] = 0;
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[8..10].copy_from_slice(&0u16.to_be_bytes());
    out[10..12].copy_from_slice(&0u16.to_be_bytes());
    out[12..16].copy_from_slice(&[0u8; 4]);
    out[16..20].copy_from_slice(&[0u8; 4]);
    out[20..24].copy_from_slice(&[0u8; 4]);
    out[24..28].copy_from_slice(&[0u8; 4]);
    out[28..34].copy_from_slice(&mac.octets());
    out[34..44].copy_from_slice(&[0u8; 10]);
    out[44..108].copy_from_slice(&[0u8; 64]);
    out[108..236].copy_from_slice(&[0u8; 128]);
    out[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE.to_be_bytes());
    Some(240)
}

/// Parse a DHCP OFFER or ACK.
pub fn parse_dhcp_reply(packet: &[u8]) -> Option<DhcpReply> {
    if packet.len() < 240 {
        return None;
    }
    let op = packet[0];
    if op != 2 && op != 5 {
        return None;
    }
    if packet[236] != 0x63 || packet[237] != 0x82 || packet[238] != 0x53 || packet[239] != 0x63 {
        return None;
    }

    let mut address = [0u8; 4];
    address.copy_from_slice(&packet[16..20]);
    let address = Ipv4Addr(address);
    if address.as_u32() == 0 {
        return None;
    }

    let mut message_type = 0u8;
    let mut server = None;
    let mut position = 240;
    while position < packet.len() {
        let code = packet[position];
        position = position.checked_add(1)?;
        if code == 255 {
            break;
        }
        if position >= packet.len() {
            return None;
        }
        let length = packet[position] as usize;
        position = position.checked_add(1)?;
        let option_end = position.checked_add(length)?;
        if option_end > packet.len() {
            return None;
        }
        if code == 53 && length == 1 {
            message_type = packet[position];
        } else if code == 54 && length == 4 {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&packet[position..position.checked_add(4)?]);
            server = Some(Ipv4Addr(bytes));
        }
        position = option_end;
    }

    if message_type == 0 {
        return None;
    }
    Some(DhcpReply {
        address,
        server,
        message_type,
    })
}

/// The one's-complement checksum of an IPv4 pseudo-header plus `data`.
///
/// The one's-complement sum is *defined* by end-around carry: words are added
/// with wraparound and the carry folded back at the end. `wrapping_add` here
/// is the protocol's arithmetic, not an overflow excuse.
fn pseudo_checksum(source: Ipv4Addr, destination: Ipv4Addr, protocol: u8, data: &[u8]) -> u16 {
    let word = |high: u8, low: u8| -> u32 { ((high as u32) << 8) | (low as u32) };
    let mut sum = 0u32;
    sum = sum.wrapping_add(word(source.0[0], source.0[1]));
    sum = sum.wrapping_add(word(source.0[2], source.0[3]));
    sum = sum.wrapping_add(word(destination.0[0], destination.0[1]));
    sum = sum.wrapping_add(word(destination.0[2], destination.0[3]));
    // The pseudo-header is: source, destination, a zero byte, the protocol,
    // and the segment length. The protocol is the low byte of a big-endian
    // 16-bit word, so the word is `0x0006`, not `0x0600`.
    sum = sum.wrapping_add(protocol as u32);
    sum = sum.wrapping_add(data.len() as u32);

    for pair in data.as_chunks::<2>().0 {
        sum = sum.wrapping_add(word(pair[0], pair[1]));
    }
    if let [last] = data.as_chunks::<2>().1 {
        sum = sum.wrapping_add((*last as u32) << 8);
    }
    fold_carry(sum)
}

/// Fold a wide one's-complement sum into 16 bits.
///
/// Each fold step adds the high half to the low half; the step itself can
/// carry again, so the loop runs until the carry is gone.
fn fold_carry(sum: u32) -> u16 {
    let mut sum = sum;
    while sum >> 16 != 0 {
        sum = (sum & 0xffff).wrapping_add(sum >> 16);
    }
    !(sum as u16)
}

/// Read a big-endian `u32` at `offset`.
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let window = data.get(offset..offset.checked_add(4)?)?;
    Some(
        ((window[0] as u32) << 24)
            | ((window[1] as u32) << 16)
            | ((window[2] as u32) << 8)
            | (window[3] as u32),
    )
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
        let after = position.checked_add(2)?;
        if after > data.len() {
            return None;
        }
        return Some(after);
    }

    let mut cursor = position;
    while cursor < data.len() {
        let label_length = data[cursor] as usize;
        if label_length == 0 {
            return cursor.checked_add(1);
        }
        if label_length & 0xc0 != 0 {
            return None;
        }
        cursor = cursor.checked_add(1)?;
        let label_end = cursor.checked_add(label_length)?;
        if label_end > data.len() {
            return None;
        }
        cursor = label_end;
    }
    None
}

/// The one's-complement checksum of a byte region.
///
/// See [`pseudo_checksum`] for why the additions wrap: that is the
/// definition of the checksum, not a tolerated overflow.
fn checksum(data: &[u8]) -> u16 {
    let word = |high: u8, low: u8| -> u32 { ((high as u32) << 8) | (low as u32) };
    let mut sum = 0u32;
    for pair in data.as_chunks::<2>().0 {
        sum = sum.wrapping_add(word(pair[0], pair[1]));
    }
    if let [last] = data.as_chunks::<2>().1 {
        sum = sum.wrapping_add((*last as u32) << 8);
    }
    fold_carry(sum)
}

/// Read a big-endian `u16` at `offset`.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let window = data.get(offset..offset.checked_add(2)?)?;
    Some(((window[0] as u16) << 8) | (window[1] as u16))
}

#[cfg(test)]
mod tests {
    use super::{
        ARP_REPLY, BROADCAST_MAC, DNS_CLASS_IN, DNS_TYPE_A, ETHERNET_HEADER_BYTES, ETHERTYPE_ARP,
        ETHERTYPE_IPV4, ICMP_HEADER_BYTES, IP_PROTOCOL_TCP, IPV4_HEADER_BYTES, Ipv4Addr, MacAddr,
        TCP_ACK, TCP_FIN, TCP_HEADER_BYTES, TCP_PSH, TCP_SYN, build_arp_request,
        build_dhcp_discover, build_dhcp_request, build_dns_query, build_ethernet,
        build_icmp_echo_request, build_ipv4, build_tcp, build_udp, parse_arp_reply,
        parse_dhcp_reply, parse_dns_a_record, parse_ethernet, parse_icmp_echo_reply, parse_ipv4,
        parse_tcp, parse_udp, pseudo_checksum,
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

    #[test]
    fn tcp_segment_round_trips_and_checks_its_pseudo_header() {
        let source = ip(10, 0, 2, 15);
        let destination = ip(93, 184, 216, 34);
        let payload = b"GET / HTTP/1.1\r\n";
        let mut buffer = [0u8; 128];

        let length = build_tcp(
            source,
            destination,
            49153,
            80,
            1,
            1,
            TCP_ACK | TCP_PSH,
            payload,
            &mut buffer,
        )
        .expect("the segment fits");
        assert_eq!(length, TCP_HEADER_BYTES + payload.len());
        // The pseudo-header's protocol word is `0x0006`, not `0x0600`.
        assert_eq!(((buffer[16] as u16) << 8) | buffer[17] as u16, 0xdbe8);

        let segment =
            parse_tcp(&buffer[..length], source, destination).expect("the segment parses");
        assert_eq!(segment.source_port, 49153);
        assert_eq!(segment.destination_port, 80);
        assert_eq!(segment.sequence, 1);
        assert_eq!(segment.acknowledgement, 1);
        assert!(segment.has(TCP_ACK));
        assert!(segment.has(TCP_PSH));
        assert_eq!(segment.payload, payload);

        let mut corrupt = buffer;
        corrupt[16] ^= 0xff;
        assert!(parse_tcp(&corrupt[..length], source, destination).is_none());
    }

    #[test]
    fn tcp_syn_ack_with_an_mss_option_is_accepted() {
        let source = ip(10, 0, 2, 100);
        let destination = ip(10, 0, 2, 15);
        let mut buffer = [0u8; 64];

        // A bare 20-byte header, then an MSS option (type 2, length 4, 1460).
        let length = build_tcp(
            source,
            destination,
            80,
            49153,
            0x2222_2222,
            0x1234_5679,
            TCP_SYN | TCP_ACK,
            &[],
            &mut buffer,
        )
        .expect("the bare segment fits");
        assert_eq!(length, TCP_HEADER_BYTES);

        buffer[TCP_HEADER_BYTES..TCP_HEADER_BYTES + 4].copy_from_slice(&[2, 4, 0x05, 0xb4]);
        let total = TCP_HEADER_BYTES + 4;
        buffer[12..14].copy_from_slice(&(((6u16 << 12) | (TCP_SYN | TCP_ACK)).to_be_bytes()));
        buffer[16..18].copy_from_slice(&0u16.to_be_bytes());
        let checksum = pseudo_checksum(source, destination, IP_PROTOCOL_TCP, &buffer[..total]);
        buffer[16..18].copy_from_slice(&checksum.to_be_bytes());

        let segment = parse_tcp(&buffer[..total], source, destination).expect("options parse");
        assert!(segment.has(TCP_SYN));
        assert!(segment.has(TCP_ACK));
        assert_eq!(segment.sequence, 0x2222_2222);
        assert_eq!(segment.acknowledgement, 0x1234_5679);
        assert!(segment.payload.is_empty());
    }

    #[test]
    fn tcp_syn_and_fin_flags_are_distinct() {
        let source = ip(10, 0, 2, 15);
        let destination = ip(10, 0, 2, 2);
        let mut buffer = [0u8; 64];
        let length = build_tcp(
            source,
            destination,
            49153,
            80,
            0,
            0,
            TCP_SYN,
            &[],
            &mut buffer,
        )
        .expect("the segment fits");
        let segment =
            parse_tcp(&buffer[..length], source, destination).expect("the segment parses");
        assert!(segment.has(TCP_SYN));
        assert!(!segment.has(TCP_ACK));

        let length = build_tcp(
            source,
            destination,
            49153,
            80,
            1,
            1,
            TCP_FIN,
            &[],
            &mut buffer,
        )
        .expect("the segment fits");
        let segment =
            parse_tcp(&buffer[..length], source, destination).expect("the segment parses");
        assert!(segment.has(TCP_FIN));
        assert!(!segment.has(TCP_PSH));
    }

    #[test]
    fn icmp_echo_request_and_reply_round_trip() {
        let payload = [1, 2, 3, 4];
        let mut buffer = [0u8; 64];
        let length =
            build_icmp_echo_request(0x1234, 1, &payload, &mut buffer).expect("the request fits");
        assert_eq!(length, ICMP_HEADER_BYTES + payload.len());
        assert_eq!(buffer[0], 8);

        // Turn the request into a reply by changing the type and recomputing.
        let mut reply = buffer;
        reply[0] = 0;
        reply[2..4].copy_from_slice(&0u16.to_be_bytes());
        let checksum = super::checksum(&reply[..length]);
        reply[2..4].copy_from_slice(&checksum.to_be_bytes());

        let parsed = parse_icmp_echo_reply(&reply[..length]).expect("the reply parses");
        assert_eq!(parsed.identifier, 0x1234);
        assert_eq!(parsed.sequence, 1);
        assert_eq!(parsed.payload, &payload);

        let mut corrupt = reply;
        corrupt[2] ^= 0xff;
        assert!(parse_icmp_echo_reply(&corrupt[..length]).is_none());
    }

    #[test]
    fn dhcp_discover_and_request_have_the_expected_options() {
        let mac = mac(0x52, 0x54, 0x00, 0x12, 0x34, 0x56);
        let mut buffer = [0u8; 512];

        let length = build_dhcp_discover(mac, 0x12345678, &mut buffer).expect("the discover fits");
        assert_eq!(length, 244);
        assert_eq!(buffer[0], 1);
        assert_eq!(buffer[1], 1);
        assert_eq!(buffer[2], 6);
        assert_eq!(buffer[4..8], 0x12345678u32.to_be_bytes());
        assert_eq!(buffer[28..34], mac.octets());
        assert_eq!(buffer[236], 0x63);
        assert_eq!(buffer[237], 0x82);
        assert_eq!(buffer[238], 0x53);
        assert_eq!(buffer[239], 0x63);
        assert_eq!(buffer[240], 53);
        assert_eq!(buffer[241], 1);
        assert_eq!(buffer[242], 1);

        let requested = ip(10, 0, 2, 15);
        let server = ip(10, 0, 2, 2);
        let length = build_dhcp_request(mac, 0x12345678, requested, server, &mut buffer)
            .expect("the request fits");
        assert_eq!(buffer[240], 53);
        assert_eq!(buffer[242], 3);
        assert_eq!(buffer[243], 50);
        assert_eq!(buffer[244], 4);
        assert_eq!(buffer[245..249], requested.octets());
        assert_eq!(buffer[249], 54);
        assert_eq!(buffer[250], 4);
        assert_eq!(buffer[251..255], server.octets());
        assert_eq!(buffer[255], 255);
        assert_eq!(length, 256);
    }

    #[test]
    fn dhcp_offer_is_parsed() {
        let mac = mac(0x52, 0x54, 0x00, 0x12, 0x34, 0x56);
        let mut buffer = [0u8; 512];
        let length = build_dhcp_discover(mac, 1, &mut buffer).expect("the discover fits");

        // Make a synthetic OFFER: op 2, yiaddr 10.0.2.15, type 2, server 10.0.2.2.
        let mut offer = buffer;
        offer[0] = 2;
        offer[16..20].copy_from_slice(&ip(10, 0, 2, 15).octets());
        offer[240] = 53;
        offer[241] = 1;
        offer[242] = 2;
        offer[243] = 54;
        offer[244] = 4;
        offer[245..249].copy_from_slice(&ip(10, 0, 2, 2).octets());
        offer[249] = 255;

        let reply = parse_dhcp_reply(&offer[..250]).expect("the offer parses");
        assert_eq!(reply.address, ip(10, 0, 2, 15));
        assert_eq!(reply.server, Some(ip(10, 0, 2, 2)));
        assert_eq!(reply.message_type, 2);

        assert_eq!(
            parse_dhcp_reply(&offer[..length]),
            None,
            "a DISCOVER is not a reply"
        );
    }
}
