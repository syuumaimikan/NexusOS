//! Working out what is on a network, and what it is running.
//!
//! # What this is for
//!
//! Finding out what a machine is offering, from the machine itself. That is the
//! first thing anybody wants when a network is not doing what they expected:
//! *is the port open, is something listening, and what is it?* Guessing at it
//! with a browser tells you about one port and one protocol.
//!
//! # What is in here and what is not
//!
//! **Only the deciding.** Nothing here opens a connection or reads a byte from
//! one. The program does that -- `user/nexus-netool` -- and hands the results
//! to these functions, which say what they mean.
//!
//! That split is not a testing trick, and in particular it is not a fake
//! network: there is no pretend socket anywhere in this crate. It is that
//! "a connection that has been in `Connecting` for four seconds is a port
//! nothing answered for" is a *judgement*, and judgements are worth writing
//! down separately from the I/O that produced their inputs, because they are
//! where the mistakes are.
//!
//! # Connect scanning, and its one honest limitation
//!
//! This completes the handshake. It is not a half-open scan, because a
//! half-open scan needs to send a bare SYN and abandon it, and this system's
//! network service offers connections rather than packets.
//!
//! So every open port this finds has had a real connection made to it and
//! closed again, which anything listening will have noticed and very possibly
//! logged. That is stated here rather than discovered by somebody wondering why
//! their own server logged them.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(test)]
mod tests;

/// Four bytes, as the network service uses them.
pub type Address = [u8; 4];

/// What a port turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding {
    /// Something accepted a connection.
    Open,
    /// Something answered and said no. A machine is there; this port is shut.
    Refused,
    /// Nothing answered at all before the patience ran out.
    ///
    /// Deliberately not called "filtered". A silent port may be behind
    /// something dropping packets, or the machine may not exist, or the network
    /// may have lost it -- and this cannot tell those apart, so it does not
    /// name one of them.
    Silent,
}

impl Finding {
    /// Whether this is evidence that a machine is there at all.
    ///
    /// A refusal is: something has to be running to refuse. Silence is not.
    #[must_use]
    pub const fn proves_a_machine(self) -> bool {
        matches!(self, Self::Open | Self::Refused)
    }
}

/// How the network service's view of a connection is reported to this crate.
///
/// A copy of what `nexus_netclient::State` says rather than a use of it, so
/// that this crate can be tested on the build machine, where the network
/// service does not exist and neither does the handle type its API is written
/// in terms of. The program converts one to the other in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The handshake has not finished.
    Connecting,
    /// Open in both directions.
    Open,
    /// The peer has finished sending; what arrived is still readable.
    PeerDone,
    /// Over.
    Closed,
    /// The service has never heard of it.
    Unknown,
}

/// What to conclude about one port.
///
/// `None` means "not yet" -- keep waiting. The caller polls; this says when to
/// stop.
///
/// # Why a refusal and a timeout are different answers
///
/// A connection that reaches `Closed` without ever being `Open` was refused:
/// the machine sent a reset, which means it is there. A connection still
/// `Connecting` when the patience runs out is silence, which means nothing at
/// all was heard. Collapsing the two into "not open" throws away the only
/// evidence that distinguishes a live machine with a shut port from an address
/// with nothing on it.
#[must_use]
pub fn conclude(progress: Progress, waited: u64, patience: u64) -> Option<Finding> {
    match progress {
        // Open, or opened and already finished -- either way it accepted.
        Progress::Open | Progress::PeerDone => Some(Finding::Open),
        // Closed before it ever opened. Something sent a reset.
        Progress::Closed => Some(Finding::Refused),
        // The service has forgotten it, which is not an answer from the far
        // end. Treated as silence rather than as a refusal, because inventing
        // evidence of a machine is the worse of the two mistakes.
        Progress::Unknown => Some(Finding::Silent),
        Progress::Connecting => (waited >= patience).then_some(Finding::Silent),
    }
}

/// One port, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub number: u16,
    pub finding: Finding,
    /// The first thing it said, if it said anything before being asked.
    pub greeting: Option<String>,
}

