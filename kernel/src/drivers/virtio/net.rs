//! The VirtIO network device: two queues, one receive buffer, one transmit
//! buffer, and a small DNS path.
//!
//! # Why two queues
//!
//! VirtIO net has a receive queue and a transmit queue. The block driver's
//! one-queue model does not fit: a DNS lookup has to send an ARP request, wait
//! for the reply, send a UDP query, and wait for the response, and the send and
//! receive directions must be independent. This driver uses two `VirtQueue`s,
//! each with its own notify offset.
//!
//! # Synchronous by construction
//!
//! Like the block driver, each queue carries at most one request at a time.
//! That makes the timeout path decidable: one descriptor chain to unwind, one
//! frame to keep, and one device reset to perform. The network service is not a
//! full socket stack; it is a bounded request/reply path that proves the NIC
//! can reach the QEMU user network.
//!
//! # The QEMU user-network contract
//!
//! The guest address, gateway, and DNS server are the fixed values QEMU's
//! user-mode networking assigns (`10.0.2.15`, `10.0.2.2`, `10.0.2.3`). The driver
//! reads the NIC's MAC from the device configuration structure rather than
//! guessing it. A future DHCP client would replace these constants; this driver
//! does not claim DHCP, IPv6, TCP, or a general socket API.
//!
//! # Device input is not trusted
//!
//! The device can answer late, answer with a forged descriptor head, or write a
//! byte count that does not describe the request. Every completion is checked
//! before any received byte is copied into the caller's buffer.

use super::pci as transport;
use super::queue::{self, VirtQueue};
use super::{FEATURE_VERSION_1, VirtioError};
use crate::arch::ADDRESS_SPACE;
use crate::drivers::mmio;
use crate::drivers::pci;
use crate::klog;
use crate::logger::Level;
use crate::mm::{direct_map, frame_allocator};
use nova_net::{
    BROADCAST_MAC, ETHERTYPE_IPV4, IP_PROTOCOL_UDP, Ipv4Addr, MacAddr, build_arp_request,
    build_dns_query, build_ethernet, build_ipv4, build_udp, parse_arp_reply, parse_dns_a_record,
    parse_ethernet, parse_ipv4, parse_udp,
};

/// `VIRTIO_NET_F_MAC`: the device configuration structure carries a MAC.
pub const FEATURE_NET_MAC: u32 = 5;

/// `VIRTIO_NET_F_STATUS`: the device configuration structure carries a link
/// status word.
pub const FEATURE_NET_STATUS: u32 = 16;

/// Bytes in the VirtIO 1.0 net header.
///
/// The driver negotiates `VIRTIO_F_VERSION_1`, whose header is
/// `virtio_net_hdr_v1`: 12 bytes, ending with the `num_buffers` word. The legacy
/// header is 10 bytes and the merged-receive header is also 12; using the wrong
/// length makes the driver strip bytes from the Ethernet frame rather than the
/// header, which turns every send into a malformed frame.
pub const NET_HEADER_BYTES: usize = 12;

/// The largest Ethernet frame this driver stages.
///
/// 2048 is bigger than an Ethernet frame's 1514-byte maximum and leaves room for
/// the header. The network service's payload budget is smaller, but the driver
/// is the place that must reject a frame that would not fit one descriptor.
pub const MAX_FRAME_BYTES: usize = 2048;

/// Guest address assigned by QEMU user networking.
pub const LINK_ADDRESS: Ipv4Addr = Ipv4Addr::new([10, 0, 2, 15]);

/// Gateway address assigned by QEMU user networking.
pub const LINK_GATEWAY: Ipv4Addr = Ipv4Addr::new([10, 0, 2, 2]);

/// DNS server address assigned by QEMU user networking.
pub const DNS_SERVER: Ipv4Addr = Ipv4Addr::new([10, 0, 2, 3]);

/// Source port this driver uses for its DNS query.
pub const DNS_SOURCE_PORT: u16 = 49152;

