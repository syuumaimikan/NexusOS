//! TCP: connections, and a service that answers them.
//!
//! Enough TCP to be talked to. A machine on the other side of the link opens a
//! connection, sends a request, gets an answer and closes — with a real
//! three-way handshake, real sequence numbers, real acknowledgements and a real
//! four-way close. Nothing here is simulated; the other side is an ordinary TCP
//! implementation that has no idea what it is talking to.
//!
//! # What is deliberately not here
//!
//! **Active open.** This end never starts a connection. A client needs a source
//! port allocator, a SYN retransmission timer and a connect timeout, and none
//! of them is needed to answer somebody.
//!
//! **A send queue and congestion control.** One segment of response is prepared
//! and sent, and it is small. There is no window arithmetic beyond honouring
//! the peer's, no slow start, no Nagle, and no coalescing.
//!
//! **Out-of-order reassembly.** A segment that arrives ahead of a gap is
//! dropped rather than held, so the peer resends it in order. That is correct
//! and slow, which on a link where nothing is reordered costs nothing.
//!
//! What *is* here is retransmission, because without it this would not be TCP
//! at all: a response whose acknowledgement does not come back is sent again,
//! and a connection that never gets one is reset rather than left open.
//!
//! # One thread
//!
//! Everything runs on the network thread, so there are no locks inside a
//! connection: segments arrive one at a time and are finished with before the
//! next is looked at.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use nexus_net as wire;
use nexus_net::{tcp_flag, Ipv4};

use crate::sync::IrqSpinLock;
use crate::{arch, kprintln, sched};

/// The port the service listens on.
///
/// Eighty, because what answers is an HTTP response and the thing on the other
/// end is likeliest to be something that speaks it.
pub const PORT: u16 = 80;

/// How much this end is willing to receive at once.
///
/// One buffer's worth. A window larger than the buffer behind it is a promise
/// that cannot be kept, and the peer is entitled to send exactly this much
/// before waiting.
const WINDOW: u16 = 4096;

/// The most a request may be before it is refused.
const MAX_REQUEST: usize = 2048;

/// How long to wait for an acknowledgement before sending again.
const RETRANSMIT_MS: u64 = 250;
/// How many times, before the connection is given up on.
const RETRANSMITS: u32 = 4;

/// Where a connection has got to.
///
/// The states this end can be in, which is fewer than TCP defines because this
/// end never opens a connection: there is no SYN-SENT and no simultaneous open.
/// There is no `Closed`: a connection that is closed is not there at all, and
/// the one slot is an `Option`. A state meaning "exists but is not a
/// connection" would be a second way to say the same thing, and two ways to say
/// it is how they come to disagree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// A SYN arrived and was answered; waiting for the handshake's third part.
    SynReceived,
    /// Open in both directions.
    Established,
    /// The peer has finished sending. This end may still send.
    CloseWait,
    /// This end has sent its own FIN and is waiting for it to be acknowledged.
    LastAck,
}

/// One connection.
#[derive(Clone)]
struct Connection {
    state: State,
    peer: Ipv4,
    peer_port: u16,
    /// The next sequence number this end will send.
    send_next: u32,
    /// The oldest byte this end has sent that has not been acknowledged.
    send_unacknowledged: u32,
    /// The next sequence number this end expects to receive.
    receive_next: u32,
    /// How much the peer says it can take.
    peer_window: u16,
    /// What the peer has sent so far.
    request: Vec<u8>,
    /// Whether the request has been answered.
    answered: bool,
}

/// The one connection this end will hold at a time.
///
/// One rather than a table, because there is one service and it answers in a
/// single exchange. A second SYN while one is open is refused with a reset,
/// which is a truthful answer -- the alternative, dropping it, leaves the other
/// side retrying against a machine that will never explain itself.
static CONNECTION: IrqSpinLock<Option<Connection>> = IrqSpinLock::new(None);

/// Connections accepted, requests answered, and resets sent, for the monitor.
static ACCEPTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static ANSWERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RESETS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RETRIED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Say that the service is open.
pub fn listen() {
    kprintln!("[net ] listening on TCP port {PORT}");
}

