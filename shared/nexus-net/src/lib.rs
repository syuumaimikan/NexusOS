#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! The shapes on the wire: Ethernet, ARP, IPv4, ICMP and UDP.
//!
//! Reading and writing bytes, and nothing else. No state, no device, no
//! decisions — every function here takes a slice and either makes sense of it
//! or does not. That is what makes the protocols testable on the host: the
//! tests at the bottom of this file build a packet, parse it back, and check
//! the checksum arithmetic against numbers taken from the standards, on a
//! machine with no network card in it.
//!
//! # Byte order
//!
//! Everything on the wire is big endian, which is the opposite of the machine.
//! There is no type here that hides that: numbers are read and written through
//! `be16`/`be32` at the exact points where they cross, so a field that was
//! never converted is a field that is visibly not converted.

/// A hardware address.
pub type Mac = [u8; 6];
/// An IPv4 address.
pub type Ipv4 = [u8; 4];

/// The address every card on the segment answers to.
pub const BROADCAST: Mac = [0xFF; 6];
/// The address that means "no address yet".
pub const UNSPECIFIED: Ipv4 = [0, 0, 0, 0];
/// The address that means "everyone on this segment".
pub const BROADCAST_IPV4: Ipv4 = [0xFF, 0xFF, 0xFF, 0xFF];

/// Bytes of Ethernet header: destination, source, and what is inside.
pub const ETHERNET_HEADER: usize = 14;

/// What an Ethernet frame carries, as the type field says.
pub mod ether {
    pub const IPV4: u16 = 0x0800;
    pub const ARP: u16 = 0x0806;
}

/// What an IPv4 packet carries, as the protocol field says.
pub mod protocol {
    pub const ICMP: u8 = 1;
    pub const UDP: u8 = 17;
    /// Not spoken yet. Named here because a packet is dispatched on this byte
    /// and one that is silently lumped in with the unknown is a protocol nobody
    /// notices arriving.
    #[allow(dead_code)]
    pub const TCP: u8 = 6;
}

/// Read a big-endian 16-bit number.
#[must_use]
pub fn be16(bytes: &[u8], at: usize) -> u16 {
    if at + 2 > bytes.len() {
        return 0;
    }
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// Read a big-endian 32-bit number.
#[must_use]
pub fn be32(bytes: &[u8], at: usize) -> u32 {
    if at + 4 > bytes.len() {
        return 0;
    }
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Write a big-endian 16-bit number.
pub fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_be_bytes());
}

/// Write a big-endian 32-bit number.
pub fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// The Internet checksum: the one's complement of the one's complement sum.
///
/// Used by IPv4, ICMP, UDP and TCP, with different things fed into it. It is
/// the same arithmetic every time, which is why it is one function: a system
/// with four copies of this would be a system where three of them are subtly
/// different.
///
/// The end-around carry is what makes it one's complement addition rather than
/// ordinary addition, and it is the part that is usually got wrong: a carry out
/// of the top has to come back in at the bottom, and it can carry again.
#[must_use]
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    // A byte left over from one part pairs with the first byte of the next: the
    // sum is over the concatenation, not over each part separately.
    let mut half: Option<u8> = None;

    for part in parts {
        let mut index = 0;
        if let Some(high) = half.take() {
            if part.is_empty() {
                half = Some(high);
                continue;
            }
            sum += u32::from(u16::from_be_bytes([high, part[0]]));
            index = 1;
        }
        while index + 1 < part.len() {
            sum += u32::from(u16::from_be_bytes([part[index], part[index + 1]]));
            index += 2;
        }
        if index < part.len() {
            half = Some(part[index]);
        }
    }
    if let Some(high) = half {
        // An odd total length is padded with a zero byte, which is not the same
        // as ignoring it.
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Write an Ethernet header into `frame` and say how long it is.
pub fn ethernet(frame: &mut [u8], to: Mac, from: Mac, kind: u16) -> usize {
    frame[0..6].copy_from_slice(&to);
    frame[6..12].copy_from_slice(&from);
    put16(frame, 12, kind);
    ETHERNET_HEADER
}

/// What an Ethernet frame turned out to be.
pub struct Frame<'a> {
    pub to: Mac,
    /// Who sent it. Not looked at above Ethernet: the addresses that matter are
    /// the ones inside the packet, and a frame's source is only ever learned
    /// from -- which ARP does out of the ARP packet itself, where the sender
    /// has said so on the record.
    #[allow(dead_code)]
    pub from: Mac,
    pub kind: u16,
    pub payload: &'a [u8],
}

