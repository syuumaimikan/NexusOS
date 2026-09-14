//! The other side of the kernel's network service.
//!
//! A program that has been handed the network channel holds authority and not
//! much else: what crosses that channel is four-byte tags and little-endian
//! numbers, and writing that out at every call site would be four chances per
//! call to get an offset wrong. So it is written out once, here.
//!
//! Every call is a request and its reply, in that order, with nothing else on
//! the channel in between -- which is a property of the service and not of this
//! code, and the reason this can be a plain function call rather than a state
//! machine.
//!
//! # Where the blocking is
//!
//! Nowhere. `open` returns before the connection is made and `read` returns
//! whatever has arrived, which may be nothing. A caller that wants to wait does
//! so on its own terms -- with a deadline, in a loop it controls, next to
//! whatever else it is waiting for. That is the right way round for a program
//! with a window to keep drawing: a fetch that blocked the thread would be a
//! window that stops repainting while a page loads, and a page load is exactly
//! when somebody is looking at it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

use nexus_user::Handle;

/// An IPv4 address, in the order it is written down.
pub type Address = [u8; 4];

/// The most bytes one [`read`] will bring back.
///
/// The service's limit, less nothing: asking for more is answered with this
/// much anyway, and a caller that sized its buffer from a larger number would
/// find it half empty every time.
pub const MAX_READ: usize = 240;

/// The most bytes one [`send`] should offer at once.
///
/// A message carries the tag, the identifier and the payload, so the payload
/// has to leave room for the first two. Offering more is not an error -- what
/// does not fit is refused as a message that is too long, which is a failure
/// where a short write would have been fine, so this is what callers use.
pub const MAX_SEND: usize = 240;

/// Where a connection has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The handshake has not finished.
    Connecting,
    /// Open in both directions.
    Open,
    /// The peer has finished sending. What arrived is still readable.
    PeerDone,
    /// Over.
    Closed,
    /// The service has never heard of it.
    Unknown,
}

impl State {
    /// Whether anything more can arrive on it.
    #[must_use]
    pub const fn is_over(self) -> bool {
        matches!(self, Self::Closed | Self::Unknown)
    }

    fn from_wire(byte: u8) -> Self {
        match byte {
            0 => Self::Connecting,
            1 => Self::Open,
            2 => Self::PeerDone,
            3 => Self::Closed,
            _ => Self::Unknown,
        }
    }
}

/// What went wrong.
///
/// The service's refusals carry text written for a person, so they are kept as
/// text: a program that turned "this machine has no address yet" into a number
/// and back would have invented an error code for every sentence the kernel can
/// say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The channel is gone, or the reply was not one.
    Unreachable,
    /// The service said no, and this is what it said.
    Refused(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreachable => f.write_str("the network service is not answering"),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

/// What this machine's own addressing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Where {
    pub address: Address,
    pub gateway: Address,
    /// The resolver this machine was told to use. Zeroes if it was told none.
    pub resolver: Address,
}

/// One request, and its reply.
///
/// The reply's tag is checked here rather than by the caller, so that an
/// unexpected message is one failure in one place instead of a wrong answer
/// read as a right one at nine call sites.
fn ask(service: Handle, request: &[u8]) -> Result<Vec<u8>, Error> {
    if nexus_user::send(service, request, &[]).is_err() {
        return Err(Error::Unreachable);
    }
    let mut reply = [0u8; 256];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(service, &mut reply, &mut none) else {
        return Err(Error::Unreachable);
    };
    if received.bytes < 4 {
        return Err(Error::Unreachable);
    }
    match &reply[..4] {
        b"ok  " => Ok(reply[4..received.bytes].to_vec()),
        b"err!" => Err(Error::Refused(
            core::str::from_utf8(&reply[4..received.bytes])
                .unwrap_or("the service refused, unreadably")
                .to_string(),
        )),
        _ => Err(Error::Unreachable),
    }
}

/// Build a request: a tag, then whatever follows it.
fn request(tag: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + parts.iter().map(|part| part.len()).sum::<usize>());
    bytes.extend_from_slice(tag);
    for part in parts {
        bytes.extend_from_slice(part);
    }
    bytes
}

/// What this machine's address, gateway and resolver are.
pub fn interface(service: Handle) -> Result<Where, Error> {
    let reply = ask(service, b"wher")?;
    if reply.len() < 12 {
        return Err(Error::Unreachable);
    }
    Ok(Where {
        address: [reply[0], reply[1], reply[2], reply[3]],
        gateway: [reply[4], reply[5], reply[6], reply[7]],
        resolver: [reply[8], reply[9], reply[10], reply[11]],
    })
}