/// How many poll iterations one request may wait.
///
/// An iteration bound rather than a wall-clock deadline, for the same reason as
/// the block driver's budget: under emulation the instruction stream and the
/// machine clock are not the same measure.
const POLL_BUDGET: u64 = 100_000_000;

/// How many received packets a DNS lookup or ARP lookup will inspect.
const LOOKUP_ATTEMPTS: usize = 8;

/// Why a network request failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetError {
    /// The PCI transport would not come up.
    Transport(VirtioError),
    /// A virtqueue operation was refused.
    Queue(queue::QueueError),
    /// The device did not answer within the driver's budget.
    Timeout,
    /// The device's completion did not describe this request.
    BadResponse,
    /// The device does not advertise a MAC address.
    NoMac,
    /// The device has fewer than the receive and transmit queues.
    NoQueues,
    /// No network device is live.
    NoDevice,
    /// The frame does not fit the driver's staging bound.
    FrameTooLong,
    /// The caller's buffer is too small.
    BufferSmall,
    /// No ARP reply was found for the destination.
    ArpFailed,
    /// No DNS answer was found.
    DnsFailed,
}

impl NetError {
    /// A stable name, for logs and test failures.
    pub const fn name(self) -> &'static str {
        match self {
            NetError::Transport(error) => error.name(),
            NetError::Queue(_) => "N_QUEUE",
            NetError::Timeout => "N_TIMEOUT",
            NetError::BadResponse => "N_BAD_RESPONSE",
            NetError::NoMac => "N_NO_MAC",
            NetError::NoQueues => "N_NO_QUEUES",
            NetError::NoDevice => "N_NO_DEVICE",
            NetError::FrameTooLong => "N_FRAME_TOO_LONG",
            NetError::BufferSmall => "N_BUFFER_SMALL",
            NetError::ArpFailed => "N_ARP_FAILED",
            NetError::DnsFailed => "N_DNS_FAILED",
        }
    }
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetError::Transport(error) => write!(f, "transport: {error:?}"),
            NetError::Queue(error) => write!(f, "queue: {error:?}"),
            other => f.write_str(other.name()),
        }
    }
}

/// The link state a service can report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LinkInfo {
    /// The NIC's Ethernet address.
    pub mac: MacAddr,
    /// The guest IPv4 address.
    pub address: Ipv4Addr,
    /// The gateway IPv4 address.
    pub gateway: Ipv4Addr,
    /// Whether the device reports the link as up.
    pub link_up: bool,
}

/// One VirtIO network device.
pub struct VirtioNet {
    transport: transport::Transport,
    rx: VirtQueue,
    tx: VirtQueue,
    rx_frame: Option<usize>,
    tx_frame: Option<usize>,
    mac: MacAddr,
    link_up: bool,
}

impl VirtioNet {
    /// Take over a network device: negotiate, allocate both queues, and go live.
    pub fn open(device: &pci::Device) -> Result<Self, NetError> {
        let mut transport = transport::Transport::open(device).map_err(NetError::Transport)?;

        let wanted = super::Features {
            low: (1 << FEATURE_NET_MAC) | (1 << FEATURE_NET_STATUS),
            high: 1 << FEATURE_VERSION_1,
        };
        transport
            .start_device(wanted)
            .map_err(NetError::Transport)?;

        let offered = transport.device_features();
        if !offered.low_has(FEATURE_NET_MAC) {
            transport.reset();
            return Err(NetError::NoMac);
        }

        let queues = transport.num_queues();
        if queues < 2 {
            transport.reset();
            return Err(NetError::NoQueues);
        }

        let mac = read_mac(&transport).map_err(|_| NetError::NoMac)?;

        let mut rx = VirtQueue::allocate(queue::MAX_QUEUE_SIZE).map_err(NetError::Queue)?;
        let mut tx = VirtQueue::allocate(queue::MAX_QUEUE_SIZE).map_err(NetError::Queue)?;

        if let Err(error) = transport.setup_queue(
            0,
            queue::MAX_QUEUE_SIZE,
            rx.desc_phys(),
            rx.avail_phys(),
            rx.used_phys(),
        ) {
            // SAFETY: neither queue has been enabled, so the device has no
            // reference to these frames.
            unsafe { rx.release() };
            unsafe { tx.release() };
            transport.reset();
            return Err(NetError::Transport(error));
        }

        if let Err(error) = transport.setup_queue(
            1,
            queue::MAX_QUEUE_SIZE,
            tx.desc_phys(),
            tx.avail_phys(),
            tx.used_phys(),
        ) {
            // SAFETY: as above.
            unsafe { rx.release() };
            unsafe { tx.release() };
            transport.reset();
            return Err(NetError::Transport(error));
        }

        transport.driver_ok();

        let rx_frame = alloc_frame()?;
        let tx_frame = alloc_frame().map_err(|_| {
            free_frame(rx_frame);
            NetError::Queue(queue::QueueError::OutOfMemory)
        })?;

        let link_up = if offered.low_has(FEATURE_NET_STATUS) {
            // SAFETY: the device configuration structure was mapped with room
            // for the status field, and only this driver reads it.
            let config = transport.device_config().ok_or(NetError::NoMac)?;
            unsafe { mmio::read_u16(config, 6) & 1 != 0 }
        } else {
            // Without the status feature the device makes no link claim. This
            // driver therefore does not claim one either.
            false
        };

        Ok(Self {
            transport,
            rx,
            tx,
            rx_frame: Some(rx_frame),
            tx_frame: Some(tx_frame),
            mac,
            link_up,
        })
    }

