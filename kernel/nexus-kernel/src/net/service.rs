//! The network, as something a program can hold.
//!
//! Everything above this in the system reaches the network through a channel,
//! and holding an end of that channel is the whole of the authority. There is
//! no socket system call and no address a program could name instead: a program
//! that was never handed this cannot open a connection, cannot send a datagram,
//! and cannot find out what this machine's address is. That is the same
//! argument as for the spawn service and the filesystem, applied to the wire.
//!
//! # Request and reply, and nothing else
//!
//! Every message from a program is answered with exactly one message, tagged
//! with four bytes. Nothing is pushed the other way.
//!
//! That is a deliberate choice against the obvious alternative, which is for
//! the kernel to send data up as it arrives. Pushing is better for a program
//! that is otherwise idle and worse for everything else: the replies a program
//! is waiting for and the data it has not asked for arrive on the same queue,
//! in an order neither end controls, so every caller has to be written to
//! handle a push arriving in the middle of a request it has not finished. That
//! is a whole class of bug traded for some latency, and the latency is a
//! twenty-millisecond poll while a fetch is in flight -- which a program with a
//! wait set and a deadline already knows how to do, because that is how it
//! draws a clock.
//!
//! # Which thread
//!
//! The network thread, the same one that reads frames. The stack is written for
//! one thread and this is that thread: a service on its own would have to take
//! the stack's locks from the outside, in an order the receive path does not
//! respect. So the loop drains the card, moves the connections along, and then
//! answers whatever programs have asked -- in that order, because an answer
//! about a connection should be about the connection as it is now.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use nexus_net::Ipv4;

use crate::ipc;
use crate::kprintln;
use crate::sync::IrqSpinLock;

use super::{datagram, stream};

/// The most bytes one `recv` will hand back.
///
/// Bounded by what a channel message carries, less the tag and the identifier.
/// A caller asking for more is given this much and asks again, which is the
/// whole of the flow control between a program and the stack.
pub const MAX_READ: usize = ipc::MAX_MESSAGE - 16;

/// The end of the channel the kernel holds.
///
/// A list, because more than one program may be given the network: the
/// compositor holds one so it can pass it to a browser, and a program that
/// asked for its own would get another. Each is served in turn.
static SERVICES: IrqSpinLock<Vec<Arc<ipc::Endpoint>>> = IrqSpinLock::new(Vec::new());

/// Requests answered and refused, for the monitor.
static ANSWERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Requests answered, requests refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        ANSWERED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// Make a channel to the network, and return the end a program should hold.
///
/// The kernel's end joins a wait set the network thread waits on, so that a
/// request wakes the thread that has to answer it. Without that, a program's
/// first call waits for a frame to arrive from somewhere -- and on a machine
/// nobody is talking to, none does. That was a browser that hung before it had
/// drawn anything, on a machine whose network was working perfectly.
pub fn endpoint() -> Arc<ipc::Endpoint> {
    let (service, client) = ipc::Endpoint::pair();
    let set = waker();
    let key = NEXT_KEY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if set
        .add(key, crate::waitset::Watched::Channel(service.clone()))
        .is_err()
    {
        kprintln!("[net ] no room to watch another network channel");
    }
    SERVICES.lock().push(service);
    client
}

/// The next key in the network thread's wait set.
static NEXT_KEY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The set the network thread waits on.
///
/// Made once, on the first call. A `static` holding an `Arc` cannot be built at
/// compile time, and the alternative -- an `Option` that everything unwraps --
/// is the same thing with more places to get it wrong.
fn waker() -> Arc<crate::waitset::WaitSet> {
    static WAKER: IrqSpinLock<Option<Arc<crate::waitset::WaitSet>>> = IrqSpinLock::new(None);
    let mut guard = WAKER.lock();
    if let Some(set) = guard.as_ref() {
        return set.clone();
    }
    let set = Arc::new(crate::waitset::WaitSet::new());
    *guard = Some(set.clone());
    set
}

/// Wake the network thread, because something it is responsible for happened.
///
/// Called from the card's interrupt handler as well as by the channels, so that
/// one wait covers both: a frame arriving and a program asking are the two
/// things that thread exists for, and it should not have to choose which to
/// wait on.
pub fn signal() {
    waker().signal();
}

/// Wait for a frame or a request, whichever comes first.
///
/// `milliseconds` is how long at most. A machine with nothing on the network
/// and no connection open waits with no deadline at all and costs nothing
/// between frames; one with a connection open wakes often enough to move it
/// along.
///
/// The order below is the whole of the correctness. The set's counter is read
/// *first*, so that anything which happens from here on is a change this sees
/// rather than one it sleeps through; then the two things it is waiting for are
/// tested; then it blocks. Any other order leaves a window where a frame
/// arrives and the thread goes to sleep anyway.
pub fn wait_for_work(milliseconds: Option<u64>) {
    let set = waker();
    let seen = set.change_count();

    if crate::drivers::virtio_net::has_frame_waiting() {
        return;
    }
    if !set.poll().is_empty() {
        return;
    }

    let deadline = milliseconds.map(|milliseconds| {
        crate::arch::time::ticks() + crate::arch::time::ms_to_ticks(milliseconds)
    });
    set.wait_since(seen, deadline);
}

