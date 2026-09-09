//! Networking: one interface, and enough of the Internet to be on it.
//!
//! The card driver moves frames. This decides what they mean. Between them
//! there is nothing else: no socket layer yet, no routing table, one interface
//! and one address.
//!
//! # What is here
//!
//! ARP, IPv4, ICMP echo, UDP, and a DHCP client — which is the shortest path to
//! a claim that can be checked. A machine that has an address it was *given*
//! has necessarily sent a broadcast, had it received by a server, parsed the
//! offer, asked for the address, and been acknowledged. Four frames each way,
//! every layer of the stack, and an answer that could not have been invented
//! locally: 10.0.2.15 comes from somewhere.
//!
//! Then it pings the gateway, which is the other half: DHCP proves broadcast
//! and UDP, and a ping proves ARP, unicast, and that something out there
//! answered a packet this machine addressed to it.
//!
//! # Where it runs
//!
//! One kernel thread. Frames arrive by interrupt and the interrupt does nothing
//! but acknowledge the card and wake this thread, because everything below —
//! parsing, the ARP cache, sending a reply — takes locks and allocates.
//!
//! # What is not here
//!
//! TCP, IPv6, a routing table, fragmentation, a firewall, and any way for a
//! program to use any of it. Each is its own piece of work and each is named in
//! the roadmap; none of them is stubbed out here, because a stub that returns
//! success is worse than a function that does not exist.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::drivers::virtio_net;
use crate::sync::IrqSpinLock;
use crate::{arch, kprintln, sched};

use nexus_net as wire;
use nexus_net::{Ipv4, Mac};

/// Everything this machine knows about being on a network.
#[derive(Clone, Copy)]
pub struct Interface {
    /// The card's address, which is the card's and not this module's.
    pub mac: Mac,
    /// This machine's address, or all zeroes before it has one.
    pub ip: Ipv4,
    /// Which part of it names the network.
    pub mask: Ipv4,
    /// Where to send anything that is not on this network.
    pub gateway: Ipv4,
    /// Who to ask about names.
    pub dns: Ipv4,
    /// Who gave out the address, and for how long.
    pub server: Ipv4,
    pub lease_seconds: u32,
}

impl Interface {
    const fn new() -> Self {
        Self {
            mac: [0; 6],
            ip: wire::UNSPECIFIED,
            mask: wire::UNSPECIFIED,
            gateway: wire::UNSPECIFIED,
            dns: wire::UNSPECIFIED,
            server: wire::UNSPECIFIED,
            lease_seconds: 0,
        }
    }

    /// Whether `address` is on this machine's own network.
    ///
    /// What decides whether a packet goes straight to its destination or to the
    /// gateway. With no mask nothing is local, which is the right answer before
    /// there is an address.
    fn is_local(&self, address: Ipv4) -> bool {
        self.mask != wire::UNSPECIFIED
            && (0..4).all(|i| self.ip[i] & self.mask[i] == address[i] & self.mask[i])
    }
}

/// The one interface.
static INTERFACE: IrqSpinLock<Interface> = IrqSpinLock::new(Interface::new());

/// One learned hardware address.
#[derive(Clone, Copy)]
struct Neighbour {
    ip: Ipv4,
    mac: Mac,
    known: bool,
}

/// What this machine has learned about who is where.
///
/// A fixed table rather than a map: it is consulted from a lock and the whole
/// point of it is to be small. Eight is more neighbours than a machine with one
/// gateway ever talks to, and the oldest is overwritten rather than aged out —
/// an entry that is wrong is corrected by the next reply, and one that is stale
/// costs one ARP.
static NEIGHBOURS: IrqSpinLock<[Neighbour; 8]> = IrqSpinLock::new(
    [Neighbour {
        ip: wire::UNSPECIFIED,
        mac: [0; 6],
        known: false,
    }; 8],
);

/// Where the last DHCP reply was put, for the client to read.
static OFFER: IrqSpinLock<Option<wire::Dhcp>> = IrqSpinLock::new(None);

/// What the last ping got back, in milliseconds, and whether it got back.
static PING_REPLY: AtomicU64 = AtomicU64::new(u64::MAX);
/// The identifier the outstanding ping used.
static PING_ID: AtomicU64 = AtomicU64::new(0);

/// Whether the interface is configured.
static UP: AtomicBool = AtomicBool::new(false);