/// Connections accepted, requests answered, resets sent, segments resent.
#[must_use]
pub fn statistics() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        ACCEPTED.load(Ordering::Relaxed),
        ANSWERED.load(Ordering::Relaxed),
        RESETS.load(Ordering::Relaxed),
        RETRIED.load(Ordering::Relaxed),
    )
}

/// One TCP segment, from `from` to this machine.
pub fn receive(from: Ipv4, to: Ipv4, bytes: &[u8]) {
    let Some(segment) = wire::parse_tcp(bytes, from, to) else {
        return;
    };
    if segment.destination != PORT {
        // Nothing is listening there. A reset says so, which is what lets the
        // other side fail at once rather than time out.
        if !segment.has(tcp_flag::RST) {
            reset(from, &segment);
        }
        return;
    }

    let mut guard = CONNECTION.lock();

    // A reset ends whatever is there, and is never answered: a reset answering
    // a reset is two machines shouting at each other for ever.
    if segment.has(tcp_flag::RST) {
        if let Some(connection) = guard.as_ref() {
            if connection.peer == from && connection.peer_port == segment.source {
                kprintln!("[net ] the peer reset the connection");
                *guard = None;
            }
        }
        return;
    }

    // A new connection.
    if segment.has(tcp_flag::SYN) && !segment.has(tcp_flag::ACK) {
        if guard.is_some() {
            drop(guard);
            reset(from, &segment);
            return;
        }

        // The initial sequence number. Not zero and not predictable from the
        // outside: a connection whose numbers a stranger can guess is one they
        // can inject data into.
        let initial = (arch::time::uptime_ms() as u32)
            .wrapping_mul(2_654_435_761)
            .wrapping_add(u32::from(segment.source) << 16);

        let connection = Connection {
            state: State::SynReceived,
            peer: from,
            peer_port: segment.source,
            send_next: initial.wrapping_add(1),
            send_unacknowledged: initial,
            // The SYN itself occupies one sequence number, so what is expected
            // next is one past it.
            receive_next: segment.sequence.wrapping_add(1),
            peer_window: segment.window,
            request: Vec::new(),
            answered: false,
        };
        *guard = Some(connection);
        drop(guard);

        send(
            from,
            segment.source,
            initial,
            segment.sequence.wrapping_add(1),
            tcp_flag::SYN | tcp_flag::ACK,
            &[],
        );
        ACCEPTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }

    let Some(connection) = guard.as_mut() else {
        drop(guard);
        reset(from, &segment);
        return;
    };
    if connection.peer != from || connection.peer_port != segment.source {
        drop(guard);
        reset(from, &segment);
        return;
    }

    if segment.has(tcp_flag::ACK) {
        // Only forwards. An acknowledgement for something already acknowledged
        // is a duplicate, and taking it backwards would undo real progress.
        if wire::sequence_at_or_before(connection.send_unacknowledged, segment.acknowledgement)
            && wire::sequence_at_or_before(segment.acknowledgement, connection.send_next)
        {
            connection.send_unacknowledged = segment.acknowledgement;
        }
    }
    connection.peer_window = segment.window;

    // The handshake's third part. Until it arrives this end has a half-open
    // connection and must not treat anything on it as real: a SYN can be forged
    // from any address, and the acknowledgement of this end's own sequence
    // number is what proves the peer is where it claimed to be.
    if connection.state == State::SynReceived {
        if !segment.has(tcp_flag::ACK) || connection.send_unacknowledged != connection.send_next {
            return;
        }
        connection.state = State::Established;
    }

    // Data, but only the piece that begins exactly where this end is up to.
    // Anything ahead of that is dropped rather than held: the peer will send it
    // again in order, which on a link that does not reorder never happens.
    if !segment.payload.is_empty() {
        if segment.sequence == connection.receive_next {
            let room = MAX_REQUEST.saturating_sub(connection.request.len());
            let taken = segment.payload.len().min(room);
            connection
                .request
                .extend_from_slice(&segment.payload[..taken]);
            connection.receive_next = connection.receive_next.wrapping_add(taken as u32);
        }
        let (peer, port, next, expect) = (
            connection.peer,
            connection.peer_port,
            connection.send_next,
            connection.receive_next,
        );
        drop(guard);
        // Acknowledged straight away rather than with the response, because
        // building the response reads the whole system's state and the peer
        // should not be waiting on that to know its bytes arrived.
        send(peer, port, next, expect, tcp_flag::ACK, &[]);
        guard = CONNECTION.lock();
    }

    // Their FIN: they have finished sending. This end may still answer.
    let Some(connection) = guard.as_mut() else {
        return;
    };
    if segment.has(tcp_flag::FIN)
        && segment.sequence.wrapping_add(segment.payload.len() as u32) == connection.receive_next
    {
        connection.receive_next = connection.receive_next.wrapping_add(1);
        if connection.state == State::Established {
            connection.state = State::CloseWait;
        }
        let (peer, port, next, expect) = (
            connection.peer,
            connection.peer_port,
            connection.send_next,
            connection.receive_next,
        );
        drop(guard);
        send(peer, port, next, expect, tcp_flag::ACK, &[]);
        guard = CONNECTION.lock();
    }

    let Some(connection) = guard.as_mut() else {
        return;
    };

    // Answer, once there is something to answer and it has not been answered.
    let ready = !connection.answered
        && matches!(connection.state, State::Established | State::CloseWait)
        && complete(&connection.request);
    if ready {
        connection.answered = true;
        let peer = connection.peer;
        let port = connection.peer_port;
        let request = connection.request.clone();
        let mut sequence = connection.send_next;
        let expect = connection.receive_next;
        drop(guard);

        let body = respond(&request);
        let bytes = body.as_bytes();

        // The response's sequence space is claimed *before* it is sent, not
        // after it is acknowledged. An acknowledgement is only accepted if it
        // falls at or before what this end has sent -- so a connection that
        // sent bytes without saying so would reject the very acknowledgement it
        // was waiting for, resend four times, and give up on a peer that had
        // answered immediately every time. Which is exactly what it did.
        {
            let mut guard = CONNECTION.lock();
            if let Some(connection) = guard.as_mut() {
                connection.send_next = sequence.wrapping_add(bytes.len() as u32);
            }
        }

        if deliver(peer, port, sequence, expect, bytes) {
            sequence = sequence.wrapping_add(bytes.len() as u32);
            ANSWERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

            // And then this end is finished too.
            send(
                peer,
                port,
                sequence,
                expect,
                tcp_flag::FIN | tcp_flag::ACK,
                &[],
            );
            let mut guard = CONNECTION.lock();
            if let Some(connection) = guard.as_mut() {
                connection.send_next = sequence.wrapping_add(1);
                connection.state = State::LastAck;
            }
        } else {
            // Nothing came back after every retry. The connection is gone
            // whether or not the peer knows it, and saying so is what stops
            // this end holding it open for ever.
            kprintln!("[net ] the peer stopped acknowledging; the connection is closed");
            reset_open(peer, port, sequence);
            *CONNECTION.lock() = None;
        }
        return;
    }

    // The last acknowledgement, after which there is nothing left.
    if connection.state == State::LastAck && connection.send_unacknowledged == connection.send_next
    {
        *guard = None;
        kprintln!("[net ] the connection closed cleanly");
    }
}