/// Answer everything that has been asked, and return.
///
/// Called from the network thread. Never blocks: a service that waited here
/// would be a network thread that stopped reading frames because nobody had
/// asked it anything, which is a machine whose connections stall whenever its
/// programs are quiet.
pub fn serve() {
    // Cloned out from under the lock, because answering sends on the endpoint
    // and a program could be adding another service at the same moment.
    let services: Vec<Arc<ipc::Endpoint>> = SERVICES.lock().clone();
    let mut gone = false;

    for service in &services {
        // A bound per turn, so that one program asking in a tight loop cannot
        // keep this thread from the card.
        for _ in 0..32 {
            if service.queued() == 0 {
                break;
            }
            let Some(request) = service.try_receive() else {
                break;
            };
            let reply = answer(&request.bytes);
            if service.send(&reply, Vec::new()).is_err() {
                gone = true;
                break;
            }
        }
        if !service.peer_open() {
            gone = true;
        }
    }

    if gone {
        SERVICES.lock().retain(|service| service.peer_open());
    }
}

/// The tags a request can carry, and the ones a reply does.
mod tag {
    /// Start a connection. `ip[4] port[2]`.
    pub const OPEN: &[u8; 4] = b"open";
    /// Queue bytes to go out. `id[4] bytes`.
    pub const SEND: &[u8; 4] = b"send";
    /// Take what has arrived. `id[4] limit[2]`.
    pub const RECV: &[u8; 4] = b"recv";
    /// Finish sending. `id[4]`.
    pub const SHUT: &[u8; 4] = b"shut";
    /// Forget it entirely. `id[4]`.
    pub const DROP: &[u8; 4] = b"drop";
    /// Why a connection ended badly, if it did. `id[4]`.
    pub const WHY: &[u8; 4] = b"why ";
    /// Take a port for datagrams. No payload.
    pub const BIND: &[u8; 4] = b"bind";
    /// Send a datagram. `port[2] ip[4] port[2] bytes`.
    pub const SEND_DATAGRAM: &[u8; 4] = b"sdgm";
    /// Take a datagram that has arrived. `port[2]`.
    pub const READ_DATAGRAM: &[u8; 4] = b"rdgm";
    /// Give a port back. `port[2]`.
    pub const UNBIND: &[u8; 4] = b"unbd";
    /// What this machine's address, gateway and resolver are. No payload.
    pub const WHERE: &[u8; 4] = b"wher";

    /// It worked. What follows depends on what was asked.
    pub const OK: &[u8; 4] = b"ok  ";
    /// It did not, and the rest is why, as text for a person.
    pub const ERROR: &[u8; 4] = b"err!";
}

/// What a connection's state is, on the wire.
///
/// One byte, because a caller needs to know three things about a connection --
/// whether it is still being made, whether it is open, whether it is over --
/// and a caller that had to infer them from what it read would decide a server
/// that paused had finished.
pub mod wire_state {
    pub const CONNECTING: u8 = 0;
    pub const OPEN: u8 = 1;
    /// The peer has finished sending; what arrived is still readable.
    pub const PEER_DONE: u8 = 2;
    /// Over, for whatever reason.
    pub const CLOSED: u8 = 3;
    /// No such connection.
    pub const UNKNOWN: u8 = 4;
}

/// Refuse, with a reason a person can read.
fn refuse(why: &str) -> Vec<u8> {
    REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut reply = Vec::with_capacity(4 + why.len());
    reply.extend_from_slice(tag::ERROR);
    reply.extend_from_slice(why.as_bytes());
    reply
}

/// It worked, with this after the tag.
fn accept(payload: &[u8]) -> Vec<u8> {
    ANSWERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut reply = Vec::with_capacity(4 + payload.len());
    reply.extend_from_slice(tag::OK);
    reply.extend_from_slice(payload);
    reply
}

/// Read a little-endian `u32` out of a request.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// And a `u16`.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

/// And an address.
fn ip_at(bytes: &[u8], at: usize) -> Option<Ipv4> {
    let slice = bytes.get(at..at + 4)?;
    Some([slice[0], slice[1], slice[2], slice[3]])
}

