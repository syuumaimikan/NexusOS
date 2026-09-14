//! TCP the other way round: connections this machine starts.
//!
//! [`super::tcp`] answers connections. This one makes them, which is a
//! different program with the same wire format. A server knows the peer exists
//! because the peer spoke first; a client has to find that out, and everything
//! here that is not in the server is a consequence of that:
//!
//! * a **source port** has to be chosen, and chosen so that two connections
//!   never share one;
//! * the **SYN has to be retransmitted**, because the first packet to an
//!   address this machine has never spoken to also triggers the ARP that finds
//!   it, and the ARP is the one most likely to be lost;
//! * there has to be a **timeout**, because a connection to an address nothing
//!   answers at must end in a refusal and not in a program waiting for ever.
//!
//! # Several at once
//!
//! A browser fetching a page opens one connection; a browser fetching a page
//! and looking up a name opens two. So this is a table rather than the server's
//! single slot, and a stream is addressed by an identifier the caller keeps
//! rather than by its port -- a port is reused the moment a connection closes
//! and an identifier is not, which is the difference between talking to the
//! wrong connection and being told the one you meant has gone.
//!
//! # One thread, still
//!
//! Everything here runs on the network thread: segments arrive one at a time
//! and are finished with before the next is looked at. The table is behind a
//! lock all the same, because the *service* that programs talk to runs on that
//! thread too and calls in from the other side.
//!
//! # What is deliberately not here
//!
//! No congestion control, no window scaling, no selective acknowledgement, no
//! out-of-order reassembly. A segment ahead of a gap is dropped and the peer
//! sends it again, which is correct and slow. On a link that does not reorder
//! it costs nothing, and inventing the machinery for a link this system has
//! never run on would be inventing the bugs too.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use nexus_net as wire;
use nexus_net::{tcp_flag, Ipv4};

use crate::sync::IrqSpinLock;
use crate::{arch, kprintln};

/// How many connections this machine will have open at once.
///
/// Four: a page and the name lookup that found it, twice over. A bound rather
/// than a policy -- each stream holds two buffers, and a table that grew on
/// request would be unbounded kernel memory driven by a program.
pub const MAX_STREAMS: usize = 4;

/// How much this end is willing to receive at once, and how much it will hold.
///
/// The window *is* the buffer. A window larger than what is behind it is a
/// promise that cannot be kept, and a peer that believed it would have its
/// bytes dropped after being told they were welcome.
const WINDOW: u16 = 8192;

/// The most this end will hold waiting to be sent.
///
/// A request, not a file. Anything larger is a caller that should be sending in
/// pieces, and accepting it would be buffering on the kernel's heap what
/// belongs on the caller's.
const MAX_OUTGOING: usize = 16 * 1024;

/// The largest payload one segment carries.
///
/// Below the usual 1460 so that the segment, its IP header and the ethernet
/// header fit a 1500-byte frame with room to spare. There is no path MTU
/// discovery here; a number that always fits is worth more than a number that
/// is optimal on the link this happens to be on.
const MAX_SEGMENT: usize = 1200;

/// How long to wait for an acknowledgement before sending again.
const RETRANSMIT_MS: u64 = 300;

/// How long a connection may take to establish before it is given up on.
const CONNECT_MS: u64 = 6_000;

/// How long an established connection may go with nothing arriving.
///
/// Generous, because a server thinking about a request is not a server that has
/// gone. It exists so that a peer which vanishes mid-transfer ends the stream
/// rather than leaving the program that asked for it waiting for ever.
const IDLE_MS: u64 = 20_000;

/// The first port this end will use for a connection of its own.
///
/// The ephemeral range, kept well clear of anything that listens.
const FIRST_PORT: u16 = 49_152;
/// And the last, after which it wraps.
const LAST_PORT: u16 = 60_000;

/// Where a connection this end started has got to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// A SYN has gone out; waiting for the answer.
    SynSent,
    /// Open in both directions.
    Established,
    /// The peer has finished sending. This end may still send.
    CloseWait,
    /// This end has sent its FIN and is waiting for the peer to finish.
    FinWait,
    /// Both ends have finished. Whatever arrived is still readable.
    Closed,
}