/// What is conventionally on a port.
///
/// A small table rather than a large one, on purpose: this exists so that `22`
/// reads as `ssh` at a glance, and a list of nine hundred numbers nobody has
/// checked would be nine hundred chances to be confidently wrong.
///
/// **A name here is a convention, not an observation.** Whatever is actually
/// listening decides what it is, which is what [`identify`] is for.
#[must_use]
pub fn conventional(port: u16) -> Option<&'static str> {
    Some(match port {
        20 | 21 => "ftp",
        22 => "ssh",
        23 => "telnet",
        25 => "smtp",
        53 => "dns",
        67 | 68 => "dhcp",
        69 => "tftp",
        80 => "http",
        110 => "pop3",
        123 => "ntp",
        139 | 445 => "smb",
        143 => "imap",
        161 | 162 => "snmp",
        389 => "ldap",
        443 => "https",
        465 | 587 => "smtp",
        514 => "syslog",
        631 => "ipp",
        993 => "imaps",
        995 => "pop3s",
        1080 => "socks",
        1194 => "openvpn",
        1433 => "mssql",
        1521 => "oracle",
        1883 => "mqtt",
        3306 => "mysql",
        3389 => "rdp",
        5432 => "postgres",
        5060 | 5061 => "sip",
        5900 => "vnc",
        6379 => "redis",
        8080 | 8000 => "http-alt",
        8443 => "https-alt",
        9200 => "elasticsearch",
        27017 => "mongodb",
        _ => return None,
    })
}

/// What a service said about itself, from the first bytes it sent.
///
/// Read from the wire rather than assumed from the port number, which is the
/// whole point: a machine running a web server on 22 is exactly the sort of
/// thing worth finding out, and a table lookup would report `ssh` with total
/// confidence.
///
/// `None` when the bytes do not obviously say. Answering "unknown" is a real
/// answer; guessing from one byte is not.
#[must_use]
pub fn identify(greeting: &[u8]) -> Option<&'static str> {
    // Text protocols that announce themselves on connect.
    let starts = |prefix: &str| {
        greeting.len() >= prefix.len() && &greeting[..prefix.len()] == prefix.as_bytes()
    };

    if starts("SSH-") {
        return Some("ssh");
    }
    if starts("HTTP/") {
        return Some("http");
    }
    if starts("220-") || starts("220 ") {
        // Shared by two: the greeting says which.
        let text = core::str::from_utf8(greeting).unwrap_or("");
        if text.contains("FTP") || text.contains("ftp") {
            return Some("ftp");
        }
        return Some("smtp");
    }
    if starts("+OK") {
        return Some("pop3");
    }
    if starts("* OK") {
        return Some("imap");
    }
    if starts("RFB ") {
        return Some("vnc");
    }
    // TLS: a handshake record, version, then a server hello. Bytes rather than
    // text, and checked as three things rather than one so that a stray 0x16 in
    // a text protocol does not read as TLS.
    if greeting.len() >= 3 && greeting[0] == 0x16 && greeting[1] == 0x03 && greeting[2] <= 0x04 {
        return Some("tls");
    }
    // MySQL: a length-prefixed handshake whose fifth byte is the protocol
    // version, and 10 is the only one in use.
    if greeting.len() >= 5 && greeting[3] == 0 && greeting[4] == 10 {
        return Some("mysql");
    }
    None
}

/// What one machine turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub address: Address,
    pub ports: Vec<Port>,
}

impl Host {
    /// Whether anything at all answered.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.ports
            .iter()
            .any(|port| port.finding.proves_a_machine())
    }

    /// The open ports, in the order they were asked about.
    #[must_use]
    pub fn open(&self) -> Vec<&Port> {
        self.ports
            .iter()
            .filter(|port| port.finding == Finding::Open)
            .collect()
    }
}

/// An address as a person writes it.
#[must_use]
pub fn show(address: Address) -> String {
    format!(
        "{}.{}.{}.{}",
        address[0], address[1], address[2], address[3]
    )
}