/// Make sense of a frame, or not.
#[must_use]
pub fn parse_ethernet(frame: &[u8]) -> Option<Frame<'_>> {
    if frame.len() < ETHERNET_HEADER {
        return None;
    }
    let mut to = [0u8; 6];
    let mut from = [0u8; 6];
    to.copy_from_slice(&frame[0..6]);
    from.copy_from_slice(&frame[6..12]);
    Some(Frame {
        to,
        from,
        kind: be16(frame, 12),
        payload: &frame[ETHERNET_HEADER..],
    })
}

// -- ARP -----------------------------------------------------------------------

/// Bytes of an ARP packet for IPv4 over Ethernet. Fixed: the lengths are in the
/// packet, and every other combination is somebody else's network.
pub const ARP_SIZE: usize = 28;

/// What an ARP packet is asking or answering.
pub mod arp_op {
    pub const REQUEST: u16 = 1;
    pub const REPLY: u16 = 2;
}

/// What an ARP packet said.
pub struct Arp {
    pub operation: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4,
    pub target_mac: Mac,
    pub target_ip: Ipv4,
}

/// Write an ARP packet, and say how long it is.
pub fn arp(into: &mut [u8], packet: &Arp) -> usize {
    put16(into, 0, 1); // Ethernet
    put16(into, 2, ether::IPV4);
    into[4] = 6;
    into[5] = 4;
    put16(into, 6, packet.operation);
    into[8..14].copy_from_slice(&packet.sender_mac);
    into[14..18].copy_from_slice(&packet.sender_ip);
    into[18..24].copy_from_slice(&packet.target_mac);
    into[24..28].copy_from_slice(&packet.target_ip);
    ARP_SIZE
}

/// Make sense of an ARP packet, or not.
///
/// Anything that is not IPv4 over Ethernet is rejected rather than interpreted:
/// the hardware and protocol lengths are in the packet precisely so that a
/// reader can tell it is not for them.
#[must_use]
pub fn parse_arp(bytes: &[u8]) -> Option<Arp> {
    if bytes.len() < ARP_SIZE {
        return None;
    }
    if be16(bytes, 0) != 1 || be16(bytes, 2) != ether::IPV4 || bytes[4] != 6 || bytes[5] != 4 {
        return None;
    }
    let mut packet = Arp {
        operation: be16(bytes, 6),
        sender_mac: [0; 6],
        sender_ip: [0; 4],
        target_mac: [0; 6],
        target_ip: [0; 4],
    };
    packet.sender_mac.copy_from_slice(&bytes[8..14]);
    packet.sender_ip.copy_from_slice(&bytes[14..18]);
    packet.target_mac.copy_from_slice(&bytes[18..24]);
    packet.target_ip.copy_from_slice(&bytes[24..28]);
    Some(packet)
}

// -- IPv4 ----------------------------------------------------------------------

/// Bytes of an IPv4 header with no options, which is the only kind written here.
pub const IPV4_HEADER: usize = 20;

/// Write an IPv4 header over a payload already `length` bytes long, and say how
/// long the header is.
///
/// The identification field is the caller's: it belongs to whatever is
/// fragmenting, and nothing here fragments.
pub fn ipv4(
    into: &mut [u8],
    from: Ipv4,
    to: Ipv4,
    protocol: u8,
    length: usize,
    identification: u16,
) -> usize {
    into[0] = 0x45; // version 4, five 32-bit words of header
    into[1] = 0; // no differentiated services
    put16(into, 2, (IPV4_HEADER + length) as u16);
    put16(into, 4, identification);
    // Don't fragment, offset zero. Nothing here reassembles, so nothing here
    // may be fragmented: a packet that came back in pieces would be dropped,
    // and it is better to be told the packet was too big.
    put16(into, 6, 0x4000);
    into[8] = 64; // time to live
    into[9] = protocol;
    put16(into, 10, 0); // checksum, filled in below
    into[12..16].copy_from_slice(&from);
    into[16..20].copy_from_slice(&to);

    let sum = checksum(&[&into[..IPV4_HEADER]]);
    put16(into, 10, sum);
    IPV4_HEADER
}

/// What an IPv4 packet said.
pub struct Ipv4Packet<'a> {
    pub from: Ipv4,
    pub to: Ipv4,
    pub protocol: u8,
    pub payload: &'a [u8],
}