/// Do one request.
fn answer(request: &[u8]) -> Vec<u8> {
    let Some(what) = request.get(..4) else {
        return refuse("a request with no tag");
    };
    let rest = &request[4..];

    match what {
        _ if what == tag::OPEN => {
            let (Some(ip), Some(port)) = (ip_at(rest, 0), u16_at(rest, 4)) else {
                return refuse("open needs an address and a port");
            };
            match stream::open(ip, port) {
                Ok(id) => {
                    kprintln!(
                        "[net ] a program opened a connection to {}.{}.{}.{}:{port} as {id}",
                        ip[0],
                        ip[1],
                        ip[2],
                        ip[3]
                    );
                    accept(&id.to_le_bytes())
                }
                Err(error) => {
                    let text = alloc::format!("{error}");
                    refuse(&text)
                }
            }
        }

        _ if what == tag::SEND => {
            let Some(id) = u32_at(rest, 0) else {
                return refuse("send needs a connection");
            };
            match stream::write(id, &rest[4..]) {
                Some(taken) => accept(&(taken as u32).to_le_bytes()),
                None => refuse("no such connection"),
            }
        }

        _ if what == tag::RECV => {
            let Some(id) = u32_at(rest, 0) else {
                return refuse("recv needs a connection");
            };
            let limit = u16_at(rest, 4).unwrap_or(MAX_READ as u16) as usize;
            let limit = limit.min(MAX_READ);

            let state = match stream::state(id) {
                Some(stream::State::SynSent) => wire_state::CONNECTING,
                Some(stream::State::Established) => wire_state::OPEN,
                Some(stream::State::CloseWait | stream::State::FinWait) => wire_state::PEER_DONE,
                Some(stream::State::Closed) => wire_state::CLOSED,
                None => wire_state::UNKNOWN,
            };
            let bytes = stream::read(id, limit).unwrap_or_default();

            // The state goes with the data, in one answer. A caller that had to
            // ask twice could be told "open" and then handed the last bytes of
            // a connection that closed in between -- and would go back for more
            // from something that had gone.
            let mut payload = Vec::with_capacity(1 + 4 + bytes.len());
            payload.push(state);
            payload.extend_from_slice(&(stream::queued(id) as u32).to_le_bytes());
            payload.extend_from_slice(&bytes);
            accept(&payload)
        }

        _ if what == tag::SHUT => match u32_at(rest, 0) {
            Some(id) => {
                stream::close(id);
                accept(&[])
            }
            None => refuse("shut needs a connection"),
        },

        _ if what == tag::DROP => match u32_at(rest, 0) {
            Some(id) => {
                stream::forget(id);
                accept(&[])
            }
            None => refuse("drop needs a connection"),
        },

        _ if what == tag::WHY => {
            let Some(id) = u32_at(rest, 0) else {
                return refuse("why needs a connection");
            };
            // Empty when it ended the way connections are supposed to. A caller
            // that got a reason for every close would have to decide which
            // reasons were real, and "the peer finished sending" is not a
            // reason to show anybody.
            accept(stream::trouble(id).unwrap_or("").as_bytes())
        }

        _ if what == tag::BIND => match datagram::bind() {
            Some(port) => accept(&port.to_le_bytes()),
            None => refuse("no port is free for datagrams"),
        },

        _ if what == tag::SEND_DATAGRAM => {
            let (Some(from), Some(to), Some(port)) =
                (u16_at(rest, 0), ip_at(rest, 2), u16_at(rest, 6))
            else {
                return refuse("a datagram needs a port, an address and a port");
            };
            match datagram::send(from, to, port, &rest[8..]) {
                Ok(()) => accept(&[]),
                Err(why) => refuse(why),
            }
        }

        _ if what == tag::READ_DATAGRAM => {
            let Some(port) = u16_at(rest, 0) else {
                return refuse("rdgm needs a port");
            };
            match datagram::take(port) {
                Some(Some((from, source, bytes))) => {
                    let mut payload = Vec::with_capacity(6 + bytes.len());
                    payload.extend_from_slice(&from);
                    payload.extend_from_slice(&source.to_le_bytes());
                    payload.extend_from_slice(&bytes);
                    accept(&payload)
                }
                // Bound, but nothing has come. An empty answer rather than a
                // refusal: not yet is not an error, and a caller polling would
                // otherwise see a failure every twenty milliseconds.
                Some(None) => accept(&[]),
                None => refuse("that port is not bound"),
            }
        }

        _ if what == tag::UNBIND => match u16_at(rest, 0) {
            Some(port) => {
                datagram::unbind(port);
                accept(&[])
            }
            None => refuse("unbd needs a port"),
        },

        _ if what == tag::WHERE => {
            let interface = super::interface();
            if !super::is_up() {
                return refuse("this machine has no address yet");
            }
            let mut payload = Vec::with_capacity(12);
            payload.extend_from_slice(&interface.ip);
            payload.extend_from_slice(&interface.gateway);
            payload.extend_from_slice(&interface.dns);
            accept(&payload)
        }

        other => {
            let text = String::from_utf8_lossy(other);
            refuse(&alloc::format!("no such request: {text}"))
        }
    }
}