/// One connection this end started.
struct Stream {
    /// What the caller knows it by. Never reused while the machine is up.
    id: u32,
    state: State,
    peer: Ipv4,
    peer_port: u16,
    local_port: u16,
    /// The next sequence number this end will send.
    send_next: u32,
    /// The oldest byte sent that has not been acknowledged.
    send_unacknowledged: u32,
    /// The next sequence number this end expects.
    receive_next: u32,
    /// How much the peer says it can take.
    peer_window: u16,
    /// Queued to go out, not yet sent.
    outgoing: VecDeque<u8>,
    /// Sent and not yet acknowledged, kept so it can be sent again.
    ///
    /// One segment's worth. This end sends one segment at a time and waits for
    /// it, which is the simplest thing that is still TCP: a window of one is a
    /// slow connection, not a broken one.
    in_flight: Vec<u8>,
    /// Arrived and not yet read by whoever asked for the connection.
    incoming: VecDeque<u8>,
    /// When what is in flight was last sent.
    sent_at: u64,
    /// When anything was last heard from the peer.
    heard_at: u64,
    /// When the connection was asked for.
    opened_at: u64,
    /// Whether the caller has asked for it to be closed.
    closing: bool,
    /// Why it ended, if it ended badly. Reported once and then cleared.
    trouble: Option<&'static str>,
}

impl Stream {
    /// How much room is left in the receive buffer, as a window.
    fn window(&self) -> u16 {
        let used = self.incoming.len().min(WINDOW as usize);
        (WINDOW as usize - used) as u16
    }
}

/// Every connection this end has open.
static STREAMS: IrqSpinLock<Vec<Stream>> = IrqSpinLock::new(Vec::new());

/// The next identifier to hand out, and the next port to try.
static NEXT_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
static NEXT_PORT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(FIRST_PORT as u32);

/// Connections opened, bytes sent, bytes received, connections refused.
static OPENED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SENT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RECEIVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Connections opened, bytes out, bytes in, connections that came to nothing.
#[must_use]
pub fn statistics() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        OPENED.load(Ordering::Relaxed),
        SENT.load(Ordering::Relaxed),
        RECEIVED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// Why a connection could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// There is no address yet, so there is nowhere to answer.
    NoNetwork,
    /// Every slot is in use.
    Busy,
}

impl core::fmt::Display for OpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NoNetwork => "this machine has no address yet",
            Self::Busy => "too many connections are already open",
        })
    }
}

/// A port nothing else is using.
///
/// Walks forward rather than picking at random, and skips anything a live
/// stream holds. Random would be better against an off-path attacker guessing a
/// connection; walking is better at never colliding, and on a machine with four
/// connections the second matters and the first does not yet.
fn free_port(streams: &[Stream]) -> u16 {
    for _ in 0..(LAST_PORT - FIRST_PORT) as u32 {
        let port = NEXT_PORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let port = if port > LAST_PORT as u32 {
            NEXT_PORT.store(FIRST_PORT as u32 + 1, core::sync::atomic::Ordering::Relaxed);
            FIRST_PORT
        } else {
            port as u16
        };
        if !streams.iter().any(|stream| stream.local_port == port) {
            return port;
        }
    }
    FIRST_PORT
}