/// Counters, for the monitor.
static FRAMES_IN: AtomicU64 = AtomicU64::new(0);
static FRAMES_OUT: AtomicU64 = AtomicU64::new(0);
static ARP_ANSWERED: AtomicU64 = AtomicU64::new(0);
static ECHOES_ANSWERED: AtomicU64 = AtomicU64::new(0);
static UNKNOWN: AtomicU64 = AtomicU64::new(0);

/// The largest frame this module builds or reads.
const MTU: usize = virtio_net::MAX_FRAME;

// -- The thread ----------------------------------------------------------------

/// Start the thread that runs the stack.
pub fn start_thread() {
    if !virtio_net::is_present() {
        return;
    }
    match sched::spawn(
        "net",
        // Interactive: a frame that has arrived is something on the other side
        // of a wire waiting, and the reply is late from the moment it lands.
        sched::thread::Priority::Interactive,
        network_thread,
        0,
    ) {
        Ok(id) => kprintln!("[net ] network thread {id} started"),
        Err(error) => kprintln!("[net ] could not start the network thread: {error}"),
    }
}

/// Configure the interface, prove it works, and then serve.
fn network_thread(_argument: usize) {
    if let Some(mac) = virtio_net::address() {
        INTERFACE.lock().mac = mac;
    }

    if configure() {
        UP.store(true, Ordering::Release);
        prove();
    } else {
        kprintln!("[net ] no address; the interface is up but unconfigured");
    }

    // And then it is a service. Frames arrive, get answered, and this waits in
    // between.
    //
    // How it waits depends on whether the card's interrupt has proved itself.
    // Once it has, the thread blocks and costs nothing between frames. Until
    // then it polls, slowly -- because a thread that blocked on an interrupt
    // that never comes is a network that stops the moment nothing else happens
    // to be going on, and that failure looks exactly like a broken cable.
    loop {
        drain();
        if virtio_net::adopt_interrupt() {
            kprintln!("[net ] the card's interrupt arrived; the thread now blocks between frames");
        }
        if virtio_net::is_blocking() {
            virtio_net::wait_for_frame();
        } else {
            sched::sleep_ms(20);
        }
    }
}

/// Take every frame the card has.
fn drain() {
    let mut frame = [0u8; MTU];
    while let Some(length) = virtio_net::receive(&mut frame) {
        FRAMES_IN.fetch_add(1, Ordering::Relaxed);
        handle(&frame[..length]);
    }
}

// -- Receiving -----------------------------------------------------------------