    /// The NIC's MAC and the link state.
    pub fn link_info(&self) -> LinkInfo {
        LinkInfo {
            mac: self.mac,
            address: LINK_ADDRESS,
            gateway: LINK_GATEWAY,
            link_up: self.link_up,
        }
    }

    /// Whether a request is waiting on either queue.
    pub fn request_in_flight(&self) -> bool {
        self.rx.in_flight() || self.tx.in_flight()
    }

    /// Send one Ethernet frame.
    pub fn send_frame(&mut self, frame: &[u8]) -> Result<(), NetError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(NetError::FrameTooLong);
        }
        let Some(frame_index) = self.tx_frame else {
            return Err(NetError::BadResponse);
        };

        let base = frame_phys(frame_index);
        let total = NET_HEADER_BYTES + frame.len();
        if total > ADDRESS_SPACE.page_size as usize {
            return Err(NetError::FrameTooLong);
        }

        // SAFETY: the transmit frame is one page owned by this driver, the
        // header is the first eight bytes, and the frame is shorter than the
        // remaining space.
        unsafe {
            let buffer = direct_map::phys_to_virt(base);
            core::ptr::write_bytes(buffer, 0, NET_HEADER_BYTES);
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                buffer.add(NET_HEADER_BYTES),
                frame.len(),
            );
        }
        mmio::fence();

        // SAFETY: the frame is kernel-owned, outlives the request, and the
        // queue is checked for an in-flight request by `publish_one`.
        unsafe {
            self.tx
                .publish_one(base, total as u32, false)
                .map_err(NetError::Queue)?;
        }
        self.transport.notify_queue(1);

        let completion = self.await_completion(true)?;
        if completion.head != 0 || completion.written != 0 {
            self.unwind_after_failure();
            return Err(NetError::BadResponse);
        }
        Ok(())
    }

    /// Publish the receive buffer before the device can answer a request.
    ///
    /// VirtIO net drops a packet when the receive queue has no buffer. Posting
    /// the buffer only after the datagram is sent is therefore a race: the
    /// emulated backend answers synchronously and the answer arrives before
    /// the buffer exists. This method separates “make the buffer available”
    /// from “wait for the device to fill it” so every send path primes first.
    fn prime_receive(&mut self) -> Result<(), NetError> {
        let Some(frame_index) = self.rx_frame else {
            return Err(NetError::BadResponse);
        };
        let base = frame_phys(frame_index);
        let capacity = NET_HEADER_BYTES + MAX_FRAME_BYTES;
        if capacity > ADDRESS_SPACE.page_size as usize {
            return Err(NetError::BufferSmall);
        }

        // SAFETY: the receive frame is one page owned by this driver, and the
        // capacity is checked above.
        unsafe {
            let buffer = direct_map::phys_to_virt(base);
            core::ptr::write_bytes(buffer, 0, capacity);
        }
        mmio::fence();

        // SAFETY: the device writes the header and the frame into this buffer;
        // the descriptor is the whole buffer, and the queue is bounded.
        unsafe {
            self.rx
                .publish_one(base, capacity as u32, true)
                .map_err(NetError::Queue)?;
        }
        self.transport.notify_queue(0);
        Ok(())
    }

    /// Wait for one receive completion and copy the frame into `out`.
    fn poll_receive(&mut self, out: &mut [u8]) -> Result<usize, NetError> {
        let Some(frame_index) = self.rx_frame else {
            return Err(NetError::BadResponse);
        };
        let base = frame_phys(frame_index);
        let capacity = NET_HEADER_BYTES + MAX_FRAME_BYTES;

        let completion = self.await_completion(false)?;
        if completion.head != 0
            || completion.written < NET_HEADER_BYTES as u32
            || completion.written > capacity as u32
        {
            self.unwind_after_failure();
            return Err(NetError::BadResponse);
        }

        let written = completion.written as usize;
        let frame_len = written - NET_HEADER_BYTES;
        if frame_len > out.len() {
            self.unwind_after_failure();
            return Err(NetError::BufferSmall);
        }

        // SAFETY: `written` is bounded by the frame capacity, and the source is
        // the same driver-owned frame that was just published.
        unsafe {
            let buffer = direct_map::phys_to_virt(base).add(NET_HEADER_BYTES);
            core::ptr::copy_nonoverlapping(buffer, out.as_mut_ptr(), frame_len);
        }
        Ok(frame_len)
    }

    /// Receive one Ethernet frame into `out`, excluding the VirtIO header.
    pub fn receive_frame(&mut self, out: &mut [u8]) -> Result<usize, NetError> {
        self.prime_receive()?;
        self.poll_receive(out)
    }
    /// Resolve a destination IP to its Ethernet address with one ARP request.
    fn resolve_arp(&mut self, target: Ipv4Addr) -> Result<MacAddr, NetError> {
        let mut request = [0u8; MAX_FRAME_BYTES];
        let length = build_arp_request(self.mac, LINK_ADDRESS, target, &mut request)
            .ok_or(NetError::FrameTooLong)?;

        // The buffer must be in the receive queue before the request is sent:
        // slirp answers an ARP request synchronously, and a reply with no
        // available buffer is dropped.
        self.prime_receive()?;
        self.send_frame(&request[..length])?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let length = self.poll_receive(&mut buffer)?;
            if let Some((sender, sender_ip)) = parse_arp_reply(&buffer[..length]) {
                if sender_ip == target && sender != BROADCAST_MAC {
                    klog!(
                        Level::Info,
                        "virtio-net: arp {:#x} resolved to {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        target.as_u32(),
                        sender.0[0],
                        sender.0[1],
                        sender.0[2],
                        sender.0[3],
                        sender.0[4],
                        sender.0[5],
                    );
                    return Ok(sender);
                }
            }
            if attempt + 1 < LOOKUP_ATTEMPTS {
                self.prime_receive()?;
            }
        }
        Err(NetError::ArpFailed)
    }

    /// Send one UDP datagram. The destination's MAC is resolved first.
    pub fn send_udp(
        &mut self,
        destination: Ipv4Addr,
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), NetError> {
        let destination_mac = self.resolve_arp(destination)?;

        let mut udp = [0u8; MAX_FRAME_BYTES];
        let udp_length = build_udp(DNS_SOURCE_PORT, destination_port, payload, &mut udp)
            .ok_or(NetError::FrameTooLong)?;

        let mut packet = [0u8; MAX_FRAME_BYTES];
        let packet_length = build_ipv4(
            LINK_ADDRESS,
            destination,
            IP_PROTOCOL_UDP,
            &udp[..udp_length],
            &mut packet,
        )
        .ok_or(NetError::FrameTooLong)?;

        let mut frame = [0u8; MAX_FRAME_BYTES];
        let frame_length = build_ethernet(
            destination_mac,
            self.mac,
            ETHERTYPE_IPV4,
            &packet[..packet_length],
            &mut frame,
        )
        .ok_or(NetError::FrameTooLong)?;

        self.send_frame(&frame[..frame_length])
    }

    /// Receive one UDP datagram whose source port is `port`.
    pub fn receive_udp(&mut self, port: u16, out: &mut [u8]) -> Result<usize, NetError> {
        let mut frame = [0u8; MAX_FRAME_BYTES];
        for _ in 0..LOOKUP_ATTEMPTS {
            let frame_length = self.receive_frame(&mut frame)?;
            let Some(ethernet) = parse_ethernet(&frame[..frame_length]) else {
                continue;
            };
            if ethernet.ethertype != ETHERTYPE_IPV4 {
                continue;
            }
            let Some(packet) = parse_ipv4(ethernet.payload) else {
                continue;
            };
            if packet.protocol != IP_PROTOCOL_UDP {
                continue;
            }
            let Some(datagram) = parse_udp(packet.payload) else {
                continue;
            };
            if datagram.source_port != port {
                continue;
            }
            if datagram.payload.len() > out.len() {
                return Err(NetError::BufferSmall);
            }
            out[..datagram.payload.len()].copy_from_slice(datagram.payload);
            return Ok(datagram.payload.len());
        }
        Err(NetError::Timeout)
    }

    /// Resolve a DNS name to one IPv4 address.
    pub fn resolve_dns(&mut self, name: &[u8]) -> Result<Ipv4Addr, NetError> {
        // QEMU's user-mode network answers ARP for its virtual DNS server,
        // so the DNS packet can be addressed to the server's own MAC.
        let destination_mac = self.resolve_arp(DNS_SERVER)?;

        let mut query = [0u8; 512];
        let query_length =
            build_dns_query(name, 0x1234, &mut query).ok_or(NetError::FrameTooLong)?;

        let mut udp = [0u8; MAX_FRAME_BYTES];
        let udp_length = build_udp(DNS_SOURCE_PORT, 53, &query[..query_length], &mut udp)
            .ok_or(NetError::FrameTooLong)?;

        let mut packet = [0u8; MAX_FRAME_BYTES];
        let packet_length = build_ipv4(
            LINK_ADDRESS,
            DNS_SERVER,
            IP_PROTOCOL_UDP,
            &udp[..udp_length],
            &mut packet,
        )
        .ok_or(NetError::FrameTooLong)?;

        let mut frame = [0u8; MAX_FRAME_BYTES];
        let frame_length = build_ethernet(
            destination_mac,
            self.mac,
            ETHERTYPE_IPV4,
            &packet[..packet_length],
            &mut frame,
        )
        .ok_or(NetError::FrameTooLong)?;

        // Prime the receive queue before sending: slirp answers synchronously,
        // and a reply that arrives with no available buffer is dropped.
        self.prime_receive()?;
        self.send_frame(&frame[..frame_length])?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let frame_length = self.poll_receive(&mut buffer)?;
            let Some(ethernet) = parse_ethernet(&buffer[..frame_length]) else {
                continue;
            };
            if ethernet.ethertype != ETHERTYPE_IPV4 {
                continue;
            }
            let Some(packet) = parse_ipv4(ethernet.payload) else {
                continue;
            };
            if packet.protocol != IP_PROTOCOL_UDP {
                continue;
            }
            let Some(datagram) = parse_udp(packet.payload) else {
                continue;
            };
            if datagram.source_port != 53 || datagram.destination_port != DNS_SOURCE_PORT {
                continue;
            }
            if let Some(address) = parse_dns_a_record(datagram.payload) {
                return Ok(address);
            }
            if attempt + 1 < LOOKUP_ATTEMPTS {
                self.prime_receive()?;
            }
        }
        Err(NetError::DnsFailed)
    }

    /// Stop the device and give every frame and ring back.
    pub fn shutdown(&mut self) {
        if !self.transport.reset() {
            super::klog_reset_refused();
        }
        // SAFETY: the device is stopped, so the rings are the driver's alone.
        unsafe {
            self.rx.rewind();
            self.tx.rewind();
            self.rx.release();
            self.tx.release();
        }

        if let Some(frame) = self.rx_frame.take() {
            free_frame(frame);
        }
        if let Some(frame) = self.tx_frame.take() {
            free_frame(frame);
        }
    }

    /// Poll one queue until the device answers, the budget runs out, or the
    /// device reports a completion this driver cannot accept.
    fn await_completion(&mut self, transmit: bool) -> Result<queue::Completion, NetError> {
        let mut budget = POLL_BUDGET;
        loop {
            let result = if transmit {
                self.tx.take_completion()
            } else {
                self.rx.take_completion()
            };
            match result {
                Ok(Some(completion)) => return Ok(completion),
                Ok(None) => {}
                Err(error) => return Err(NetError::Queue(error)),
            }

            if budget == 0 {
                let (avail, used) = if transmit {
                    self.tx.ring_indices()
                } else {
                    self.rx.ring_indices()
                };
                klog!(
                    Level::Warn,
                    "virtio-net: timeout transmit={} avail {} used {} status {:#x} isr {:#x}",
                    transmit,
                    avail,
                    used,
                    self.transport.status(),
                    self.transport.interrupt_status()
                );
                return Err(NetError::Timeout);
            }
            budget -= 1;
            core::hint::spin_loop();
        }
    }

    /// Reset and reinitialize after a failed request.
    fn unwind_after_failure(&mut self) {
        if !self.transport.reset() {
            super::klog_reset_refused();
        }
        // SAFETY: the device is stopped, so the rings are the driver's alone.
        unsafe {
            self.rx.rewind();
            self.tx.rewind();
        }

        let wanted = super::Features {
            low: (1 << FEATURE_NET_MAC) | (1 << FEATURE_NET_STATUS),
            high: 1 << FEATURE_VERSION_1,
        };
        if self.transport.start_device(wanted).is_err() {
            return;
        }
        let _ = self.transport.setup_queue(
            0,
            self.rx.size(),
            self.rx.desc_phys(),
            self.rx.avail_phys(),
            self.rx.used_phys(),
        );
        let _ = self.transport.setup_queue(
            1,
            self.tx.size(),
            self.tx.desc_phys(),
            self.tx.avail_phys(),
            self.tx.used_phys(),
        );
        self.transport.driver_ok();
    }
}

