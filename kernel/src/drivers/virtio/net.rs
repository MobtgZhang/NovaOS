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
use core::sync::atomic::{AtomicU64, Ordering};

use crate::klog;
use crate::logger::Level;
use crate::mm::{direct_map, frame_allocator};
use nova_net::{
    BROADCAST_MAC, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpReply, ETHERTYPE_IPV4, IP_PROTOCOL_ICMP,
    IP_PROTOCOL_TCP, IP_PROTOCOL_UDP, Ipv4Addr, MacAddr, TCP_ACK, TCP_FIN, TCP_PSH, TCP_SYN,
    TcpSegment, build_arp_request, build_dhcp_discover, build_dhcp_request, build_dns_query,
    build_ethernet, build_icmp_echo_request, build_ipv4, build_tcp, build_udp, parse_arp_reply,
    parse_dhcp_reply, parse_dns_a_record, parse_ethernet, parse_icmp_echo_reply, parse_ipv4,
    parse_tcp, parse_udp,
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

/// Source port this driver uses for TCP clients.
pub const TCP_SOURCE_PORT: u16 = 49153;

/// Initial sequence number this driver uses for TCP clients.
///
/// A nonzero sequence avoids the degenerate `seq=0` state some emulated TCP
/// stacks treat specially; the handshake still checks `ack = sequence + 1`.
pub const TCP_INITIAL_SEQUENCE: u32 = 0x1234_5678;

/// A live TCP connection's sequence state.
#[derive(Clone, Copy, Debug)]
pub struct TcpConnection {
    /// The remote address.
    pub remote: Ipv4Addr,
    /// The remote Ethernet address.
    pub remote_mac: MacAddr,
    /// The local port.
    pub local_port: u16,
    /// The remote port.
    pub remote_port: u16,
    /// The next sequence number this side will send.
    pub local_seq: u32,
    /// The next sequence number this side expects from the peer.
    pub remote_seq: u32,
}

/// How many poll iterations one request may wait.
///
/// With MSI-X armed this is only a ceiling against a device that stopped
/// interrupting: the wait ends on the first interrupt, not on the budget, and
/// the spin between interrupts reads one local flag rather than the device's
/// mapped ring. Without MSI-X it remains the plain poll bound it always was.
///
/// An iteration bound rather than a wall-clock deadline, for the same reason as
/// the block driver's budget: under emulation the instruction stream and the
/// machine clock are not the same measure.
const POLL_BUDGET: u64 = 100_000_000;

/// How many iterations to keep draining *after* an interrupt arrived.
///
/// The interrupt says a completion landed, not that the waiter's first read
/// saw it: the ring's used index and the flag can race by one read. A short
/// bounded window of ordinary queue reads after the flag covers that race
/// without ever re-entering the long pre-interrupt wait.
const IRQ_DRAIN_BUDGET: u64 = 1_000_000;

/// The vector this device raises interrupts on.
///
/// Allocated from [`crate::arch::x86_64::intr::DEVICE_VECTOR_BASE`]: the PIC's
/// range is masked, the APIC timer owns 0x30, so 0x31 is the first device
/// vector. The IDT installs this module's handler at exactly this gate.
pub const IRQ_VECTOR: u32 = crate::arch::x86_64::intr::DEVICE_VECTOR_BASE;

/// How many device interrupts have been acknowledged since boot.
///
/// The count, not a boolean, is the wakeup contract: a completion can land
/// between the waiter's queue read and its flag read, and a boolean set before
/// the wait even starts would be indistinguishable from one set after. The
/// waiter compares against the count it observed when it began waiting.
static DEVICE_IRQS: AtomicU64 = AtomicU64::new(0);

/// Record one acknowledged device interrupt. Called from the interrupt
/// handler on whichever CPU the message was delivered to.
pub fn note_device_interrupt() {
    DEVICE_IRQS.fetch_add(1, Ordering::Release);
}

/// The current device-interrupt count, for diagnostics and tests.
pub fn device_irq_count() -> u64 {
    DEVICE_IRQS.load(Ordering::Acquire)
}

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
    /// No TCP handshake or segment was found.
    TcpFailed,
    /// No ICMP echo reply was found.
    IcmpFailed,
    /// No DHCP offer or acknowledgement was found.
    DhcpFailed,
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
            NetError::TcpFailed => "N_TCP_FAILED",
            NetError::IcmpFailed => "N_ICMP_FAILED",
            NetError::DhcpFailed => "N_DHCP_FAILED",
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
    /// Whether a receive descriptor is already published and waiting.
    rx_primed: bool,
    mac: MacAddr,
    link_up: bool,
    /// Whether the transport raises interrupts through MSI-X.
    msix: bool,
}