/// Make sense of an IPv4 packet, or not.
///
/// The header checksum is verified rather than assumed. A card that offered to
/// check it for us was never asked to, so this is the only thing standing
/// between a corrupted header and a reply sent to the wrong machine.
#[must_use]
pub fn parse_ipv4(bytes: &[u8]) -> Option<Ipv4Packet<'_>> {
    if bytes.len() < IPV4_HEADER || bytes[0] >> 4 != 4 {
        return None;
    }
    let header = (bytes[0] & 0x0F) as usize * 4;
    if header < IPV4_HEADER || bytes.len() < header {
        return None;
    }
    if checksum(&[&bytes[..header]]) != 0 {
        return None;
    }

    let total = be16(bytes, 2) as usize;
    // The frame may be padded out to the minimum Ethernet length, so the
    // packet's own idea of its length is what counts -- but a length longer
    // than what arrived is a packet that was cut short.
    if total < header || total > bytes.len() {
        return None;
    }
    // Fragments are dropped, not reassembled. A system that quietly handled the
    // first fragment as though it were the packet would be reading half a
    // message and believing it.
    if be16(bytes, 6) & 0x2000 != 0 || be16(bytes, 6) & 0x1FFF != 0 {
        return None;
    }

    let mut from = [0u8; 4];
    let mut to = [0u8; 4];
    from.copy_from_slice(&bytes[12..16]);
    to.copy_from_slice(&bytes[16..20]);
    Some(Ipv4Packet {
        from,
        to,
        protocol: bytes[9],
        payload: &bytes[header..total],
    })
}

// -- ICMP ----------------------------------------------------------------------

/// What an ICMP message is.
pub mod icmp_type {
    pub const ECHO_REPLY: u8 = 0;
    pub const ECHO_REQUEST: u8 = 8;
}

/// Write an ICMP echo, and say how long it is.
pub fn icmp_echo(
    into: &mut [u8],
    kind: u8,
    identifier: u16,
    sequence: u16,
    payload: &[u8],
) -> usize {
    into[0] = kind;
    into[1] = 0; // code
    put16(into, 2, 0); // checksum, filled in below
    put16(into, 4, identifier);
    put16(into, 6, sequence);
    into[8..8 + payload.len()].copy_from_slice(payload);
    let length = 8 + payload.len();
    let sum = checksum(&[&into[..length]]);
    put16(into, 2, sum);
    length
}

// -- UDP -----------------------------------------------------------------------

/// Bytes of a UDP header.
pub const UDP_HEADER: usize = 8;

/// Write a UDP header over a payload already in place, and say how long the
/// header is.
///
/// The checksum covers a *pseudo header* of the addresses and protocol as well
/// as the datagram. That is the part everyone gets wrong: without it a datagram
/// delivered to the wrong address still checksums correctly.
pub fn udp(into: &mut [u8], from: Ipv4, to: Ipv4, source: u16, destination: u16, length: usize) {
    put16(into, 0, source);
    put16(into, 2, destination);
    put16(into, 4, (UDP_HEADER + length) as u16);
    put16(into, 6, 0);

    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&from);
    pseudo[4..8].copy_from_slice(&to);
    pseudo[8] = 0;
    pseudo[9] = protocol::UDP;
    put16(&mut pseudo, 10, (UDP_HEADER + length) as u16);

    let sum = checksum(&[&pseudo, &into[..UDP_HEADER + length]]);
    // A computed checksum of zero is written as all ones, because zero means
    // "not checksummed" and the two must not be confused.
    put16(into, 6, if sum == 0 { 0xFFFF } else { sum });
}

/// What a UDP datagram said.
pub struct Datagram<'a> {
    /// The port it came from. Read by anything that replies, which so far is
    /// nothing: the DHCP client replies to a fixed port because the protocol
    /// says so, not because of what arrived.
    #[allow(dead_code)]
    pub source: u16,
    pub destination: u16,
    pub payload: &'a [u8],
}

/// Make sense of a UDP datagram, or not.
#[must_use]
pub fn parse_udp(bytes: &[u8]) -> Option<Datagram<'_>> {
    if bytes.len() < UDP_HEADER {
        return None;
    }
    let length = be16(bytes, 4) as usize;
    if length < UDP_HEADER || length > bytes.len() {
        return None;
    }
    Some(Datagram {
        source: be16(bytes, 0),
        destination: be16(bytes, 2),
        payload: &bytes[UDP_HEADER..length],
    })
}