/// Start a connection, and return what to call it.
///
/// Returns as soon as the SYN is on its way: the connection is not open yet,
/// and [`state`] says when it is. A call that blocked here would block the
/// thread that has to read the answer, which is this one.
pub fn open(peer: Ipv4, port: u16) -> Result<u32, OpenError> {
    if !super::is_up() {
        return Err(OpenError::NoNetwork);
    }

    let mut streams = STREAMS.lock();
    // Finished connections nobody has collected do not hold a slot for ever.
    streams.retain(|stream| {
        stream.state != State::Closed || !stream.incoming.is_empty() || stream.trouble.is_some()
    });
    if streams.len() >= MAX_STREAMS {
        return Err(OpenError::Busy);
    }

    let local_port = free_port(&streams);
    let id = NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let now = arch::time::uptime_ms();

    // The initial sequence number. Not zero and not guessable from outside: a
    // connection whose numbers a stranger can predict is one they can inject
    // into.
    let initial = (now as u32)
        .wrapping_mul(2_654_435_761)
        .wrapping_add(u32::from(local_port) << 13)
        .wrapping_add(id.wrapping_mul(0x9E37_79B9));

    streams.push(Stream {
        id,
        state: State::SynSent,
        peer,
        peer_port: port,
        local_port,
        // The SYN itself occupies one sequence number.
        send_next: initial.wrapping_add(1),
        send_unacknowledged: initial,
        receive_next: 0,
        peer_window: 0,
        outgoing: VecDeque::new(),
        in_flight: Vec::new(),
        incoming: VecDeque::new(),
        sent_at: now,
        heard_at: now,
        opened_at: now,
        closing: false,
        trouble: None,
    });
    drop(streams);

    segment(&Outgoing::bare(
        peer,
        local_port,
        port,
        initial,
        0,
        tcp_flag::SYN,
    ));
    OPENED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    Ok(id)
}

/// Whether any connection needs the clock: one being made, or one with
/// something outstanding.
///
/// What this decides is whether the network thread may wait to be woken or has
/// to wake on its own. A retransmission is not an event anything signals, so a
/// thread that only woke on frames would never resend the frame that was lost.
#[must_use]
pub fn anything_open() -> bool {
    STREAMS
        .lock()
        .iter()
        .any(|stream| stream.state != State::Closed)
}

/// Where a connection has got to, if it is still known.
#[must_use]
pub fn state(id: u32) -> Option<State> {
    STREAMS
        .lock()
        .iter()
        .find(|stream| stream.id == id)
        .map(|stream| stream.state)
}

/// Why a connection ended badly, if it did. Reported once.
pub fn trouble(id: u32) -> Option<&'static str> {
    let mut streams = STREAMS.lock();
    let stream = streams.iter_mut().find(|stream| stream.id == id)?;
    stream.trouble.take()
}

/// Queue bytes to go out.
///
/// Returns how many were taken. A caller that is handed back less than it
/// offered is being told to wait, which is the only flow control there is
/// between a program and this.
pub fn write(id: u32, bytes: &[u8]) -> Option<usize> {
    let mut streams = STREAMS.lock();
    let stream = streams.iter_mut().find(|stream| stream.id == id)?;
    if matches!(stream.state, State::Closed | State::FinWait) || stream.closing {
        return Some(0);
    }
    let room = MAX_OUTGOING.saturating_sub(stream.outgoing.len());
    let taken = bytes.len().min(room);
    stream.outgoing.extend(&bytes[..taken]);
    Some(taken)
}

/// Take up to `limit` bytes that have arrived.
///
/// Reading is what opens the window: the peer is told how much room is left,
/// and a program that stopped reading would stop the transfer rather than make
/// the kernel hold the whole of it.
pub fn read(id: u32, limit: usize) -> Option<Vec<u8>> {
    let mut streams = STREAMS.lock();
    let stream = streams.iter_mut().find(|stream| stream.id == id)?;
    let taken = limit.min(stream.incoming.len());
    let bytes: Vec<u8> = stream.incoming.drain(..taken).collect();
    Some(bytes)
}

/// How much has arrived and not been read.
#[must_use]
pub fn queued(id: u32) -> usize {
    STREAMS
        .lock()
        .iter()
        .find(|stream| stream.id == id)
        .map_or(0, |stream| stream.incoming.len())
}

/// Finish sending, once everything queued has gone.
pub fn close(id: u32) {
    let mut streams = STREAMS.lock();
    if let Some(stream) = streams.iter_mut().find(|stream| stream.id == id) {
        stream.closing = true;
    }
}