impl VirtioNet {
    /// Take over a network device: negotiate, allocate both queues, and go live.
    pub fn open(device: &pci::Device) -> Result<Self, NetError> {
        let mut transport = transport::Transport::open(device).map_err(NetError::Transport)?;

        // MSI-X goes first: the device must not pass ACKNOWLEDGE before its
        // interrupt delivery is configured. The destination is the bootstrap
        // processor — the CPU whose snapshot the boot report carries — and the
        // waiter reads a shared count, so a message delivered to a different
        // CPU than the waiter still ends the wait.
        //
        // A device without MSI-X capability is not fatal: the driver falls
        // back to the polling wait it has always had. The failure is logged,
        // not swallowed, so the fallback is visible in the boot output.
        // SAFETY: the boot snapshot was captured before drivers started, and
        // reads are read-only; the same call pattern the rest of the kernel
        // uses after `boot::capture`.
        let msix = match transport.enable_msix(
            device,
            IRQ_VECTOR,
            unsafe { crate::boot::read_boot_info() }.bsp_lapic_id,
        ) {
            Ok(()) => true,
            Err(error) => {
                klog!(
                    Level::Warn,
                    "virtio-net: msi-x unavailable ({}), polling remains",
                    error.name(),
                );
                false
            }
        };

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
            rx_primed: false,
            mac,
            link_up,
            msix,
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

        let completion = match self.await_completion(true) {
            Ok(completion) => completion,
            Err(error) => {
                self.unwind_after_failure();
                return Err(error);
            }
        };
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
        if self.rx_primed {
            return Ok(());
        }
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
        self.rx_primed = true;
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

        let completion = match self.await_completion(false) {
            Ok(completion) => completion,
            Err(error) => {
                self.unwind_after_failure();
                return Err(error);
            }
        };
        if completion.head != 0
            || completion.written < NET_HEADER_BYTES as u32
            || completion.written > capacity as u32
        {
            self.unwind_after_failure();
            return Err(NetError::BadResponse);
        }

        self.rx_primed = false;
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

    /// Resolve the Ethernet address used to reach `destination`.
    ///
    /// QEMU's user network only has the guest, the gateway, the DNS server, and
    /// the SMB server on the emulated link. External addresses are reached
    /// through the gateway, so an ARP request for an off-link address has no
    /// reply and the driver must fall back to the gateway's MAC.
    fn resolve_destination_mac(&mut self, destination: Ipv4Addr) -> Result<MacAddr, NetError> {
        match self.resolve_arp(destination) {
            Ok(mac) => Ok(mac),
            Err(NetError::ArpFailed) | Err(NetError::Timeout) => self.resolve_arp(LINK_GATEWAY),
            Err(error) => Err(error),
        }
    }

    /// Send one UDP datagram. The destination's MAC is resolved first.
    pub fn send_udp(
        &mut self,
        destination: Ipv4Addr,
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), NetError> {
        self.send_udp_from(DNS_SOURCE_PORT, destination, destination_port, payload)
    }

    /// Send one UDP datagram from `source_port`. The destination's MAC is
    /// resolved first.
    pub fn send_udp_from(
        &mut self,
        source_port: u16,
        destination: Ipv4Addr,
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), NetError> {
        let destination_mac = self.resolve_destination_mac(destination)?;
        self.prime_receive()?;
        self.send_udp_to(
            destination,
            destination_mac,
            source_port,
            destination_port,
            payload,
        )
    }

    /// Send one UDP datagram to a known destination MAC.
    fn send_udp_to(
        &mut self,
        destination: Ipv4Addr,
        destination_mac: MacAddr,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), NetError> {
        let mut udp = [0u8; MAX_FRAME_BYTES];
        let udp_length = build_udp(source_port, destination_port, payload, &mut udp)
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

    /// Send one UDP datagram to the IPv4 broadcast address.
    fn send_udp_broadcast(
        &mut self,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
    ) -> Result<(), NetError> {
        let mut udp = [0u8; MAX_FRAME_BYTES];
        let udp_length = build_udp(source_port, destination_port, payload, &mut udp)
            .ok_or(NetError::FrameTooLong)?;

        let mut packet = [0u8; MAX_FRAME_BYTES];
        let packet_length = build_ipv4(
            LINK_ADDRESS,
            Ipv4Addr::new([255, 255, 255, 255]),
            IP_PROTOCOL_UDP,
            &udp[..udp_length],
            &mut packet,
        )
        .ok_or(NetError::FrameTooLong)?;

        let mut frame = [0u8; MAX_FRAME_BYTES];
        let frame_length = build_ethernet(
            BROADCAST_MAC,
            self.mac,
            ETHERTYPE_IPV4,
            &packet[..packet_length],
            &mut frame,
        )
        .ok_or(NetError::FrameTooLong)?;

        self.send_frame(&frame[..frame_length])
    }

    /// Open a TCP connection with a SYN / SYN-ACK / ACK handshake.
    pub fn connect_tcp(
        &mut self,
        destination: Ipv4Addr,
        port: u16,
    ) -> Result<TcpConnection, NetError> {
        self.connect_tcp_from(destination, port, TCP_SOURCE_PORT)
    }

    /// Open a TCP connection from a chosen local port.
    pub fn connect_tcp_from(
        &mut self,
        destination: Ipv4Addr,
        port: u16,
        local_port: u16,
    ) -> Result<TcpConnection, NetError> {
        let remote_mac = self.resolve_destination_mac(destination)?;
        let mut connection = TcpConnection {
            remote: destination,
            remote_mac,
            local_port,
            remote_port: port,
            local_seq: TCP_INITIAL_SEQUENCE,
            remote_seq: 0,
        };

        // SYN.
        let mut tcp = [0u8; MAX_FRAME_BYTES];
        let tcp_length = build_tcp(
            LINK_ADDRESS,
            destination,
            local_port,
            port,
            connection.local_seq,
            0,
            TCP_SYN,
            &[],
            &mut tcp,
        )
        .ok_or(NetError::FrameTooLong)?;
        self.prime_receive()?;
        self.send_tcp_payload(destination, remote_mac, &tcp[..tcp_length])?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let frame_length = self.poll_receive(&mut buffer)?;
            let Some(segment) = self.parse_tcp_frame(&buffer[..frame_length], port, local_port)
            else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            klog!(
                Level::Info,
                "virtio-net: tcp flags {} seq {} ack {} len {}",
                segment.flags,
                segment.sequence,
                segment.acknowledgement,
                segment.payload.len()
            );
            if !segment.has(TCP_SYN) || !segment.has(TCP_ACK) {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            }
            if segment.acknowledgement != connection.local_seq.wrapping_add(1) {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            }

            connection.local_seq = connection.local_seq.wrapping_add(1);
            connection.remote_seq = segment.sequence.wrapping_add(1);

            let mut ack = [0u8; MAX_FRAME_BYTES];
            let ack_length = build_tcp(
                LINK_ADDRESS,
                destination,
                local_port,
                port,
                connection.local_seq,
                connection.remote_seq,
                TCP_ACK,
                &[],
                &mut ack,
            )
            .ok_or(NetError::FrameTooLong)?;
            self.send_tcp_payload(destination, remote_mac, &ack[..ack_length])?;
            return Ok(connection);
        }
        Err(NetError::TcpFailed)
    }

    /// Send bytes on an already-connected TCP connection.
    pub fn send_tcp(
        &mut self,
        connection: &mut TcpConnection,
        payload: &[u8],
    ) -> Result<(), NetError> {
        let mut tcp = [0u8; MAX_FRAME_BYTES];
        let tcp_length = build_tcp(
            LINK_ADDRESS,
            connection.remote,
            connection.local_port,
            connection.remote_port,
            connection.local_seq,
            connection.remote_seq,
            TCP_ACK | TCP_PSH,
            payload,
            &mut tcp,
        )
        .ok_or(NetError::FrameTooLong)?;
        self.prime_receive()?;
        self.send_tcp_payload(connection.remote, connection.remote_mac, &tcp[..tcp_length])?;
        connection.local_seq = connection.local_seq.wrapping_add(payload.len() as u32);
        Ok(())
    }

    /// Receive payload bytes from an already-connected TCP connection.
    pub fn receive_tcp(
        &mut self,
        connection: &mut TcpConnection,
        out: &mut [u8],
    ) -> Result<usize, NetError> {
        let mut frame = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let frame_length = self.receive_frame(&mut frame)?;
            let Some(segment) = self.parse_tcp_frame(
                &frame[..frame_length],
                connection.remote_port,
                connection.local_port,
            ) else {
                continue;
            };

            if segment.payload.is_empty() {
                if segment.has(TCP_FIN) {
                    return Ok(0);
                }
                continue;
            }
            if segment.sequence != connection.remote_seq {
                continue;
            }
            if segment.acknowledgement != connection.local_seq {
                continue;
            }
            if segment.payload.len() > out.len() {
                return Err(NetError::BufferSmall);
            }
            out[..segment.payload.len()].copy_from_slice(segment.payload);
            let length = segment.payload.len();

            connection.remote_seq = connection.remote_seq.wrapping_add(length as u32);

            let mut ack = [0u8; MAX_FRAME_BYTES];
            let ack_length = build_tcp(
                LINK_ADDRESS,
                connection.remote,
                connection.local_port,
                connection.remote_port,
                connection.local_seq,
                connection.remote_seq,
                TCP_ACK,
                &[],
                &mut ack,
            )
            .ok_or(NetError::FrameTooLong)?;
            self.send_tcp_payload(connection.remote, connection.remote_mac, &ack[..ack_length])?;
            return Ok(length);
        }
        Err(NetError::TcpFailed)
    }