/// The ports a DHCP conversation uses. Fixed by the protocol, and the reason a
/// client can be answered before it has an address: the server broadcasts to a
/// port rather than replying to one.
pub const DHCP_SERVER_PORT: u16 = 67;
pub const DHCP_CLIENT_PORT: u16 = 68;

/// The four bytes that say the options are DHCP's and not BOOTP's.
pub const MAGIC: u32 = 0x6382_5363;

/// Bytes of the fixed part of a DHCP message, before the magic and the options.
pub const DHCP_FIXED: usize = 236;

/// What a DHCP message meant.
#[derive(Clone, Copy)]
pub struct Dhcp {
    pub kind: u8,
    pub offered: Ipv4,
    pub server: Ipv4,
    pub mask: Ipv4,
    pub router: Ipv4,
    pub dns: Ipv4,
    pub lease: u32,
    pub transaction: u32,
}

/// The message types this client sends and understands.
pub mod dhcp_type {
    pub const DISCOVER: u8 = 1;
    pub const OFFER: u8 = 2;
    pub const REQUEST: u8 = 3;
    pub const ACK: u8 = 5;
    pub const NAK: u8 = 6;
}

/// Write one option, and say where the next one goes.
pub fn option(message: &mut [u8], at: usize, code: u8, value: &[u8]) -> usize {
    message[at] = code;
    message[at + 1] = value.len() as u8;
    message[at + 2..at + 2 + value.len()].copy_from_slice(value);
    at + 2 + value.len()
}

/// Make sense of a DHCP message, or not.
pub fn parse_dhcp(bytes: &[u8]) -> Option<Dhcp> {
    if bytes.len() < DHCP_FIXED + 4 || bytes[0] != 2 {
        return None;
    }
    if be32(bytes, DHCP_FIXED) != MAGIC {
        return None;
    }

    let mut message = Dhcp {
        kind: 0,
        offered: [bytes[16], bytes[17], bytes[18], bytes[19]],
        server: UNSPECIFIED,
        mask: UNSPECIFIED,
        router: UNSPECIFIED,
        dns: UNSPECIFIED,
        lease: 0,
        transaction: be32(bytes, 4),
    };

    let mut at = DHCP_FIXED + 4;
    while at < bytes.len() {
        let code = bytes[at];
        if code == 255 {
            break;
        }
        if code == 0 {
            // Padding, which has no length byte.
            at += 1;
            continue;
        }
        if at + 2 > bytes.len() {
            break;
        }
        let length = bytes[at + 1] as usize;
        if at + 2 + length > bytes.len() {
            break;
        }
        let value = &bytes[at + 2..at + 2 + length];
        match (code, length) {
            (53, 1) => message.kind = value[0],
            (54, 4) => message.server.copy_from_slice(value),
            (1, 4) => message.mask.copy_from_slice(value),
            // A router option may list several; the first is the one used.
            (3, l) if l >= 4 => message.router.copy_from_slice(&value[..4]),
            (6, l) if l >= 4 => message.dns.copy_from_slice(&value[..4]),
            (51, 4) => message.lease = be32(value, 0),
            _ => {}
        }
        at += 2 + length;
    }

    Some(message)
}

// -- TCP -----------------------------------------------------------------------

/// Bytes of a TCP header with no options, which is the only kind written here.
pub const TCP_HEADER: usize = 20;

/// The flags in a TCP header.
///
/// Six bits that carry the whole protocol. A segment is not a "SYN packet" or
/// an "ACK packet": it is a segment with some of these set, and more than one
/// of them at once is the normal case -- a handshake's second segment is a SYN
/// and an ACK together.
pub mod tcp_flag {
    pub const FIN: u8 = 1 << 0;
    pub const SYN: u8 = 1 << 1;
    pub const RST: u8 = 1 << 2;
    pub const PSH: u8 = 1 << 3;
    pub const ACK: u8 = 1 << 4;
}

/// What a TCP segment said.
pub struct Segment<'a> {
    pub source: u16,
    pub destination: u16,
    /// Where this segment's first byte sits in the sender's stream.
    pub sequence: u32,
    /// The next byte the sender expects, valid only when `ACK` is set.
    pub acknowledgement: u32,
    pub flags: u8,
    /// How much more the sender is willing to receive.
    pub window: u16,
    pub payload: &'a [u8],
}