/// Start a connection. Returns what to call it.
///
/// Returns before it is open: [`read`] says when it is. Connecting takes a
/// round trip at least and an ARP as well on a cold cache, and a call that
/// waited for that would be waiting on the thread that has to draw the window
/// saying so.
pub fn open(service: Handle, to: Address, port: u16) -> Result<u32, Error> {
    let reply = ask(service, &request(b"open", &[&to, &port.to_le_bytes()]))?;
    if reply.len() < 4 {
        return Err(Error::Unreachable);
    }
    Ok(u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]))
}

/// Queue bytes to go out. Returns how many were taken.
///
/// Fewer than offered means the rest should be offered again later, which is
/// the only flow control between a program and the stack.
pub fn send(service: Handle, id: u32, bytes: &[u8]) -> Result<usize, Error> {
    let take = bytes.len().min(MAX_SEND);
    let reply = ask(
        service,
        &request(b"send", &[&id.to_le_bytes(), &bytes[..take]]),
    )?;
    if reply.len() < 4 {
        return Err(Error::Unreachable);
    }
    Ok(u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize)
}

/// Send all of it, however many calls that takes.
///
/// Returns what is left, which is empty when it all went. A caller that gets
/// something back should try again after letting the stack run, not spin: the
/// reason it did not all fit is that the connection is not open yet or the peer
/// has not made room.
pub fn send_all<'a>(service: Handle, id: u32, mut bytes: &'a [u8]) -> Result<&'a [u8], Error> {
    while !bytes.is_empty() {
        let taken = send(service, id, bytes)?;
        if taken == 0 {
            break;
        }
        bytes = &bytes[taken..];
    }
    Ok(bytes)
}

/// What has arrived, and where the connection has got to.
///
/// Both in one answer on purpose. A caller that asked for the state and then
/// for the bytes could be told "open" and handed the last bytes of a connection
/// that closed in between, and would go back for more from something that had
/// gone.
pub fn read(service: Handle, id: u32) -> Result<(State, usize, Vec<u8>), Error> {
    let reply = ask(
        service,
        &request(
            b"recv",
            &[&id.to_le_bytes(), &(MAX_READ as u16).to_le_bytes()],
        ),
    )?;
    if reply.len() < 5 {
        return Err(Error::Unreachable);
    }
    let state = State::from_wire(reply[0]);
    let waiting = u32::from_le_bytes([reply[1], reply[2], reply[3], reply[4]]) as usize;
    Ok((state, waiting, reply[5..].to_vec()))
}

/// Finish sending, once everything queued has gone out.
pub fn shutdown(service: Handle, id: u32) -> Result<(), Error> {
    ask(service, &request(b"shut", &[&id.to_le_bytes()])).map(|_| ())
}

/// Forget the connection entirely.
pub fn close(service: Handle, id: u32) -> Result<(), Error> {
    ask(service, &request(b"drop", &[&id.to_le_bytes()])).map(|_| ())
}

/// Why a connection ended badly, if it did.
///
/// Empty when it ended the way connections are supposed to.
pub fn why(service: Handle, id: u32) -> Result<String, Error> {
    let reply = ask(service, &request(b"why ", &[&id.to_le_bytes()]))?;
    Ok(core::str::from_utf8(&reply).unwrap_or("").to_string())
}

/// Take a port for datagrams.
pub fn bind(service: Handle) -> Result<u16, Error> {
    let reply = ask(service, b"bind")?;
    if reply.len() < 2 {
        return Err(Error::Unreachable);
    }
    Ok(u16::from_le_bytes([reply[0], reply[1]]))
}

/// Give a port back.
pub fn unbind(service: Handle, port: u16) -> Result<(), Error> {
    ask(service, &request(b"unbd", &[&port.to_le_bytes()])).map(|_| ())
}

/// Send one datagram from a port this program holds.
pub fn send_datagram(
    service: Handle,
    from: u16,
    to: Address,
    port: u16,
    payload: &[u8],
) -> Result<(), Error> {
    ask(
        service,
        &request(
            b"sdgm",
            &[&from.to_le_bytes(), &to, &port.to_le_bytes(), payload],
        ),
    )
    .map(|_| ())
}

/// Take a datagram that has arrived on a port this program holds.
///
/// `None` means nothing has come yet, which is not a failure -- a caller
/// polling would otherwise see an error every time round.
#[allow(clippy::type_complexity)]
pub fn read_datagram(service: Handle, port: u16) -> Result<Option<(Address, u16, Vec<u8>)>, Error> {
    let reply = ask(service, &request(b"rdgm", &[&port.to_le_bytes()]))?;
    if reply.is_empty() {
        return Ok(None);
    }
    if reply.len() < 6 {
        return Err(Error::Unreachable);
    }
    let from = [reply[0], reply[1], reply[2], reply[3]];
    let source = u16::from_le_bytes([reply[4], reply[5]]);
    Ok(Some((from, source, reply[6..].to_vec())))
}