/// Read an address a person wrote.
///
/// Strict: four parts, each a number that fits in a byte, nothing else. A
/// parser that accepted `1.2.3` or `1.2.3.4.5` would be a parser that scanned
/// something other than what was typed.
#[must_use]
pub fn address(text: &str) -> Option<Address> {
    let mut out = [0u8; 4];
    let mut parts = 0;
    for (index, part) in text.trim().split('.').enumerate() {
        if index >= 4 || part.is_empty() {
            return None;
        }
        out[index] = part.parse::<u8>().ok()?;
        parts += 1;
    }
    (parts == 4).then_some(out)
}

/// The addresses of a network written as `10.0.2.0/24`, without its ends.
///
/// The first address of a range names the network and the last is its
/// broadcast, and neither is a machine. Scanning them wastes two probes and,
/// worse, a reply to the broadcast would be read as a host that is not there.
///
/// Refused above /16, and that is a real limit rather than caution: /16 is
/// 65,534 addresses and this machine opens connections one at a time. A tool
/// that accepted /8 would accept sixteen million and appear to have hung.
#[must_use]
pub fn network(text: &str) -> Option<Vec<Address>> {
    let (head, tail) = text.trim().split_once('/')?;
    let base = address(head)?;
    let bits: u32 = tail.parse().ok()?;
    if !(16..=32).contains(&bits) {
        return None;
    }
    let base = u32::from_be_bytes(base);
    let width = 32 - bits;
    let count = 1u64 << width;
    let first = base & !((count - 1) as u32);

    // /31 and /32 have no room for a network and broadcast address, and both
    // are ordinary ways to name one machine.
    let (from, to) = if count <= 2 {
        (0, count)
    } else {
        (1, count - 1)
    };
    Some(
        (from..to)
            .map(|offset| (first.wrapping_add(offset as u32)).to_be_bytes())
            .collect(),
    )
}

/// The ports worth trying when nobody said which.
///
/// Chosen to be short. A scan of every port is sixty-five thousand connections
/// and this machine makes them one at a time; the point of a default is to
/// answer "is there anything here" in a few seconds.
pub const USUAL: &[u16] = &[
    21, 22, 23, 25, 53, 80, 110, 143, 443, 445, 587, 993, 995, 3306, 3389, 5432, 5900, 6379, 8080,
    8443,
];

/// Ports a person wrote, as `22`, `80,443` or `1-1024`.
///
/// Empty means [`USUAL`]. Sorted and deduplicated, because asking the same port
/// twice is two connections and one answer.
#[must_use]
pub fn ports(text: &str) -> Option<Vec<u16>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(USUAL.to_vec());
    }
    let mut out = Vec::new();
    for piece in text.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            return None;
        }
        match piece.split_once('-') {
            Some((low, high)) => {
                let low: u16 = low.trim().parse().ok()?;
                let high: u16 = high.trim().parse().ok()?;
                if low > high || low == 0 {
                    return None;
                }
                out.extend(low..=high);
            }
            None => {
                let one: u16 = piece.parse().ok()?;
                if one == 0 {
                    return None;
                }
                out.push(one);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// One line about a port, for a person to read.
#[must_use]
pub fn line(port: &Port) -> String {
    let what = match (&port.greeting, conventional(port.number)) {
        (Some(greeting), _) => match identify(greeting.as_bytes()) {
            Some(named) => format!("{named}, which it said itself"),
            None => format!("said {:?}", shorten(greeting)),
        },
        (None, Some(name)) => format!("{name} by convention"),
        (None, None) => String::from("nothing known about it"),
    };
    match port.finding {
        Finding::Open => format!("{} open: {what}", port.number),
        Finding::Refused => format!("{} refused", port.number),
        Finding::Silent => format!("{} no answer", port.number),
    }
}

/// A greeting cut to one readable line.
///
/// Banners run to several lines and contain control characters; a window that
/// printed one raw would have its layout decided by whatever is listening on
/// the far end, which is a machine somewhere else.
#[must_use]
pub fn shorten(greeting: &str) -> String {
    let mut out = String::new();
    for character in greeting.chars() {
        if out.chars().count() >= 60 {
            out.push('…');
            break;
        }
        if character == '\r' || character == '\n' {
            break;
        }
        if character.is_control() {
            out.push('·');
        } else {
            out.push(character);
        }
    }
    out
}