    /// Close a TCP connection with FIN.
    pub fn close_tcp(&mut self, connection: &mut TcpConnection) -> Result<(), NetError> {
        let mut tcp = [0u8; MAX_FRAME_BYTES];
        let tcp_length = build_tcp(
            LINK_ADDRESS,
            connection.remote,
            connection.local_port,
            connection.remote_port,
            connection.local_seq,
            connection.remote_seq,
            TCP_FIN | TCP_ACK,
            &[],
            &mut tcp,
        )
        .ok_or(NetError::FrameTooLong)?;
        self.send_tcp_payload(connection.remote, connection.remote_mac, &tcp[..tcp_length])?;
        connection.local_seq = connection.local_seq.wrapping_add(1);
        Ok(())
    }

    /// Send a raw TCP segment inside Ethernet + IPv4 framing.
    fn send_tcp_payload(
        &mut self,
        destination: Ipv4Addr,
        destination_mac: MacAddr,
        tcp: &[u8],
    ) -> Result<(), NetError> {
        let mut packet = [0u8; MAX_FRAME_BYTES];
        let packet_length =
            build_ipv4(LINK_ADDRESS, destination, IP_PROTOCOL_TCP, tcp, &mut packet)
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

    /// Send an ICMP echo request and wait for its reply.
    pub fn icmp_echo(
        &mut self,
        destination: Ipv4Addr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, NetError> {
        let destination_mac = self.resolve_destination_mac(destination)?;

        let mut icmp = [0u8; MAX_FRAME_BYTES];
        let icmp_length = build_icmp_echo_request(identifier, sequence, payload, &mut icmp)
            .ok_or(NetError::FrameTooLong)?;

        let mut packet = [0u8; MAX_FRAME_BYTES];
        let packet_length = build_ipv4(
            LINK_ADDRESS,
            destination,
            IP_PROTOCOL_ICMP,
            &icmp[..icmp_length],
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

        self.prime_receive()?;
        self.send_frame(&frame[..frame_length])?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let length = self.poll_receive(&mut buffer)?;
            let Some(ethernet) = parse_ethernet(&buffer[..length]) else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            if ethernet.ethertype != ETHERTYPE_IPV4 {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            }
            let Some(packet) = parse_ipv4(ethernet.payload) else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            if packet.protocol != IP_PROTOCOL_ICMP || packet.source != destination {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            }
            let Some(reply) = parse_icmp_echo_reply(packet.payload) else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            if reply.identifier != identifier || reply.sequence != sequence {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            }
            if reply.payload.len() > out.len() {
                return Err(NetError::BufferSmall);
            }
            out[..reply.payload.len()].copy_from_slice(reply.payload);
            return Ok(reply.payload.len());
        }
        Err(NetError::IcmpFailed)
    }

    /// Acquire an address from QEMU's built-in DHCP server.
    pub fn dhcp_acquire(&mut self) -> Result<DhcpReply, NetError> {
        let xid = 0x1234_5678u32;

        let mut discover = [0u8; MAX_FRAME_BYTES];
        let discover_length =
            build_dhcp_discover(self.mac, xid, &mut discover).ok_or(NetError::FrameTooLong)?;
        self.prime_receive()?;
        self.send_udp_broadcast(
            DHCP_CLIENT_PORT,
            DHCP_SERVER_PORT,
            &discover[..discover_length],
        )?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        let mut offer = None;
        for attempt in 0..LOOKUP_ATTEMPTS {
            let length = self.poll_receive(&mut buffer)?;
            let Some(datagram) =
                self.parse_udp_frame(&buffer[..length], DHCP_SERVER_PORT, DHCP_CLIENT_PORT)
            else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            let Some(reply) = parse_dhcp_reply(datagram) else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            if reply.message_type == 2 {
                offer = Some(reply);
                break;
            }
            if attempt + 1 < LOOKUP_ATTEMPTS {
                self.prime_receive()?;
            }
        }

        let offer = offer.ok_or(NetError::DhcpFailed)?;
        let server = offer.server.ok_or(NetError::DhcpFailed)?;
        let server_mac = self.resolve_destination_mac(server)?;

        let mut request = [0u8; MAX_FRAME_BYTES];
        let request_length = build_dhcp_request(self.mac, xid, offer.address, server, &mut request)
            .ok_or(NetError::FrameTooLong)?;
        self.prime_receive()?;
        self.send_udp_to(
            server,
            server_mac,
            DHCP_CLIENT_PORT,
            DHCP_SERVER_PORT,
            &request[..request_length],
        )?;

        let mut buffer = [0u8; MAX_FRAME_BYTES];
        for attempt in 0..LOOKUP_ATTEMPTS {
            let length = self.poll_receive(&mut buffer)?;
            let Some(datagram) =
                self.parse_udp_frame(&buffer[..length], DHCP_SERVER_PORT, DHCP_CLIENT_PORT)
            else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            let Some(reply) = parse_dhcp_reply(datagram) else {
                if attempt + 1 < LOOKUP_ATTEMPTS {
                    self.prime_receive()?;
                }
                continue;
            };
            if reply.message_type == 5 {
                return Ok(reply);
            }
            if attempt + 1 < LOOKUP_ATTEMPTS {
                self.prime_receive()?;
            }
        }
        Err(NetError::DhcpFailed)
    }

    /// Parse one TCP segment from an Ethernet + IPv4 frame.
    fn parse_tcp_frame<'a>(
        &self,
        frame: &'a [u8],
        source_port: u16,
        destination_port: u16,
    ) -> Option<TcpSegment<'a>> {
        let ethernet = parse_ethernet(frame)?;
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            return None;
        }
        let packet = parse_ipv4(ethernet.payload)?;
        if packet.protocol != IP_PROTOCOL_TCP {
            return None;
        }
        let segment = parse_tcp(packet.payload, packet.source, packet.destination)?;
        if segment.source_port != source_port || segment.destination_port != destination_port {
            return None;
        }
        Some(segment)
    }

    /// Parse one UDP payload from an Ethernet + IPv4 frame.
    fn parse_udp_frame<'a>(
        &self,
        frame: &'a [u8],
        source_port: u16,
        destination_port: u16,
    ) -> Option<&'a [u8]> {
        let ethernet = parse_ethernet(frame)?;
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            return None;
        }
        let packet = parse_ipv4(ethernet.payload)?;
        if packet.protocol != IP_PROTOCOL_UDP {
            return None;
        }
        let datagram = parse_udp(packet.payload)?;
        if datagram.source_port != source_port || datagram.destination_port != destination_port {
            return None;
        }
        Some(datagram.payload)
    }

    /// Receive one UDP datagram whose source port is `port`.
    pub fn receive_udp(&mut self, port: u16, out: &mut [u8]) -> Result<usize, NetError> {
        self.receive_udp_matching(port, 0, out)
    }

    /// Receive one UDP datagram addressed to a given local port.
    ///
    /// This is the socket path: a bound local port receives whatever datagrams
    /// the remote endpoint sends to it. Matching only the destination port is
    /// what makes a UDP socket a socket rather than a DNS-reply reader.
    pub fn receive_udp_at(&mut self, local_port: u16, out: &mut [u8]) -> Result<usize, NetError> {
        self.receive_udp_matching(0, local_port, out)
    }

    /// Receive one UDP datagram matching `source_port` and `destination_port`.
    ///
    /// A zero source or destination port means "any" for that endpoint.
    fn receive_udp_matching(
        &mut self,
        source_port: u16,
        destination_port: u16,
        out: &mut [u8],
    ) -> Result<usize, NetError> {
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
            klog!(
                Level::Info,
                "virtio-net: udp src {} dst {} len {}",
                datagram.source_port,
                datagram.destination_port,
                datagram.payload.len()
            );
            if source_port != 0 && datagram.source_port != source_port {
                continue;
            }
            if destination_port != 0 && datagram.destination_port != destination_port {
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
                klog!(Level::Info, "virtio-net: dns {:#x}", address.as_u32());
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
        // The count this wait began with: any later increment is an interrupt
        // that arrived *during* the wait, which is the signal this function
        // exists to receive.
        let irqs_at_start = device_irq_count();
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

            // An interrupt since the wait began: switch from waiting to
            // draining. The first read after the flag can still miss the
            // completion (the used ring's index and the flag race by one
            // read), so the drain window stays bounded rather than single.
            if self.msix && device_irq_count() != irqs_at_start {
                budget = budget.min(IRQ_DRAIN_BUDGET);
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
        self.rx_primed = false;

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
    // SAFETY: `mm::init` published the allocator during boot. The device reads
    // this frame's address as a 32-bit DMA word, so it must come from the
    // DMA-capable pool below the 4 GiB boundary.
    let frame = unsafe { frame_allocator().alloc_dma() }
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