impl Segment<'_> {
    /// Whether a flag is set.
    #[must_use]
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// How much of the sequence space this segment occupies.
    ///
    /// Its payload, plus one for a SYN and one for a FIN. Those two are not
    /// data and they are still *numbered*, which is what lets the other side
    /// acknowledge them: an implementation that acknowledged a FIN with the
    /// sequence number it arrived on would be asking for the FIN again for
    /// ever.
    #[must_use]
    pub fn sequence_length(&self) -> u32 {
        self.payload.len() as u32
            + u32::from(self.has(tcp_flag::SYN))
            + u32::from(self.has(tcp_flag::FIN))
    }
}

/// Write a TCP segment over a payload already in place at `TCP_HEADER`, and say
/// how long the whole segment is.
///
/// The checksum covers the same pseudo header UDP's does, for the same reason:
/// without it a segment delivered to the wrong address still checksums.
#[allow(clippy::too_many_arguments)]
pub fn tcp(
    into: &mut [u8],
    from: Ipv4,
    to: Ipv4,
    source: u16,
    destination: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    window: u16,
    length: usize,
) -> usize {
    put16(into, 0, source);
    put16(into, 2, destination);
    put32(into, 4, sequence);
    put32(into, 8, acknowledgement);
    // The high nibble is the header length in 32-bit words. Five, because no
    // options are written; a receiver uses it to find the payload, so a header
    // that lied about its own length would hand over the wrong bytes.
    into[12] = 5 << 4;
    into[13] = flags;
    put16(into, 14, window);
    put16(into, 16, 0);
    put16(into, 18, 0);

    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&from);
    pseudo[4..8].copy_from_slice(&to);
    pseudo[9] = protocol::TCP;
    put16(&mut pseudo, 10, (TCP_HEADER + length) as u16);

    let sum = checksum(&[&pseudo, &into[..TCP_HEADER + length]]);
    put16(into, 16, sum);
    TCP_HEADER + length
}

/// Make sense of a TCP segment, or not.
///
/// The checksum is verified against the addresses the packet actually arrived
/// with, which is the only thing that catches a segment delivered to the wrong
/// machine.
#[must_use]
pub fn parse_tcp<'a>(bytes: &'a [u8], from: Ipv4, to: Ipv4) -> Option<Segment<'a>> {
    if bytes.len() < TCP_HEADER {
        return None;
    }
    let header = (bytes[12] >> 4) as usize * 4;
    if header < TCP_HEADER || bytes.len() < header {
        return None;
    }

    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&from);
    pseudo[4..8].copy_from_slice(&to);
    pseudo[9] = protocol::TCP;
    put16(&mut pseudo, 10, bytes.len() as u16);
    if checksum(&[&pseudo, bytes]) != 0 {
        return None;
    }

    Some(Segment {
        source: be16(bytes, 0),
        destination: be16(bytes, 2),
        sequence: be32(bytes, 4),
        acknowledgement: be32(bytes, 8),
        // Only the six that are defined here; the rest of the byte is reserved
        // and older stacks put nothing in it.
        flags: bytes[13] & 0x3F,
        window: be16(bytes, 14),
        payload: &bytes[header..],
    })
}