/// Read the NIC's MAC from the device configuration structure.
fn read_mac(transport: &transport::Transport) -> Result<MacAddr, NetError> {
    let config = transport.device_config().ok_or(NetError::NoMac)?;
    let mut octets = [0u8; 6];
    // SAFETY: the configuration structure was mapped with room for the MAC,
    // and only this driver reads it.
    unsafe {
        for (index, octet) in octets.iter_mut().enumerate() {
            *octet = mmio::read_u8(config, index);
        }
    }
    Ok(MacAddr(octets))
}

/// Allocate one zeroed page for a network buffer.
fn alloc_frame() -> Result<usize, NetError> {
    // SAFETY: `mm::init` published the allocator during boot.
    let frame = unsafe { frame_allocator().alloc() }
        .map_err(|_| NetError::Queue(queue::QueueError::OutOfMemory))?;

    // SAFETY: the frame was just allocated, so it is this caller's; the direct
    // map covers it; the length is one page.
    unsafe {
        core::ptr::write_bytes(
            direct_map::phys_to_virt(frame_phys(frame)),
            0,
            ADDRESS_SPACE.page_size as usize,
        );
    }

    Ok(frame)
}

/// Give one network buffer frame back.
fn free_frame(frame: usize) {
    // SAFETY: the frame came from `alloc_frame` and the caller owns it.
    unsafe { frame_allocator().dealloc(frame) };
}

/// The physical address of a frame.
const fn frame_phys(frame: usize) -> u64 {
    frame as u64 * ADDRESS_SPACE.page_size
}
