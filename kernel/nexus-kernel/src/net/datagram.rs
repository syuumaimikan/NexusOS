//! UDP, as far as a program can reach it.
//!
//! A port a program has taken, datagrams that arrive on it, and a way to send
//! one. That is the whole of it, because that is the whole of what UDP is: this
//! module adds no ordering, no retransmission and no acknowledgement, and a
//! caller that wants those is writing a protocol on top -- which is what a
//! resolver does, and where the retrying belongs.
//!
//! # Why this exists rather than a resolver in the kernel
//!
//! A browser needs names turned into addresses. The kernel could do it: there
//! is already a DHCP client here, and DNS is a comparable amount of parsing.
//!
//! But which server to ask, how long to cache an answer, what to do when a name
//! does not resolve, whether a name is a name at all or an address typed with
//! dots -- those are policy, and policy in the kernel is policy nothing can
//! replace. So the kernel carries the part that needs the card, which is this,
//! and `shared/nexus-dns` carries the part that needs an opinion.
//!
//! # The ports this will not give out
//!
//! The DHCP client owns 68 and nothing here may take it. Everything a program
//! gets is from the ephemeral range, which is where a client's source port
//! belongs and also where nothing is expected to be listening.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use nexus_net as wire;
use nexus_net::Ipv4;

use crate::sync::IrqSpinLock;

/// How many ports programs may hold at once.
///
/// A bound rather than a policy: each port holds a queue, and a table that grew
/// on request would be unbounded kernel memory driven by a program.
const MAX_PORTS: usize = 8;

/// How many datagrams one port will hold before the oldest is dropped.
///
/// Dropped rather than refused, and the *oldest* rather than the newest. A
/// datagram is a thing that was true a moment ago; a queue that filled up and
/// then refused everything new would hand a resolver the answer to a question
/// it has given up on and none of the answers since.
const MAX_QUEUED: usize = 8;

/// The most one datagram may carry.
///
/// A DNS answer over UDP is 512 bytes by the original rule and more with
/// EDNS(0), which this does not offer -- so a server talking to it will keep
/// inside 512 and truncate rather than exceed. This is larger than that with
/// room to spare, and small enough that eight of them per port is not a memory
/// policy.
const MAX_DATAGRAM: usize = 1500;

/// The range programs' ports come from.
const FIRST_PORT: u16 = 40_000;
const LAST_PORT: u16 = 48_000;

/// One port a program has taken.
struct Bound {
    port: u16,
    /// What has arrived on it: who from, from which port, and the bytes.
    arrived: VecDeque<(Ipv4, u16, Vec<u8>)>,
}

static BOUND: IrqSpinLock<Vec<Bound>> = IrqSpinLock::new(Vec::new());

/// The next port to try.
static NEXT_PORT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(FIRST_PORT as u32);

/// Datagrams sent, received, and dropped because a queue was full.
static SENT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RECEIVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DROPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Sent, received, dropped.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        SENT.load(Ordering::Relaxed),
        RECEIVED.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
    )
}

/// Take a port, and say which.
pub fn bind() -> Option<u16> {
    let mut bound = BOUND.lock();
    if bound.len() >= MAX_PORTS {
        return None;
    }
    for _ in 0..(LAST_PORT - FIRST_PORT) as u32 {
        let next = NEXT_PORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let port = if next > LAST_PORT as u32 {
            NEXT_PORT.store(FIRST_PORT as u32 + 1, core::sync::atomic::Ordering::Relaxed);
            FIRST_PORT
        } else {
            next as u16
        };
        if bound.iter().any(|holder| holder.port == port) {
            continue;
        }
        bound.push(Bound {
            port,
            arrived: VecDeque::new(),
        });
        return Some(port);
    }
    None
}

/// Give a port back, dropping anything still queued on it.
pub fn unbind(port: u16) {
    BOUND.lock().retain(|holder| holder.port != port);
}

/// Send one datagram from a port that has been taken.
pub fn send(from: u16, to: Ipv4, port: u16, payload: &[u8]) -> Result<(), &'static str> {
    if payload.len() > MAX_DATAGRAM {
        return Err("that datagram is too long");
    }
    if !BOUND.lock().iter().any(|holder| holder.port == from) {
        // Refused rather than sent anyway. A program sending from a port it
        // does not hold could answer somebody else's question, and the answer
        // would come back to a queue it cannot read.
        return Err("that port is not bound");
    }
    if !super::is_up() {
        return Err("this machine has no address yet");
    }

    let interface = super::interface();
    let mut packet = alloc::vec![0u8; wire::UDP_HEADER + payload.len()];
    packet[wire::UDP_HEADER..].copy_from_slice(payload);
    wire::udp(
        &mut packet,
        interface.ip,
        to,
        from,
        port,
        wire::UDP_HEADER + payload.len(),
    );
    super::send_ipv4(to, wire::protocol::UDP, &packet)
        .map_err(|_| "that address could not be reached")?;
    SENT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Take the oldest datagram on a port.
///
/// `None` means the port is not bound at all, which is a caller's mistake.
/// `Some(None)` means it is bound and nothing has come, which is not.
#[allow(clippy::type_complexity)]
pub fn take(port: u16) -> Option<Option<(Ipv4, u16, Vec<u8>)>> {
    let mut bound = BOUND.lock();
    let holder = bound.iter_mut().find(|holder| holder.port == port)?;
    Some(holder.arrived.pop_front())
}

/// A datagram arrived for one of this machine's ports.
///
/// Returns whether anybody wanted it, so that the caller can go on to whatever
/// handles the ports the kernel keeps for itself.
pub fn receive(from: Ipv4, source: u16, destination: u16, payload: &[u8]) -> bool {
    let mut bound = BOUND.lock();
    let Some(holder) = bound.iter_mut().find(|holder| holder.port == destination) else {
        return false;
    };
    if payload.len() > MAX_DATAGRAM {
        return true;
    }
    if holder.arrived.len() >= MAX_QUEUED {
        holder.arrived.pop_front();
        DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    holder.arrived.push_back((from, source, payload.to_vec()));
    RECEIVED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    true
}