/// One frame.
fn handle(frame: &[u8]) {
    let Some(parsed) = wire::parse_ethernet(frame) else {
        return;
    };
    // A card in promiscuous mode would see the whole segment. This one is not,
    // but a frame is checked anyway: the address is in the frame and trusting
    // the card to have filtered is trusting a device to enforce something the
    // driver can check for nothing.
    let mine = INTERFACE.lock().mac;
    if parsed.to != mine && parsed.to != wire::BROADCAST {
        return;
    }

    match parsed.kind {
        wire::ether::ARP => handle_arp(parsed.payload),
        wire::ether::IPV4 => handle_ipv4(parsed.payload),
        _ => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// An ARP packet: learn from it, and answer it if it was a question for us.
fn handle_arp(bytes: &[u8]) {
    let Some(packet) = wire::parse_arp(bytes) else {
        return;
    };

    // Learn from every ARP, question or answer. The sender put its own address
    // in the packet, so a request from the gateway teaches as much as a reply.
    remember(packet.sender_ip, packet.sender_mac);

    if packet.operation != wire::arp_op::REQUEST {
        return;
    }
    let interface = *INTERFACE.lock();
    if interface.ip == wire::UNSPECIFIED || packet.target_ip != interface.ip {
        return;
    }

    let mut reply = [0u8; wire::ARP_SIZE];
    wire::arp(
        &mut reply,
        &wire::Arp {
            operation: wire::arp_op::REPLY,
            sender_mac: interface.mac,
            sender_ip: interface.ip,
            target_mac: packet.sender_mac,
            target_ip: packet.sender_ip,
        },
    );
    if send_frame(packet.sender_mac, wire::ether::ARP, &reply).is_ok() {
        ARP_ANSWERED.fetch_add(1, Ordering::Relaxed);
    }
}

/// An IPv4 packet.
fn handle_ipv4(bytes: &[u8]) {
    let Some(packet) = wire::parse_ipv4(bytes) else {
        return;
    };
    let interface = *INTERFACE.lock();
    // Before there is an address, only broadcasts are for us -- which is
    // exactly the case a DHCP offer arrives in.
    if packet.to != interface.ip
        && packet.to != wire::BROADCAST_IPV4
        && interface.ip != wire::UNSPECIFIED
    {
        return;
    }

    match packet.protocol {
        wire::protocol::ICMP => handle_icmp(&interface, packet.from, packet.payload),
        wire::protocol::UDP => handle_udp(packet.from, packet.payload),
        _ => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// An ICMP message: answer an echo, and notice a reply to one of ours.
fn handle_icmp(interface: &Interface, from: Ipv4, bytes: &[u8]) {
    if bytes.len() < 8 {
        return;
    }
    // The checksum covers the whole message. A reply built from a corrupted
    // request would be a reply to something nobody sent.
    if wire::checksum(&[bytes]) != 0 {
        return;
    }

    match bytes[0] {
        wire::icmp_type::ECHO_REQUEST => {
            if interface.ip == wire::UNSPECIFIED {
                return;
            }
            // The payload comes back unchanged, which is what makes an echo an
            // echo: the sender checks its own bytes.
            let mut message = [0u8; MTU];
            let payload = &bytes[8..];
            let length = wire::icmp_echo(
                &mut message,
                wire::icmp_type::ECHO_REPLY,
                wire::be16(bytes, 4),
                wire::be16(bytes, 6),
                &payload[..payload.len().min(MTU - 8 - wire::IPV4_HEADER)],
            );
            if send_ipv4(from, wire::protocol::ICMP, &message[..length]).is_ok() {
                ECHOES_ANSWERED.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Only ours. An identifier is what separates this machine's ping from
        // any other echo on the segment.
        wire::icmp_type::ECHO_REPLY
            if u64::from(wire::be16(bytes, 4)) == PING_ID.load(Ordering::Relaxed) =>
        {
            PING_REPLY.store(arch::time::uptime_ms(), Ordering::Release);
        }
        _ => {}
    }
}

/// A UDP datagram. Only the DHCP client is listening.
fn handle_udp(_from: Ipv4, bytes: &[u8]) {
    let Some(datagram) = wire::parse_udp(bytes) else {
        return;
    };
    if datagram.destination == wire::DHCP_CLIENT_PORT {
        if let Some(message) = wire::parse_dhcp(datagram.payload) {
            *OFFER.lock() = Some(message);
        }
        return;
    }
    UNKNOWN.fetch_add(1, Ordering::Relaxed);
}

// -- Sending -------------------------------------------------------------------

/// Put a frame on the wire.
fn send_frame(to: Mac, kind: u16, payload: &[u8]) -> Result<(), virtio_net::NetError> {
    let mut frame = [0u8; MTU];
    let from = INTERFACE.lock().mac;
    let header = wire::ethernet(&mut frame, to, from, kind);
    if header + payload.len() > MTU {
        return Err(virtio_net::NetError::TooLong(header + payload.len()));
    }
    frame[header..header + payload.len()].copy_from_slice(payload);

    // Padded to sixty bytes. Ethernet's minimum frame is not a formality: a
    // shorter one is a runt, and what drops it is somewhere out on the wire
    // where nothing reports back.
    let length = (header + payload.len()).max(60);
    virtio_net::transmit(&frame[..length])?;
    FRAMES_OUT.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Send an IPv4 packet, working out who to hand it to.
///
/// Local addresses are ARPed for; everything else goes to the gateway. That is
/// the whole of this machine's routing, and it is one line because there is one
/// interface: a routing table is what this becomes when there are two.
pub fn send_ipv4(to: Ipv4, protocol: u8, payload: &[u8]) -> Result<(), virtio_net::NetError> {
    let interface = *INTERFACE.lock();
    let next = if to == wire::BROADCAST_IPV4 || interface.is_local(to) {
        to
    } else {
        interface.gateway
    };

    let target = if to == wire::BROADCAST_IPV4 {
        wire::BROADCAST
    } else {
        match resolve(next) {
            Some(mac) => mac,
            None => return Err(virtio_net::NetError::NoDevice),
        }
    };

    let mut packet = [0u8; MTU];
    if wire::IPV4_HEADER + payload.len() > MTU - wire::ETHERNET_HEADER {
        return Err(virtio_net::NetError::TooLong(payload.len()));
    }
    packet[wire::IPV4_HEADER..wire::IPV4_HEADER + payload.len()].copy_from_slice(payload);
    // The identification field only matters to something reassembling
    // fragments, and nothing here fragments -- but it has to differ between
    // packets, so it counts.
    let identification = FRAMES_OUT.load(Ordering::Relaxed) as u16;
    wire::ipv4(
        &mut packet,
        interface.ip,
        to,
        protocol,
        payload.len(),
        identification,
    );
    send_frame(
        target,
        wire::ether::IPV4,
        &packet[..wire::IPV4_HEADER + payload.len()],
    )
}

// -- ARP -----------------------------------------------------------------------

/// Note that `ip` is at `mac`.
fn remember(ip: Ipv4, mac: Mac) {
    if ip == wire::UNSPECIFIED {
        return;
    }
    let mut table = NEIGHBOURS.lock();
    if let Some(entry) = table.iter_mut().find(|entry| entry.known && entry.ip == ip) {
        entry.mac = mac;
        return;
    }
    if let Some(entry) = table.iter_mut().find(|entry| !entry.known) {
        *entry = Neighbour {
            ip,
            mac,
            known: true,
        };
        return;
    }
    table[0] = Neighbour {
        ip,
        mac,
        known: true,
    };
}

/// What is at `ip`, asking if necessary.
fn resolve(ip: Ipv4) -> Option<Mac> {
    if let Some(mac) = lookup(ip) {
        return Some(mac);
    }

    let interface = *INTERFACE.lock();
    if interface.ip == wire::UNSPECIFIED {
        return None;
    }

    // Asked more than once. A single request that is lost is a machine that
    // decides the gateway does not exist, and the first ARP on a segment is
    // exactly the frame most likely to be dropped.
    for _ in 0..3 {
        let mut request = [0u8; wire::ARP_SIZE];
        wire::arp(
            &mut request,
            &wire::Arp {
                operation: wire::arp_op::REQUEST,
                sender_mac: interface.mac,
                sender_ip: interface.ip,
                target_mac: [0; 6],
                target_ip: ip,
            },
        );
        send_frame(wire::BROADCAST, wire::ether::ARP, &request).ok()?;

        let deadline = arch::time::uptime_ms() + 300;
        while arch::time::uptime_ms() < deadline {
            drain();
            if let Some(mac) = lookup(ip) {
                return Some(mac);
            }
            sched::sleep_ms(5);
        }
    }
    None
}

/// What is at `ip`, if it is already known.
fn lookup(ip: Ipv4) -> Option<Mac> {
    NEIGHBOURS
        .lock()
        .iter()
        .find(|entry| entry.known && entry.ip == ip)
        .map(|entry| entry.mac)
}

// -- DHCP ----------------------------------------------------------------------

/// Get an address, or fail saying so.
///
/// Four messages: discover, offer, request, acknowledge. The request is not a
/// formality — it is what tells every *other* server on the segment that their
/// offer was not taken, and a client that skipped it would be holding addresses
/// it never uses.
fn configure() -> bool {
    let mac = INTERFACE.lock().mac;
    // A transaction identifier that differs between boots, so a reply to a
    // previous run's conversation is not mistaken for this one's. The clock and
    // the card's address are the two things to hand that are not constant.
    let transaction = (arch::time::uptime_ms() as u32).wrapping_mul(2_654_435_761)
        ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);

    for attempt in 0..4 {
        *OFFER.lock() = None;
        if !send_dhcp(
            wire::dhcp_type::DISCOVER,
            transaction,
            wire::UNSPECIFIED,
            wire::UNSPECIFIED,
        ) {
            return false;
        }

        let Some(offer) = await_dhcp(transaction, wire::dhcp_type::OFFER, 1000) else {
            kprintln!("[net ] no DHCP offer (attempt {})", attempt + 1);
            continue;
        };

        *OFFER.lock() = None;
        if !send_dhcp(
            wire::dhcp_type::REQUEST,
            transaction,
            offer.offered,
            offer.server,
        ) {
            return false;
        }

        let Some(ack) = await_dhcp(transaction, wire::dhcp_type::ACK, 1000) else {
            kprintln!(
                "[net ] the offer was not acknowledged (attempt {})",
                attempt + 1
            );
            continue;
        };

        let mut interface = INTERFACE.lock();
        interface.ip = ack.offered;
        interface.mask = if ack.mask == wire::UNSPECIFIED {
            [255, 255, 255, 0]
        } else {
            ack.mask
        };
        interface.gateway = ack.router;
        interface.dns = ack.dns;
        interface.server = ack.server;
        interface.lease_seconds = ack.lease;
        let interface = *interface;

        kprintln!(
            "[net ] address {}.{}.{}.{}/{} from {}.{}.{}.{}, gateway {}.{}.{}.{}, \
             DNS {}.{}.{}.{}, lease {} s",
            ack.offered[0],
            ack.offered[1],
            ack.offered[2],
            ack.offered[3],
            prefix_length(interface.mask),
            ack.server[0],
            ack.server[1],
            ack.server[2],
            ack.server[3],
            ack.router[0],
            ack.router[1],
            ack.router[2],
            ack.router[3],
            ack.dns[0],
            ack.dns[1],
            ack.dns[2],
            ack.dns[3],
            ack.lease
        );
        return true;
    }
    false
}

/// How many bits of a mask are set, which is how a mask is written down.
fn prefix_length(mask: Ipv4) -> u32 {
    mask.iter().map(|byte| byte.count_ones()).sum()
}

/// Wait for a DHCP message of a particular kind belonging to this conversation.
fn await_dhcp(transaction: u32, kind: u8, milliseconds: u64) -> Option<wire::Dhcp> {
    let deadline = arch::time::uptime_ms() + milliseconds;
    loop {
        drain();
        {
            let mut slot = OFFER.lock();
            if let Some(message) = *slot {
                if message.transaction == transaction && message.kind == kind {
                    *slot = None;
                    return Some(message);
                }
                if message.transaction == transaction && message.kind == wire::dhcp_type::NAK {
                    *slot = None;
                    return None;
                }
                // Somebody else's conversation, or a kind that is not wanted.
                *slot = None;
            }
        }
        if arch::time::uptime_ms() >= deadline {
            return None;
        }
        sched::sleep_ms(5);
    }
}

/// Send one DHCP message.
fn send_dhcp(kind: u8, transaction: u32, requested: Ipv4, server: Ipv4) -> bool {
    let mac = INTERFACE.lock().mac;
    let mut message = [0u8; 300];

    message[0] = 1; // a request, from a client
    message[1] = 1; // over Ethernet
    message[2] = 6; // with six-byte addresses
    message[3] = 0; // and no relays
    wire::put32(&mut message, 4, transaction);
    // Broadcast, because a machine with no address cannot be sent a unicast it
    // would accept: the reply would be addressed to an IP it does not have yet.
    wire::put16(&mut message, 10, 0x8000);
    message[28..34].copy_from_slice(&mac);
    wire::put32(&mut message, wire::DHCP_FIXED, wire::MAGIC);

    let mut at = wire::DHCP_FIXED + 4;
    at = wire::option(&mut message, at, 53, &[kind]);
    if requested != wire::UNSPECIFIED {
        at = wire::option(&mut message, at, 50, &requested);
    }
    if server != wire::UNSPECIFIED {
        at = wire::option(&mut message, at, 54, &server);
    }
    // What this client wants told: the mask, the router and a resolver. Asked
    // for rather than assumed, because a server is entitled to send only what
    // it was asked about.
    at = wire::option(&mut message, at, 55, &[1, 3, 6]);
    message[at] = 255; // end
    at += 1;

    let mut datagram = [0u8; wire::UDP_HEADER + 300];
    datagram[wire::UDP_HEADER..wire::UDP_HEADER + at].copy_from_slice(&message[..at]);
    wire::udp(
        &mut datagram,
        wire::UNSPECIFIED,
        wire::BROADCAST_IPV4,
        wire::DHCP_CLIENT_PORT,
        wire::DHCP_SERVER_PORT,
        at,
    );

    send_ipv4(
        wire::BROADCAST_IPV4,
        wire::protocol::UDP,
        &datagram[..wire::UDP_HEADER + at],
    )
    .is_ok()
}

// -- Proving it ----------------------------------------------------------------

/// Ping the gateway, once, and say what happened.
///
/// The other half of the claim. DHCP proves broadcast, UDP and that a server
/// out there parsed what this machine sent. A ping proves ARP -- the gateway's
/// hardware address had to be *asked for* -- and unicast in both directions.
fn prove() {
    let gateway = INTERFACE.lock().gateway;
    if gateway == wire::UNSPECIFIED {
        return;
    }

    match ping(gateway, 1000) {
        Some(milliseconds) => kprintln!(
            "[net ] ping {}.{}.{}.{}: reply in {} ms",
            gateway[0],
            gateway[1],
            gateway[2],
            gateway[3],
            milliseconds
        ),
        None => kprintln!(
            "[net ] ping {}.{}.{}.{}: no reply",
            gateway[0],
            gateway[1],
            gateway[2],
            gateway[3]
        ),
    }
}

/// Send one echo request and wait for its reply.
///
/// Returns how long it took, in milliseconds.
pub fn ping(target: Ipv4, milliseconds: u64) -> Option<u64> {
    let identifier = (arch::time::uptime_ms() as u16) | 0x8000;
    PING_ID.store(u64::from(identifier), Ordering::Release);
    PING_REPLY.store(u64::MAX, Ordering::Release);

    let mut message = [0u8; 8 + 16];
    let length = wire::icmp_echo(
        &mut message,
        wire::icmp_type::ECHO_REQUEST,
        identifier,
        1,
        b"NexusOS ping    ",
    );

    let sent = arch::time::uptime_ms();
    send_ipv4(target, wire::protocol::ICMP, &message[..length]).ok()?;

    let deadline = sent + milliseconds;
    loop {
        drain();
        let at = PING_REPLY.load(Ordering::Acquire);
        if at != u64::MAX {
            return Some(at.saturating_sub(sent));
        }
        if arch::time::uptime_ms() >= deadline {
            return None;
        }
        sched::sleep_ms(5);
    }
}

// -- What the rest of the system can ask ---------------------------------------

/// Whether the interface has an address.
#[must_use]
pub fn is_up() -> bool {
    UP.load(Ordering::Acquire)
}

/// Everything the interface knows.
#[must_use]
pub fn interface() -> Interface {
    *INTERFACE.lock()
}

/// Frames in, frames out, ARP requests answered, echoes answered, and frames
/// this machine had nothing to do with.
#[must_use]
pub fn statistics() -> (u64, u64, u64, u64, u64) {
    (
        FRAMES_IN.load(Ordering::Relaxed),
        FRAMES_OUT.load(Ordering::Relaxed),
        ARP_ANSWERED.load(Ordering::Relaxed),
        ECHOES_ANSWERED.load(Ordering::Relaxed),
        UNKNOWN.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mask_says_what_is_local() {
        let interface = Interface {
            mac: [0; 6],
            ip: [10, 0, 2, 15],
            mask: [255, 255, 255, 0],
            gateway: [10, 0, 2, 2],
            dns: [10, 0, 2, 3],
            server: [10, 0, 2, 2],
            lease_seconds: 86400,
        };
        assert!(interface.is_local([10, 0, 2, 2]));
        assert!(interface.is_local([10, 0, 2, 255]));
        assert!(!interface.is_local([10, 0, 3, 1]));
        assert!(!interface.is_local([8, 8, 8, 8]));
    }

    #[test]
    fn nothing_is_local_without_a_mask() {
        // Before DHCP there is no mask, and a machine that thought the whole
        // Internet was on its segment would ARP for every address it was given.
        let interface = Interface::new();
        assert!(!interface.is_local([10, 0, 2, 2]));
    }

    #[test]
    fn a_prefix_is_the_bits_that_are_set() {
        assert_eq!(prefix_length([255, 255, 255, 0]), 24);
        assert_eq!(prefix_length([255, 0, 0, 0]), 8);
        assert_eq!(prefix_length([0, 0, 0, 0]), 0);
    }

    #[test]
    fn an_offer_is_read_back_out_of_a_real_message() {
        // A message shaped exactly as a server sends one: the fixed part, the
        // magic, and options in the order they usually arrive.
        let mut bytes = [0u8; wire::DHCP_FIXED + 4 + 32];
        bytes[0] = 2; // a reply
        wire::put32(&mut bytes, 4, 0xDEAD_BEEF);
        bytes[16..20].copy_from_slice(&[10, 0, 2, 15]);
        wire::put32(&mut bytes, wire::DHCP_FIXED, wire::MAGIC);

        let mut at = wire::DHCP_FIXED + 4;
        at = option(&mut bytes, at, 53, &[wire::dhcp_type::OFFER]);
        at = option(&mut bytes, at, 1, &[255, 255, 255, 0]);
        at = option(&mut bytes, at, 3, &[10, 0, 2, 2]);
        at = option(&mut bytes, at, 6, &[10, 0, 2, 3]);
        at = option(&mut bytes, at, 51, &86_400u32.to_be_bytes());
        at = option(&mut bytes, at, 54, &[10, 0, 2, 2]);
        bytes[at] = 255;

        let message = wire::parse_dhcp(&bytes).expect("a message this shape must parse");
        assert_eq!(message.kind, wire::dhcp_type::OFFER);
        assert_eq!(message.offered, [10, 0, 2, 15]);
        assert_eq!(message.mask, [255, 255, 255, 0]);
        assert_eq!(message.router, [10, 0, 2, 2]);
        assert_eq!(message.dns, [10, 0, 2, 3]);
        assert_eq!(message.lease, 86_400);
        assert_eq!(message.server, [10, 0, 2, 2]);
        assert_eq!(message.transaction, 0xDEAD_BEEF);
    }

    #[test]
    fn padding_between_options_is_skipped() {
        let mut bytes = [0u8; wire::DHCP_FIXED + 4 + 16];
        bytes[0] = 2;
        wire::put32(&mut bytes, wire::DHCP_FIXED, wire::MAGIC);
        let mut at = wire::DHCP_FIXED + 4;
        // A pad byte has no length of its own, so a parser that treated it like
        // any other option would read the next option's code as a length and
        // walk off the end of the message.
        bytes[at] = 0;
        at += 1;
        at = option(&mut bytes, at, 53, &[wire::dhcp_type::ACK]);
        bytes[at] = 255;

        let message = wire::parse_dhcp(&bytes).expect("padding is not an error");
        assert_eq!(message.kind, wire::dhcp_type::ACK);
    }

    #[test]
    fn a_message_without_the_magic_is_not_dhcp() {
        let mut bytes = [0u8; wire::DHCP_FIXED + 4 + 8];
        bytes[0] = 2;
        // A BOOTP reply, which has the same fixed part and no options this
        // client can read. Treating it as DHCP would mean accepting an address
        // from a conversation that never happened.
        wire::put32(&mut bytes, wire::DHCP_FIXED, 0);
        assert!(wire::parse_dhcp(&bytes).is_none());
    }

    #[test]
    fn an_option_running_past_the_end_stops_the_walk() {
        let mut bytes = [0u8; wire::DHCP_FIXED + 4 + 8];
        bytes[0] = 2;
        wire::put32(&mut bytes, wire::DHCP_FIXED, wire::MAGIC);
        let at = wire::DHCP_FIXED + 4;
        bytes[at] = 6;
        // A length longer than what is left. A parser that trusted it would
        // read past the message; this one stops and reports what it has.
        bytes[at + 1] = 200;
        let message = wire::parse_dhcp(&bytes).expect("a truncated option is not a broken message");
        assert_eq!(message.dns, wire::UNSPECIFIED);
    }

    #[test]
    fn a_request_this_client_sends_can_be_read_back() {
        // Not sent -- there is no card in a host test. Built the same way, and
        // parsed by the same parser a server would use for the option walk.
        let mut message = [0u8; 300];
        message[0] = 2; // pretend it is a reply, so the parser will read it
        wire::put32(&mut message, 4, 0x0102_0304);
        wire::put32(&mut message, wire::DHCP_FIXED, wire::MAGIC);
        let mut at = wire::DHCP_FIXED + 4;
        at = wire::option(&mut message, at, 53, &[wire::dhcp_type::REQUEST]);
        at = wire::option(&mut message, at, 50, &[10, 0, 2, 15]);
        at = wire::option(&mut message, at, 54, &[10, 0, 2, 2]);
        message[at] = 255;

        let parsed = wire::parse_dhcp(&message).expect("what this writes, it can read");
        assert_eq!(parsed.kind, wire::dhcp_type::REQUEST);
        assert_eq!(parsed.server, [10, 0, 2, 2]);
    }
}