/// Whether the peer has said enough for this end to answer.
///
/// A blank line ends an HTTP request's headers. Answering before it arrives
/// would be answering half a request, and a peer that sends its request in two
/// segments is entirely ordinary.
fn complete(request: &[u8]) -> bool {
    request.windows(4).any(|window| window == b"\r\n\r\n")
        || request.windows(2).any(|window| window == b"\n\n")
        || request.len() >= MAX_REQUEST
}

/// Send a segment and wait for it to be acknowledged, resending if it is not.
///
/// This is what makes it TCP rather than a datagram with extra headers. The
/// waiting is done by pumping the card, because the acknowledgement is a frame
/// that has to be read and parsed before it counts -- and the thread doing the
/// waiting is the only thread that reads frames.
fn deliver(peer: Ipv4, port: u16, sequence: u32, expect: u32, body: &[u8]) -> bool {
    let wanted = sequence.wrapping_add(body.len() as u32);

    for attempt in 0..RETRANSMITS {
        if attempt > 0 {
            RETRIED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        send(
            peer,
            port,
            sequence,
            expect,
            tcp_flag::PSH | tcp_flag::ACK,
            body,
        );

        let deadline = arch::time::uptime_ms() + RETRANSMIT_MS;
        while arch::time::uptime_ms() < deadline {
            super::drain();
            {
                let guard = CONNECTION.lock();
                let Some(connection) = guard.as_ref() else {
                    // The peer reset it while this was waiting.
                    return false;
                };
                if wire::sequence_at_or_before(wanted, connection.send_unacknowledged) {
                    return true;
                }
            }
            sched::sleep_ms(5);
        }
    }
    false
}

/// What to say back.
///
/// A real answer about a real machine: what it is, how long it has been up,
/// what address it is answering from, and how many processes it is running.
/// Nothing here is a fixed string pretending to be a page -- every number is
/// read when the request arrives, which is what makes an answer that changes
/// between two requests mean something.
fn respond(request: &[u8]) -> String {
    let interface = super::interface();
    let (frames_in, frames_out, arp, echoes, _) = super::statistics();
    let uptime = arch::time::uptime_ms();
    let (processes, ended) = crate::process::statistics();

    // The first line of the request, for the log and for the answer. A server
    // that never looked at what it was asked would answer the same thing to a
    // request it did not understand.
    let line = request
        .split(|byte| *byte == b'\r' || *byte == b'\n')
        .next()
        .unwrap_or(&[]);
    let line = core::str::from_utf8(line).unwrap_or("<not text>");
    kprintln!("[net ] a request arrived: {line}");

    let body = format!(
        "NexusOS\r\n\
         =======\r\n\
         \r\n\
         you asked   : {line}\r\n\
         address     : {}.{}.{}.{}/{}\r\n\
         uptime      : {}.{:03} s\r\n\
         processes   : {} started, {} ended\r\n\
         frames      : {} in, {} out\r\n\
         answered    : {} ARP, {} echoes\r\n\
         \r\n\
         This page was built by a kernel that wrote its own TCP.\r\n",
        interface.ip[0],
        interface.ip[1],
        interface.ip[2],
        interface.ip[3],
        interface.mask.iter().map(|b| b.count_ones()).sum::<u32>(),
        uptime / 1000,
        uptime % 1000,
        processes,
        ended,
        frames_in,
        frames_out,
        arp,
        echoes
    );

    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// Send one segment.
fn send(peer: Ipv4, port: u16, sequence: u32, expect: u32, flags: u8, payload: &[u8]) {
    let interface = super::interface();
    let mut segment = [0u8; wire::TCP_HEADER + 1400];
    if payload.len() > 1400 {
        return;
    }
    segment[wire::TCP_HEADER..wire::TCP_HEADER + payload.len()].copy_from_slice(payload);
    let length = wire::tcp(
        &mut segment,
        interface.ip,
        peer,
        PORT,
        port,
        sequence,
        expect,
        flags,
        WINDOW,
        payload.len(),
    );
    super::send_ipv4(peer, wire::protocol::TCP, &segment[..length]).ok();
}

/// Refuse a segment that belongs to no connection.
///
/// The acknowledgement number is what makes a reset believable: a peer checks
/// that a reset is in its window before acting on one, precisely so that a
/// stranger cannot tear down somebody else's connection with a forged packet.
fn reset(peer: Ipv4, segment: &wire::Segment<'_>) {
    RESETS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let interface = super::interface();
    let mut reply = [0u8; wire::TCP_HEADER];
    let length = wire::tcp(
        &mut reply,
        interface.ip,
        peer,
        segment.destination,
        segment.source,
        // A reset to an unacknowledged segment carries the sequence number the
        // peer was expecting; one to an acknowledged segment carries theirs.
        if segment.has(tcp_flag::ACK) {
            segment.acknowledgement
        } else {
            0
        },
        segment.sequence.wrapping_add(segment.sequence_length()),
        tcp_flag::RST | tcp_flag::ACK,
        0,
        0,
    );
    super::send_ipv4(peer, wire::protocol::TCP, &reply[..length]).ok();
}

/// Reset a connection this end is giving up on.
fn reset_open(peer: Ipv4, port: u16, sequence: u32) {
    RESETS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    send(peer, port, sequence, 0, tcp_flag::RST, &[]);
}