/// Forget a connection entirely, resetting it if it is still open.
pub fn forget(id: u32) {
    let mut streams = STREAMS.lock();
    let Some(at) = streams.iter().position(|stream| stream.id == id) else {
        return;
    };
    let stream = streams.remove(at);
    drop(streams);
    if matches!(stream.state, State::Established | State::CloseWait) {
        segment(
            &Outgoing::bare(
                stream.peer,
                stream.local_port,
                stream.peer_port,
                stream.send_next,
                stream.receive_next,
                tcp_flag::RST,
            )
            .with_window(0),
        );
    }
}

/// One segment addressed to a connection this end started.
///
/// Called from the network thread for every TCP segment whose destination port
/// is not the one the server listens on.
pub fn receive(from: Ipv4, segment_bytes: &[u8], to: Ipv4) {
    let Some(parsed) = wire::parse_tcp(segment_bytes, from, to) else {
        return;
    };

    let now = arch::time::uptime_ms();
    let mut streams = STREAMS.lock();
    let Some(stream) = streams.iter_mut().find(|stream| {
        stream.local_port == parsed.destination
            && stream.peer_port == parsed.source
            && stream.peer == from
    }) else {
        // Not ours. Left alone rather than reset: a reset to a segment that
        // belongs to a connection this machine has already forgotten is noise,
        // and one to a forged segment is a reset sent to a stranger.
        return;
    };
    stream.heard_at = now;

    // A reset ends it, and is never answered. What the caller sees is a
    // connection that closed with a reason, which is the difference between
    // "the server refused" and "the server said nothing".
    if parsed.has(tcp_flag::RST) {
        stream.state = State::Closed;
        stream.trouble = Some("the peer refused the connection");
        REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }

    // The answer to this end's SYN. Both flags, and an acknowledgement of
    // exactly the sequence number that went out -- anything else is a segment
    // for a connection this is not.
    if stream.state == State::SynSent {
        if !parsed.has(tcp_flag::SYN) || !parsed.has(tcp_flag::ACK) {
            return;
        }
        if parsed.acknowledgement != stream.send_next {
            return;
        }
        stream.send_unacknowledged = parsed.acknowledgement;
        stream.receive_next = parsed.sequence.wrapping_add(1);
        stream.peer_window = parsed.window;
        stream.state = State::Established;
        let (peer, local, port, next, expect, window) = (
            stream.peer,
            stream.local_port,
            stream.peer_port,
            stream.send_next,
            stream.receive_next,
            stream.window(),
        );
        drop(streams);
        // The handshake's third part.
        segment(
            &Outgoing::bare(peer, local, port, next, expect, tcp_flag::ACK).with_window(window),
        );
        return;
    }

    if parsed.has(tcp_flag::ACK) {
        // Only forwards: an acknowledgement for something already acknowledged
        // is a duplicate, and taking it backwards would undo real progress.
        if wire::sequence_at_or_before(stream.send_unacknowledged, parsed.acknowledgement)
            && wire::sequence_at_or_before(parsed.acknowledgement, stream.send_next)
        {
            let acknowledged = parsed
                .acknowledgement
                .wrapping_sub(stream.send_unacknowledged) as usize;
            stream.send_unacknowledged = parsed.acknowledgement;
            // What has been acknowledged is no longer in flight, so it is not
            // sent again.
            let drop_count = acknowledged.min(stream.in_flight.len());
            stream.in_flight.drain(..drop_count);
        }
    }
    stream.peer_window = parsed.window;

    // Data, but only the piece that begins exactly where this end is up to.
    let mut acknowledge = false;
    if !parsed.payload.is_empty() {
        if parsed.sequence == stream.receive_next {
            let room = (WINDOW as usize).saturating_sub(stream.incoming.len());
            let taken = parsed.payload.len().min(room);
            stream.incoming.extend(&parsed.payload[..taken]);
            stream.receive_next = stream.receive_next.wrapping_add(taken as u32);
            RECEIVED.fetch_add(taken as u64, core::sync::atomic::Ordering::Relaxed);
        }
        // Acknowledged even when nothing was taken, because the peer has to be
        // told the window is shut rather than left to guess from silence.
        acknowledge = true;
    }

    // Their FIN: they have finished sending. Whatever already arrived stays
    // readable -- a page whose last segment came with the FIN is a page, and
    // throwing it away on the grounds that the connection ended would lose
    // exactly the bytes that were asked for.
    if parsed.has(tcp_flag::FIN)
        && parsed
            .sequence
            .wrapping_add(parsed.payload.len() as u32)
            .wrapping_sub(stream.receive_next)
            == 0
    {
        stream.receive_next = stream.receive_next.wrapping_add(1);
        stream.state = match stream.state {
            State::FinWait => State::Closed,
            _ => State::CloseWait,
        };
        acknowledge = true;
    }

    // This end's own FIN being acknowledged.
    if stream.state == State::FinWait && stream.send_unacknowledged == stream.send_next {
        stream.state = State::Closed;
    }

    if acknowledge {
        let (peer, local, port, next, expect, window) = (
            stream.peer,
            stream.local_port,
            stream.peer_port,
            stream.send_next,
            stream.receive_next,
            stream.window(),
        );
        drop(streams);
        segment(
            &Outgoing::bare(peer, local, port, next, expect, tcp_flag::ACK).with_window(window),
        );
    }
}