/// Whether `a` is at or before `b` in a sequence space that wraps.
///
/// Sequence numbers are 32 bits and they wrap, so `<` is the wrong question:
/// after four gigabytes a connection's numbers start again from zero and a
/// comparison that used ordinary arithmetic would decide every new segment was
/// ancient history. What is meaningful is the *difference*, read as signed.
#[must_use]
pub fn sequence_at_or_before(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_a_known_header() {
        // The worked example from RFC 1071, with the checksum field zeroed.
        let header = [
            0x45u8, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(checksum(&[&header]), 0xb861);
    }

    #[test]
    fn a_checksum_verifies_to_zero() {
        let mut header = [0u8; IPV4_HEADER];
        ipv4(
            &mut header,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            protocol::UDP,
            8,
            1,
        );
        // Summing a header that already carries its checksum gives zero. That
        // is the property a receiver uses, and it is why the field is stored
        // as the complement.
        assert_eq!(checksum(&[&header]), 0);
    }

    #[test]
    fn the_carry_comes_back_around() {
        // Two words that sum past sixteen bits: 0xFFFF + 0x0001 is 0x10000,
        // and the carry has to come back in at the bottom to make 1.
        let bytes = [0xFFu8, 0xFF, 0x00, 0x01];
        assert_eq!(checksum(&[&bytes]), !1u16);
    }

    #[test]
    fn an_odd_length_pads_rather_than_drops() {
        // Three bytes: the last pairs with a zero, so it still contributes.
        assert_ne!(checksum(&[&[0u8, 0, 1]]), checksum(&[&[0u8, 0]]));
    }

    #[test]
    fn a_split_word_spans_two_parts() {
        // The sum is over the concatenation: one byte at the end of the first
        // part pairs with one at the start of the second. A checksum that
        // padded each part separately would give a different answer, and it is
        // the pseudo-header case where that would actually happen.
        assert_eq!(
            checksum(&[&[0x12u8, 0x34, 0x56], &[0x78u8]]),
            checksum(&[&[0x12u8, 0x34, 0x56, 0x78]])
        );
    }

    #[test]
    fn an_arp_reply_survives_the_round_trip() {
        let mut bytes = [0u8; ARP_SIZE];
        arp(
            &mut bytes,
            &Arp {
                operation: arp_op::REPLY,
                sender_mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
                sender_ip: [10, 0, 2, 2],
                target_mac: [0x52, 0x54, 0, 0, 0, 1],
                target_ip: [10, 0, 2, 15],
            },
        );
        let parsed = parse_arp(&bytes).expect("a packet this wrote must parse");
        assert_eq!(parsed.operation, arp_op::REPLY);
        assert_eq!(parsed.sender_ip, [10, 0, 2, 2]);
        assert_eq!(parsed.target_mac, [0x52, 0x54, 0, 0, 0, 1]);
    }

    #[test]
    fn arp_for_another_network_is_refused() {
        let mut bytes = [0u8; ARP_SIZE];
        arp(
            &mut bytes,
            &Arp {
                operation: arp_op::REQUEST,
                sender_mac: [0; 6],
                sender_ip: [0; 4],
                target_mac: [0; 6],
                target_ip: [0; 4],
            },
        );
        // Six-byte hardware addresses are what makes this Ethernet. Claim eight
        // and it is a packet for a network this does not speak.
        bytes[4] = 8;
        assert!(parse_arp(&bytes).is_none());
    }

    #[test]
    fn a_datagram_survives_the_round_trip() {
        let mut packet = [0u8; IPV4_HEADER + UDP_HEADER + 4];
        packet[IPV4_HEADER + UDP_HEADER..].copy_from_slice(b"ping");
        let (header, rest) = packet.split_at_mut(IPV4_HEADER);
        udp(rest, [10, 0, 2, 15], [10, 0, 2, 3], 5353, 53, 4);
        ipv4(
            header,
            [10, 0, 2, 15],
            [10, 0, 2, 3],
            protocol::UDP,
            UDP_HEADER + 4,
            7,
        );

        let parsed = parse_ipv4(&packet).expect("a packet this wrote must parse");
        assert_eq!(parsed.protocol, protocol::UDP);
        assert_eq!(parsed.to, [10, 0, 2, 3]);
        let datagram = parse_udp(parsed.payload).expect("and so must its datagram");
        assert_eq!(datagram.destination, 53);
        assert_eq!(datagram.payload, b"ping");
    }

    #[test]
    fn a_corrupted_header_is_refused() {
        let mut packet = [0u8; IPV4_HEADER];
        ipv4(
            &mut packet,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            protocol::ICMP,
            0,
            1,
        );
        assert!(parse_ipv4(&packet).is_some());
        // One bit, anywhere in the header. Without the checksum this would be a
        // packet delivered to 10.0.2.3.
        packet[19] ^= 1;
        assert!(parse_ipv4(&packet).is_none());
    }

    #[test]
    fn a_fragment_is_dropped_rather_than_half_read() {
        let mut packet = [0u8; IPV4_HEADER + 4];
        ipv4(
            &mut packet,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            protocol::UDP,
            4,
            1,
        );
        // More fragments, and recompute the checksum so that the only reason to
        // reject it is the flag.
        put16(&mut packet, 6, 0x2000);
        put16(&mut packet, 10, 0);
        let sum = checksum(&[&packet[..IPV4_HEADER]]);
        put16(&mut packet, 10, sum);
        assert!(parse_ipv4(&packet).is_none());
    }

    #[test]
    fn a_packet_shorter_than_it_claims_is_refused() {
        let mut packet = [0u8; IPV4_HEADER + 8];
        ipv4(
            &mut packet,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            protocol::UDP,
            8,
            1,
        );
        // Claim a hundred bytes of a twenty-eight byte packet, and checksum it
        // so that the length is the only thing wrong.
        put16(&mut packet, 2, 100);
        put16(&mut packet, 10, 0);
        let sum = checksum(&[&packet[..IPV4_HEADER]]);
        put16(&mut packet, 10, sum);
        assert!(parse_ipv4(&packet).is_none());
    }

    #[test]
    fn an_echo_carries_its_own_checksum() {
        let mut message = [0u8; 8 + 8];
        let length = icmp_echo(
            &mut message,
            icmp_type::ECHO_REQUEST,
            0x1234,
            1,
            b"nexusnet",
        );
        assert_eq!(length, 16);
        assert_eq!(checksum(&[&message[..length]]), 0);
    }

    #[test]
    fn an_offer_is_read_back_out_of_a_real_message() {
        // A message shaped exactly as a server sends one: the fixed part, the
        // magic, and options in the order they usually arrive.
        let mut bytes = [0u8; DHCP_FIXED + 4 + 48];
        bytes[0] = 2; // a reply
        put32(&mut bytes, 4, 0xDEAD_BEEF);
        bytes[16..20].copy_from_slice(&[10, 0, 2, 15]);
        put32(&mut bytes, DHCP_FIXED, MAGIC);

        let mut at = DHCP_FIXED + 4;
        at = option(&mut bytes, at, 53, &[dhcp_type::OFFER]);
        at = option(&mut bytes, at, 1, &[255, 255, 255, 0]);
        at = option(&mut bytes, at, 3, &[10, 0, 2, 2]);
        at = option(&mut bytes, at, 6, &[10, 0, 2, 3]);
        at = option(&mut bytes, at, 51, &86_400u32.to_be_bytes());
        at = option(&mut bytes, at, 54, &[10, 0, 2, 2]);
        bytes[at] = 255;

        let message = parse_dhcp(&bytes).expect("a message this shape must parse");
        assert_eq!(message.kind, dhcp_type::OFFER);
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
        let mut bytes = [0u8; DHCP_FIXED + 4 + 16];
        bytes[0] = 2;
        put32(&mut bytes, DHCP_FIXED, MAGIC);
        let mut at = DHCP_FIXED + 4;
        // A pad byte has no length of its own, so a parser that treated it like
        // any other option would read the next option's code as a length and
        // walk off the end of the message.
        bytes[at] = 0;
        at += 1;
        at = option(&mut bytes, at, 53, &[dhcp_type::ACK]);
        bytes[at] = 255;

        let message = parse_dhcp(&bytes).expect("padding is not an error");
        assert_eq!(message.kind, dhcp_type::ACK);
    }

    #[test]
    fn a_message_without_the_magic_is_not_dhcp() {
        let mut bytes = [0u8; DHCP_FIXED + 4 + 8];
        bytes[0] = 2;
        // A BOOTP reply, which has the same fixed part and no options this
        // client can read. Treating it as DHCP would mean accepting an address
        // from a conversation that never happened.
        put32(&mut bytes, DHCP_FIXED, 0);
        assert!(parse_dhcp(&bytes).is_none());
    }

    #[test]
    fn an_option_running_past_the_end_stops_the_walk() {
        let mut bytes = [0u8; DHCP_FIXED + 4 + 8];
        bytes[0] = 2;
        put32(&mut bytes, DHCP_FIXED, MAGIC);
        let at = DHCP_FIXED + 4;
        bytes[at] = 6;
        // A length longer than what is left. A parser that trusted it would
        // read past the message; this one stops and reports what it has.
        bytes[at + 1] = 200;
        let message = parse_dhcp(&bytes).expect("a truncated option is not a broken message");
        assert_eq!(message.dns, UNSPECIFIED);
    }

    #[test]
    fn a_request_this_client_sends_can_be_read_back() {
        // Not sent -- there is no card in a host test. Built the same way, and
        // parsed by the same parser a server would use for the option walk.
        let mut message = [0u8; 300];
        message[0] = 2; // pretend it is a reply, so the parser will read it
        put32(&mut message, 4, 0x0102_0304);
        put32(&mut message, DHCP_FIXED, MAGIC);
        let mut at = DHCP_FIXED + 4;
        at = option(&mut message, at, 53, &[dhcp_type::REQUEST]);
        at = option(&mut message, at, 50, &[10, 0, 2, 15]);
        at = option(&mut message, at, 54, &[10, 0, 2, 2]);
        message[at] = 255;

        let parsed = parse_dhcp(&message).expect("what this writes, it can read");
        assert_eq!(parsed.kind, dhcp_type::REQUEST);
        assert_eq!(parsed.server, [10, 0, 2, 2]);
    }

    #[test]
    fn a_segment_survives_the_round_trip() {
        let mut segment = [0u8; TCP_HEADER + 5];
        segment[TCP_HEADER..].copy_from_slice(b"hello");
        tcp(
            &mut segment,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            80,
            42000,
            0x1000_0000,
            0x2000_0000,
            tcp_flag::ACK | tcp_flag::PSH,
            8192,
            5,
        );

        let parsed = parse_tcp(&segment, [10, 0, 2, 15], [10, 0, 2, 2])
            .expect("a segment this wrote must parse");
        assert_eq!(parsed.source, 80);
        assert_eq!(parsed.destination, 42000);
        assert_eq!(parsed.sequence, 0x1000_0000);
        assert_eq!(parsed.acknowledgement, 0x2000_0000);
        assert!(parsed.has(tcp_flag::ACK));
        assert!(parsed.has(tcp_flag::PSH));
        assert!(!parsed.has(tcp_flag::SYN));
        assert_eq!(parsed.payload, b"hello");
        assert_eq!(parsed.window, 8192);
    }

    #[test]
    fn a_segment_delivered_to_the_wrong_address_is_refused() {
        let mut segment = [0u8; TCP_HEADER];
        tcp(
            &mut segment,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            80,
            42000,
            1,
            0,
            tcp_flag::SYN,
            8192,
            0,
        );
        // Same bytes, different addresses. The pseudo header is what makes the
        // checksum notice: without it this would parse perfectly.
        assert!(parse_tcp(&segment, [10, 0, 2, 15], [10, 0, 2, 3]).is_none());
    }

    #[test]
    fn a_syn_and_a_fin_each_take_a_sequence_number() {
        let mut segment = [0u8; TCP_HEADER];
        tcp(
            &mut segment,
            [10, 0, 2, 15],
            [10, 0, 2, 2],
            80,
            42000,
            1,
            0,
            tcp_flag::SYN,
            8192,
            0,
        );
        let parsed = parse_tcp(&segment, [10, 0, 2, 15], [10, 0, 2, 2]).expect("parses");
        // No payload, and still one byte of sequence space -- which is what the
        // other side acknowledges.
        assert_eq!(parsed.payload.len(), 0);
        assert_eq!(parsed.sequence_length(), 1);
    }

    #[test]
    fn a_header_with_options_finds_its_payload() {
        // A real SYN from a real client carries options: maximum segment size,
        // window scale, timestamps. They are not understood here, but the
        // header length has to be honoured or the payload starts in the middle
        // of them.
        let mut segment = [0u8; 24 + 3];
        segment[12] = 6 << 4; // six words: twenty bytes plus four of options
        segment[13] = tcp_flag::ACK;
        segment[24..].copy_from_slice(b"abc");
        // Checksum it by hand, over the whole thing.
        let mut pseudo = [0u8; 12];
        pseudo[0..4].copy_from_slice(&[10, 0, 2, 2]);
        pseudo[4..8].copy_from_slice(&[10, 0, 2, 15]);
        pseudo[9] = protocol::TCP;
        put16(&mut pseudo, 10, segment.len() as u16);
        let sum = checksum(&[&pseudo, &segment]);
        put16(&mut segment, 16, sum);

        let parsed =
            parse_tcp(&segment, [10, 0, 2, 2], [10, 0, 2, 15]).expect("options are not an error");
        assert_eq!(parsed.payload, b"abc");
    }

    #[test]
    fn sequence_numbers_compare_across_the_wrap() {
        assert!(sequence_at_or_before(1, 2));
        assert!(sequence_at_or_before(2, 2));
        assert!(!sequence_at_or_before(3, 2));
        // The whole point: just before the wrap is *earlier* than just after,
        // even though the number is larger.
        assert!(sequence_at_or_before(0xFFFF_FFFF, 1));
        assert!(!sequence_at_or_before(1, 0xFFFF_FFFF));
    }
}