/// Move every connection along: send, resend, time out.
///
/// Called from the network thread every time round its loop. Everything that
/// happens on a clock rather than on a packet happens here, which is why there
/// is no timer anywhere else in this module.
pub fn poll() {
    let now = arch::time::uptime_ms();

    // What to send, gathered under the lock and sent outside it: sending takes
    // the interface's lock and the ARP table's, and holding this one across
    // them would put three locks in an order nothing else respects.
    let mut sending: Vec<Outgoing> = Vec::new();

    {
        let mut streams = STREAMS.lock();
        for stream in streams.iter_mut() {
            match stream.state {
                State::SynSent => {
                    if now.saturating_sub(stream.opened_at) > CONNECT_MS {
                        stream.state = State::Closed;
                        stream.trouble = Some("nothing answered at that address");
                        REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    // The SYN again. The first packet to an address this
                    // machine has never spoken to is also what triggers the ARP
                    // that finds it, so the first one is very often lost --
                    // resending is not an edge case here, it is the normal path
                    // on a cold cache.
                    if now.saturating_sub(stream.sent_at) >= RETRANSMIT_MS {
                        stream.sent_at = now;
                        sending.push(Outgoing {
                            peer: stream.peer,
                            local: stream.local_port,
                            port: stream.peer_port,
                            sequence: stream.send_unacknowledged,
                            expect: 0,
                            flags: tcp_flag::SYN,
                            window: WINDOW,
                            payload: Vec::new(),
                        });
                    }
                }
                State::Established | State::CloseWait => {
                    if now.saturating_sub(stream.heard_at) > IDLE_MS {
                        stream.state = State::Closed;
                        stream.trouble = Some("the peer stopped answering");
                        continue;
                    }

                    // Something in flight that has not been acknowledged goes
                    // again. This is the whole of the retransmission: one
                    // segment at a time, resent until it is acknowledged or the
                    // connection is given up on.
                    if !stream.in_flight.is_empty() {
                        if now.saturating_sub(stream.sent_at) >= RETRANSMIT_MS {
                            stream.sent_at = now;
                            sending.push(Outgoing {
                                peer: stream.peer,
                                local: stream.local_port,
                                port: stream.peer_port,
                                sequence: stream.send_unacknowledged,
                                expect: stream.receive_next,
                                flags: tcp_flag::PSH | tcp_flag::ACK,
                                window: stream.window(),
                                payload: stream.in_flight.clone(),
                            });
                        }
                        continue;
                    }

                    // Nothing outstanding, so the next piece can go.
                    if !stream.outgoing.is_empty() {
                        let room = MAX_SEGMENT
                            .min(stream.outgoing.len())
                            .min(stream.peer_window.max(1) as usize);
                        let payload: Vec<u8> = stream.outgoing.drain(..room).collect();
                        stream.in_flight = payload.clone();
                        stream.sent_at = now;
                        let sequence = stream.send_next;
                        stream.send_next = stream.send_next.wrapping_add(payload.len() as u32);
                        SENT.fetch_add(payload.len() as u64, core::sync::atomic::Ordering::Relaxed);
                        sending.push(Outgoing {
                            peer: stream.peer,
                            local: stream.local_port,
                            port: stream.peer_port,
                            sequence,
                            expect: stream.receive_next,
                            flags: tcp_flag::PSH | tcp_flag::ACK,
                            window: stream.window(),
                            payload,
                        });
                        continue;
                    }

                    // Everything queued has gone and the caller has finished.
                    // Only now, because a FIN sent with bytes still waiting
                    // would be a request truncated by its own ending.
                    if stream.closing {
                        let sequence = stream.send_next;
                        stream.send_next = stream.send_next.wrapping_add(1);
                        stream.state = match stream.state {
                            State::CloseWait => State::Closed,
                            _ => State::FinWait,
                        };
                        stream.sent_at = now;
                        sending.push(Outgoing {
                            peer: stream.peer,
                            local: stream.local_port,
                            port: stream.peer_port,
                            sequence,
                            expect: stream.receive_next,
                            flags: tcp_flag::FIN | tcp_flag::ACK,
                            window: stream.window(),
                            payload: Vec::new(),
                        });
                    }
                }
                State::FinWait => {
                    if now.saturating_sub(stream.heard_at) > IDLE_MS {
                        stream.state = State::Closed;
                    }
                }
                State::Closed => {}
            }
        }
    }

    for out in &sending {
        segment(out);
    }
}

/// One segment to put on the wire.
///
/// A struct rather than eight arguments, because eight positional arguments of
/// which four are numbers is a call nobody can read -- and two of them can be
/// swapped without the compiler noticing, which is exactly the pair that
/// matters: the sequence number and the acknowledgement.
struct Outgoing {
    peer: Ipv4,
    local: u16,
    port: u16,
    sequence: u32,
    expect: u32,
    flags: u8,
    window: u16,
    payload: Vec<u8>,
}

impl Outgoing {
    /// A segment with nothing in it: a handshake, an acknowledgement, an end.
    fn bare(peer: Ipv4, local: u16, port: u16, sequence: u32, expect: u32, flags: u8) -> Self {
        Self {
            peer,
            local,
            port,
            sequence,
            expect,
            flags,
            window: WINDOW,
            payload: Vec::new(),
        }
    }

    /// The same, saying how much room this end has left.
    fn with_window(mut self, window: u16) -> Self {
        self.window = window;
        self
    }
}

/// Send one segment from one of this end's ports.
fn segment(out: &Outgoing) {
    if out.payload.len() > MAX_SEGMENT {
        return;
    }
    let interface = super::interface();
    let mut bytes = [0u8; wire::TCP_HEADER + MAX_SEGMENT];
    bytes[wire::TCP_HEADER..wire::TCP_HEADER + out.payload.len()].copy_from_slice(&out.payload);
    let length = wire::tcp(
        &mut bytes,
        interface.ip,
        out.peer,
        out.local,
        out.port,
        out.sequence,
        out.expect,
        out.flags,
        out.window,
        out.payload.len(),
    );
    if super::send_ipv4(out.peer, wire::protocol::TCP, &bytes[..length]).is_err() {
        // The address has not been resolved yet, which on the first packet to a
        // new peer is ordinary: the ARP is on its way and the retransmit will
        // find it. Said once at boot rather than per packet, because a log line
        // per lost SYN is a log nobody reads.
        static SAID: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, core::sync::atomic::Ordering::Relaxed) {
            kprintln!("[net ] a segment waited for an address to be resolved");
        }
    }
}
