//! Sockets, for a program built for Linux.
//!
//! One family: `AF_UNIX`. A Unix domain socket is how two programs on one
//! machine talk when neither started the other — a server puts a name in the
//! filesystem, and a client that was started by something else connects to it.
//! That is how X11 works, how Wayland works, how PulseAudio works, and how
//! nearly every desktop service on Linux is reached.
//!
//! `AF_INET` is refused rather than half-answered. This machine has a TCP stack
//! and it is reached through the Nexus network service, which is a different
//! object with a different shape; connecting the two is real work, and a
//! `socket(AF_INET, ...)` that returned a descriptor nothing could be done with
//! would be worse than one that says no.
//!
//! # Where a name lives
//!
//! In the Linux root, like every other path a translated program names. A
//! server binding `/tmp/.X11-unix/X0` puts an entry in this machine's
//! `linux/tmp/.X11-unix/` — not a file, but a name in the same namespace, which
//! is what makes a client's `connect` to that path find it.
//!
//! The table of bound names is here rather than in the filesystem because a
//! listening socket is not a file: it has no contents, it goes away when the
//! program holding it does, and a `read` of it is meaningless. Linux puts a
//! special inode there; this keeps a map, and the difference shows up only in
//! that `ls` does not list one.
//!
//! # What is not here
//!
//! Datagram sockets (`SOCK_DGRAM`), which a few protocols use for a socket that
//! does not need a connection. `SOCK_SEQPACKET`, which is a Nexus channel by
//! another name and would be easy to add when something asks. `shutdown` of one
//! direction only. Credentials (`SO_PEERCRED`), which Wayland compositors use
//! to identify a client and which this machine has no user model to answer.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::ipc;
use crate::socket::{Listener, SocketError, Stream};
use crate::sync::IrqSpinLock;

use super::linux::error;

/// The families a program may ask for.
mod family {
    /// A connection between two programs on this machine.
    pub const UNIX: u64 = 1;
    /// The internet, over the machine's IPv4 stack.
    pub const INET: u64 = 2;
}

/// The kinds of socket, and the flags `socket` and `accept4` accept beside them.
mod kind {
    /// A byte stream. The only kind here.
    pub const STREAM: u64 = 1;
    /// A datagram. Not translated.
    pub const DGRAM: u64 = 2;
    /// A sequence of whole messages. Not translated yet, and closer to what
    /// this system already has than a stream is.
    pub const SEQPACKET: u64 = 5;

    /// Bits a caller may set in the same argument as the kind.
    pub const NONBLOCK: u64 = 0o4000;
    pub const CLOEXEC: u64 = 0o2_000_000;
}

/// Address family not supported.
const EAFNOSUPPORT: u64 = (-97i64) as u64;
/// Socket type not supported.
const EPROTONOSUPPORT: u64 = (-93i64) as u64;
/// The operation is not defined for this kind of descriptor.
const ENOTSOCK: u64 = (-88i64) as u64;
/// Already connected, or already bound.
const EISCONN: u64 = (-106i64) as u64;
/// Not connected.
const ENOTCONN: u64 = (-107i64) as u64;
/// The address is already in use by another listener.
const EADDRINUSE: u64 = (-98i64) as u64;
/// Nothing is listening at that name.
const ECONNREFUSED: u64 = (-111i64) as u64;
/// The socket has not been told to listen.
const EINVAL: u64 = error::EINVAL;
/// There is nothing to accept and the caller asked not to wait.
const EAGAIN: u64 = (-11i64) as u64;
/// No route or resolution for a peer address.
const ENETUNREACH: u64 = (-101i64) as u64;
/// Interrupted.
const EINTR: u64 = (-4i64) as u64;
/// Broken connection.
const EPIPE: u64 = (-32i64) as u64;
/// The path is longer than a `sockaddr_un` holds.
const ENAMETOOLONG: u64 = (-36i64) as u64;
/// The peer did not answer before the TCP connect deadline.
const ETIMEDOUT: u64 = (-110i64) as u64;
/// No datagram port remains available.
const ENOBUFS: u64 = (-105i64) as u64;
/// A nonblocking connection is still being established.
const EINPROGRESS: u64 = (-115i64) as u64;

/// `sockaddr_un` is a two-byte family and a hundred and eight bytes of path.
///
/// The number is not this system's choice: it is what the structure has been
/// since 4.2BSD, and a program that built one is relying on it.
const SUN_PATH: usize = 108;
const SOCKADDR_UN: u64 = 2 + SUN_PATH as u64;

/// The most bytes one send or receive moves.
const MAX_TRANSFER: u64 = 64 * 1024;

/// The most handles one message may carry.
///
/// The same bound a channel message has, for the same reason: a message that
/// could carry any number of them is a message a program can make the kernel
/// allocate for.
const MAX_HANDLES: usize = 8;

/// Every name something is listening at.
///
/// Keyed by the path as a translated program sees it. The listener is held
/// weakly by nothing -- a strong reference here is what keeps a name alive
/// while the program holding it runs, and [`unbind`] is what takes it away.
static BOUND: IrqSpinLock<BTreeMap<String, Arc<Listener>>> = IrqSpinLock::new(BTreeMap::new());

/// Which names each process bound, so they can be taken away when it ends.
///
/// Without this a program that exited while listening would leave its name in
/// the table for ever, and the next program to bind it would be refused by a
/// server that is not running.
static OWNED: IrqSpinLock<BTreeMap<u64, Vec<String>>> = IrqSpinLock::new(BTreeMap::new());

/// Sockets made, and names bound.
static MADE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LISTENING: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Sockets made, and names currently bound.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        MADE.load(core::sync::atomic::Ordering::Relaxed),
        LISTENING.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// Take away every name a process bound. Called when it ends.
pub fn forget(process: u64) {
    let Some(names) = OWNED.lock().remove(&process) else {
        return;
    };
    let mut bound = BOUND.lock();
    for name in names {
        if bound.remove(&name).is_some() {
            LISTENING.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The socket a descriptor names.
fn socket_of(descriptor: u64) -> Result<Arc<crate::socket::Socket>, u64> {
    match super::linux_files::describe(descriptor)? {
        super::linux_files::Descriptor::Other(ipc::Object::Socket(socket)) => Ok(socket),
        _ => Err(ENOTSOCK),
    }
}

/// Read a `sockaddr_un` out of the caller's memory.
///
/// The path is NUL-terminated inside the hundred and eight bytes, except when
/// it is not: a *zero* first byte is Linux's "abstract" namespace, where the
/// name is the remaining bytes and has nothing to do with the filesystem. Both
/// are accepted here and kept apart by the leading NUL staying in the name, so
/// an abstract name can never collide with a path.
fn address_of(pointer: u64, length: u64) -> Result<String, u64> {
    if !(2..=SOCKADDR_UN).contains(&length) {
        return Err(EINVAL);
    }
    let Some((at, _)) = crate::arch::syscall::user_range(pointer, length, SOCKADDR_UN) else {
        return Err(error::EFAULT);
    };
    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps.
    let bytes = unsafe { core::slice::from_raw_parts(at as *const u8, length as usize) };
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if u64::from(family) != family::UNIX {
        return Err(EAFNOSUPPORT);
    }
    let path = &bytes[2..];
    if path.is_empty() {
        // An unnamed socket. Legal, and nothing can connect to it.
        return Err(EINVAL);
    }
    let name: &[u8] = if path[0] == 0 {
        // Abstract: the whole of the rest, leading NUL and all.
        path
    } else {
        let end = path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path.len());
        &path[..end]
    };
    if name.len() > SUN_PATH {
        return Err(ENAMETOOLONG);
    }
    String::from_utf8(Vec::from(name)).map_err(|_| EINVAL)
}

/// Read the fixed sixteen-byte Linux `sockaddr_in` layout.
fn ipv4_address_of(pointer: u64, length: u64) -> Result<(nexus_net::Ipv4, u16), u64> {
    if length < 16 {
        return Err(EINVAL);
    }
    let Some((at, _)) = crate::arch::syscall::user_range(pointer, 16, 16) else {
        return Err(error::EFAULT);
    };
    let bytes = unsafe { core::slice::from_raw_parts(at as *const u8, 16) };
    let address_family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if address_family != family::INET as u16 {
        return Err(EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    Ok(([bytes[4], bytes[5], bytes[6], bytes[7]], port))
}

fn ensure_datagram_bound(
    internet: &crate::socket::InternetSocket,
    requested: u16,
) -> Result<u16, u64> {
    let state = *internet.state.lock();
    let crate::socket::InternetState::Udp { port, .. } = state else {
        return Err(EINVAL);
    };
    if let Some(port) = port {
        return Ok(port);
    }
    let Some(port) = crate::net::datagram::bind_at(requested) else {
        return Err(ENOBUFS);
    };
    let mut state = internet.state.lock();
    let crate::socket::InternetState::Udp { port: slot, .. } = &mut *state else {
        crate::net::datagram::unbind(port);
        return Err(EINVAL);
    };
    if slot.is_none() {
        *slot = Some(port);
        Ok(port)
    } else {
        crate::net::datagram::unbind(port);
        Ok(slot.unwrap_or(port))
    }
}

/// `socket(family, kind, protocol)`.
pub fn socket(family: u64, kind: u64, protocol: u64) -> u64 {
    let bare = kind & !(kind::NONBLOCK | kind::CLOEXEC);
    if family == family::INET {
        let datagram = match bare {
            kind::STREAM if protocol == 0 || protocol == 6 => false,
            kind::DGRAM if protocol == 0 || protocol == 17 => true,
            _ => return EPROTONOSUPPORT,
        };
        let Some(process) = crate::sched::current_process() else {
            return error::ENOSYS;
        };
        MADE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return u64::from(process.handles.insert(
            ipc::Object::Socket(Arc::new(crate::socket::Socket::internet(
                datagram,
                kind & kind::NONBLOCK != 0,
            ))),
            ipc::Rights::ALL,
        ));
    }
    if family != family::UNIX {
        return EAFNOSUPPORT;
    }
    if kind & kind::NONBLOCK != 0 {
        return EPROTONOSUPPORT;
    }
    if bare == kind::DGRAM || bare == kind::SEQPACKET {
        crate::kprintln!("[linux] only a stream socket is translated; answering EPROTONOSUPPORT");
        return EPROTONOSUPPORT;
    }
    if bare != kind::STREAM || protocol != 0 {
        return EPROTONOSUPPORT;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    MADE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    u64::from(process.handles.insert(
        ipc::Object::Socket(Arc::new(crate::socket::Socket::new())),
        ipc::Rights::ALL,
    ))
}

/// `socketpair(family, kind, protocol, fds)`: two sockets already connected.
///
/// The one way to get a connection without a name, and what a program uses when
/// it is about to hand one side to a child. There is no `fork` here yet, so its
/// use is narrower than on Linux -- but it is also the simplest thing that
/// exercises a connection end to end, which is why the test uses it.
pub fn socketpair(family: u64, kind: u64, protocol: u64, fds: u64) -> u64 {
    if family != family::UNIX {
        return EAFNOSUPPORT;
    }
    let bare = kind & !(kind::NONBLOCK | kind::CLOEXEC);
    if bare != kind::STREAM || protocol != 0 {
        return EPROTONOSUPPORT;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Some((pointer, _)) = crate::arch::syscall::user_range_for(&process, fds, 8, 8, true) else {
        return error::EFAULT;
    };

    let (first, second) = Stream::pair();
    let one = crate::socket::Socket::new();
    one.connected(first);
    let other = crate::socket::Socket::new();
    other.connected(second);

    let left = process
        .handles
        .insert(ipc::Object::Socket(Arc::new(one)), ipc::Rights::ALL);
    let right = process
        .handles
        .insert(ipc::Object::Socket(Arc::new(other)), ipc::Rights::ALL);
    MADE.fetch_add(2, core::sync::atomic::Ordering::Relaxed);

    // SAFETY: eight bytes inside the user half, checked above, written as the
    // two `int` the ABI says they are.
    unsafe {
        core::ptr::write_unaligned(pointer as *mut [i32; 2], [left as i32, right as i32]);
    }
    0
}

/// `bind(fd, address, length)`.
pub fn bind(descriptor: u64, address: u64, length: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if let Some(internet) = socket.internet_state() {
        let (ip, port) = match ipv4_address_of(address, length) {
            Ok(address) => address,
            Err(reason) => return reason,
        };
        let interface = crate::net::interface();
        if ip != [0; 4] && ip != [127, 0, 0, 1] && ip != interface.ip {
            return (-99i64) as u64; // EADDRNOTAVAIL
        }
        if !internet.datagram {
            return if matches!(*internet.state.lock(), crate::socket::InternetState::New) {
                0
            } else {
                EINVAL
            };
        }
        return match ensure_datagram_bound(&internet, port) {
            Ok(_) => 0,
            Err(reason) => reason,
        };
    }
    let name = match address_of(address, length) {
        Ok(name) => name,
        Err(reason) => return reason,
    };
    if !socket.is_new() {
        return EISCONN;
    }
    if BOUND.lock().contains_key(&name) {
        return EADDRINUSE;
    }
    if !socket.bind(&name) {
        return EISCONN;
    }
    0
}

/// `listen(fd, backlog)`.
///
/// The backlog is read and bounded rather than honoured exactly: a listener
/// here holds up to [`crate::socket::MAX_BACKLOG`], and a program that asked
/// for more gets that. Linux does the same thing with its own limit.
pub fn listen(descriptor: u64, _backlog: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(name) = socket.bound_to() else {
        // Linux would pick an unnamed address here. Refused instead: a
        // listener nothing can find is a server that will never be connected
        // to, and saying so is better than starting one.
        return EINVAL;
    };
    if socket.listener().is_some() {
        return 0;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    let listener = Listener::new(&name);
    {
        let mut bound = BOUND.lock();
        if bound.contains_key(&name) {
            return EADDRINUSE;
        }
        bound.insert(name.clone(), Arc::clone(&listener));
    }
    if !socket.listen(listener) {
        BOUND.lock().remove(&name);
        return EINVAL;
    }
    OWNED
        .lock()
        .entry(process.id.0)
        .or_default()
        .push(name.clone());
    LISTENING.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::kprintln!("[linux] {} is listening at {name}", process.name.as_str());
    0
}

/// `connect(fd, address, length)`.
pub fn connect(descriptor: u64, address: u64, length: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if let Some(internet) = socket.internet_state() {
        if internet.datagram && length >= 2 {
            let Some((at, _)) = crate::arch::syscall::user_range(address, 2, 2) else {
                return error::EFAULT;
            };
            // SAFETY: two bytes inside the user half, checked above.
            let address_family = unsafe { (at as *const u16).read_unaligned() };
            if address_family == 0 {
                // `AF_UNSPEC` disconnects a UDP socket (used by glibc getaddrinfo
                // destination address selection between candidate checks).
                let mut state = internet.state.lock();
                if let crate::socket::InternetState::Udp { peer: current, .. } = &mut *state {
                    *current = None;
                    return 0;
                }
            }
        }
        let (peer, port) = match ipv4_address_of(address, length) {
            Ok(address) => address,
            Err(reason) => return reason,
        };
        if peer == [0; 4] || (!internet.datagram && port == 0) {
            return EINVAL;
        }
        if internet.datagram {
            if let Err(reason) = ensure_datagram_bound(&internet, 0) {
                return reason;
            }
            let mut state = internet.state.lock();
            if let crate::socket::InternetState::Udp { peer: current, .. } = &mut *state {
                *current = Some((peer, port));
                return 0;
            }
            return EINVAL;
        }
        let current_state = *internet.state.lock();
        let id = match current_state {
            crate::socket::InternetState::Tcp(id) => match crate::net::stream::state(id) {
                Some(crate::net::stream::State::Established) => return 0,
                Some(crate::net::stream::State::SynSent) if internet.is_nonblocking() => {
                    return EINPROGRESS;
                }
                Some(crate::net::stream::State::SynSent) => id,
                Some(crate::net::stream::State::Closed) | None => {
                    let _ = crate::net::stream::trouble(id);
                    return ECONNREFUSED;
                }
                _ => return EISCONN,
            },
            crate::socket::InternetState::New => {
                if !crate::net::is_up() {
                    return (-100i64) as u64; // ENETDOWN
                }
                let id = match crate::net::stream::open(peer, port) {
                    Ok(id) => id,
                    Err(crate::net::stream::OpenError::NoNetwork) => return (-100i64) as u64,
                    Err(crate::net::stream::OpenError::Busy) => return ENOBUFS,
                };
                *internet.state.lock() = crate::socket::InternetState::Tcp(id);
                if internet.is_nonblocking() {
                    return EINPROGRESS;
                }
                id
            }
            _ => return EISCONN,
        };
        let deadline = crate::arch::time::uptime_ms().saturating_add(6_000);
        loop {
            match crate::net::stream::state(id) {
                Some(crate::net::stream::State::Established) => return 0,
                Some(crate::net::stream::State::Closed) | None => {
                    let _ = crate::net::stream::trouble(id);
                    return ECONNREFUSED;
                }
                _ => {}
            }
            if crate::sched::cancelled() {
                crate::net::stream::forget(id);
                *internet.state.lock() = crate::socket::InternetState::New;
                return EINTR;
            }
            if crate::arch::time::uptime_ms() >= deadline {
                crate::net::stream::forget(id);
                *internet.state.lock() = crate::socket::InternetState::New;
                return ETIMEDOUT;
            }
            crate::sched::sleep_ms(10);
        }
    }
    if socket.stream().is_some() {
        return EISCONN;
    }
    let name = match address_of(address, length) {
        Ok(name) => name,
        Err(reason) => return reason,
    };
    let listener = BOUND.lock().get(&name).map(Arc::clone);
    let Some(listener) = listener else {
        // Nothing is listening there. The honest answer, and the one a client
        // is written to handle: it is what a desktop program gets when the
        // compositor is not running.
        return ECONNREFUSED;
    };
    match listener.connect() {
        Ok(stream) => {
            if socket.connected(stream) {
                0
            } else {
                EISCONN
            }
        }
        Err(SocketError::Backlog) => EAGAIN,
        Err(_) => ECONNREFUSED,
    }
}

/// `accept4(fd, address, length, flags)`.
pub fn accept(descriptor: u64, address: u64, length: u64, flags: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(listener) = socket.listener() else {
        return EINVAL;
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    // Validate all outputs before dequeuing the connection or allocating an
    // FD. socklen_t is four bytes at both widths, including at a page boundary.
    let peer = if address != 0 {
        let Some((length_pointer, _)) =
            crate::arch::syscall::user_range_for(&process, length, 4, 4, true)
        else {
            return error::EFAULT;
        };
        let room = unsafe { (length_pointer as *const u32).read_unaligned() }.min(2);
        let output = if room != 0 {
            let Some((output, _)) =
                crate::arch::syscall::user_range_for(&process, address, room as u64, 2, true)
            else {
                return error::EFAULT;
            };
            output
        } else {
            0
        };
        Some((length_pointer, output, room as usize))
    } else {
        None
    };

    let stream = if flags & kind::NONBLOCK != 0 {
        match listener.accept_now() {
            Some(stream) => stream,
            None => return EAGAIN,
        }
    } else {
        match listener.accept() {
            Ok(stream) => stream,
            Err(SocketError::Cancelled) => return EINTR,
            Err(_) => return ECONNREFUSED,
        }
    };

    let accepted = crate::socket::Socket::new();
    accepted.connected(stream);
    let handle = process
        .handles
        .insert(ipc::Object::Socket(Arc::new(accepted)), ipc::Rights::ALL);

    // The peer's address. A connection made through a name has no address of
    // its own, so what goes here is the family and nothing else -- which is
    // what Linux writes for an unnamed peer, and is why a server that wants to
    // know who connected has to ask a different way.
    if let Some((length, output, room)) = peer {
        let family = (family::UNIX as u16).to_le_bytes();
        unsafe {
            if room != 0 {
                core::ptr::copy_nonoverlapping(family.as_ptr(), output as *mut u8, room);
            }
            (length as *mut u32).write_unaligned(2);
        }
    }
    u64::from(handle)
}

/// `read`, `recv` and `recvfrom` on a connected socket.
pub fn receive(descriptor: u64, buffer: u64, count: u64, flags: u64) -> u64 {
    receive_from(descriptor, buffer, count, flags, 0, 0)
}

/// `recvfrom`, including the optional source `sockaddr_in` and `socklen_t`.
pub fn receive_from(
    descriptor: u64,
    buffer: u64,
    count: u64,
    flags: u64,
    address: u64,
    address_length: u64,
) -> u64 {
    /// `MSG_DONTWAIT`: do not block, whatever the socket was opened as.
    const DONTWAIT: u64 = 0x40;

    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let Some((pointer, length)) =
        crate::arch::syscall::user_range_for(&process, buffer, wanted, MAX_TRANSFER, true)
    else {
        return error::EFAULT;
    };

    if let Some(internet) = socket.internet_state() {
        return receive_internet(&internet, pointer, length, flags, address, address_length);
    }
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };

    let bytes = if flags & DONTWAIT != 0 {
        let taken = stream.read_now(length);
        if taken.is_empty() && stream.connected() {
            return EAGAIN;
        }
        taken
    } else {
        match stream.read(length) {
            Ok(bytes) => bytes,
            Err(SocketError::Cancelled) => return EINTR,
            Err(_) => return ENOTCONN,
        }
    };
    // SAFETY: the range was checked to lie inside the user half, and the
    // stream never returns more than it was asked for.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
    }
    bytes.len() as u64
}

/// `write`, `send` and `sendto` on a connected socket.
pub fn send(descriptor: u64, buffer: u64, count: u64, flags: u64) -> u64 {
    send_to(descriptor, buffer, count, flags, 0, 0)
}

/// `sendto`; a zero address means the destination of a connected socket.
pub fn send_to(
    descriptor: u64,
    buffer: u64,
    count: u64,
    flags: u64,
    address: u64,
    address_length: u64,
) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };
    // SAFETY: as in `receive`.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    let held = Vec::from(bytes);
    if let Some(internet) = socket.internet_state() {
        let peer = if address == 0 {
            None
        } else {
            match ipv4_address_of(address, address_length) {
                Ok(peer) => Some(peer),
                Err(reason) => return reason,
            }
        };
        return send_internet(&internet, &held, flags, peer);
    }
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };
    match stream.write(&held) {
        Ok(written) => written as u64,
        Err(SocketError::Cancelled) => EINTR,
        Err(_) => EPIPE,
    }
}

fn send_internet(
    internet: &crate::socket::InternetSocket,
    bytes: &[u8],
    flags: u64,
    destination: Option<(nexus_net::Ipv4, u16)>,
) -> u64 {
    const DONTWAIT: u64 = 0x40;
    if internet.datagram {
        let (source, connected) = match *internet.state.lock() {
            crate::socket::InternetState::Udp { port, peer } => (port, peer),
            _ => return EINVAL,
        };
        let Some((address, port)) = destination.or(connected) else {
            return ENOTCONN;
        };
        let source = match source {
            Some(source) => source,
            None => match ensure_datagram_bound(internet, 0) {
                Ok(source) => source,
                Err(reason) => return reason,
            },
        };
        return crate::net::datagram::send(source, address, port, bytes)
            .map_or_else(|_| ENETUNREACH, |_| bytes.len() as u64);
    }
    let crate::socket::InternetState::Tcp(id) = *internet.state.lock() else {
        return ENOTCONN;
    };
    if bytes.is_empty() {
        return if crate::net::stream::state(id).is_some() {
            0
        } else {
            EPIPE
        };
    }
    loop {
        match crate::net::stream::write(id, bytes) {
            Some(written) if written > 0 => return written as u64,
            Some(_) if flags & DONTWAIT != 0 || internet.is_nonblocking() => return EAGAIN,
            Some(_) if crate::net::stream::state(id).is_some() => {}
            _ => return EPIPE,
        }
        if crate::sched::cancelled() {
            return EINTR;
        }
        crate::sched::sleep_ms(5);
    }
}

type PeerAddress = (nexus_net::Ipv4, u16);

fn receive_internet_vec(
    internet: &crate::socket::InternetSocket,
    limit: usize,
    flags: u64,
) -> Result<(Vec<u8>, Option<PeerAddress>), u64> {
    const DONTWAIT: u64 = 0x40;
    if internet.datagram {
        let crate::socket::InternetState::Udp {
            port: Some(port),
            peer,
        } = *internet.state.lock()
        else {
            return Err(EINVAL);
        };
        loop {
            if let Some(Some((from, source, mut bytes))) = crate::net::datagram::take(port) {
                if peer.is_some_and(|(peer_ip, peer_port)| {
                    peer_ip != from || (peer_port != 0 && peer_port != source)
                }) {
                    continue;
                }
                bytes.truncate(limit);
                return Ok((bytes, Some((from, source))));
            }
            if flags & DONTWAIT != 0 || internet.is_nonblocking() {
                return Err(EAGAIN);
            }
            if crate::sched::cancelled() {
                return Err(EINTR);
            }
            crate::sched::sleep_ms(5);
        }
    }
    let crate::socket::InternetState::Tcp(id) = *internet.state.lock() else {
        return Err(ENOTCONN);
    };
    loop {
        let bytes = crate::net::stream::read(id, limit).unwrap_or_default();
        if !bytes.is_empty() {
            return Ok((bytes, None));
        }
        match crate::net::stream::state(id) {
            Some(crate::net::stream::State::Closed | crate::net::stream::State::CloseWait)
            | None => {
                return Ok((Vec::new(), None));
            }
            _ => {}
        }
        if flags & DONTWAIT != 0 || internet.is_nonblocking() {
            return Err(EAGAIN);
        }
        if crate::sched::cancelled() {
            return Err(EINTR);
        }
        crate::sched::sleep_ms(5);
    }
}

fn receive_internet(
    internet: &crate::socket::InternetSocket,
    output: u64,
    limit: usize,
    flags: u64,
    address: u64,
    address_length: u64,
) -> u64 {
    let (bytes, source) = match receive_internet_vec(internet, limit, flags) {
        Ok(result) => result,
        Err(reason) => return reason,
    };
    if !bytes.is_empty() {
        // SAFETY: `output` was validated by the caller for `limit` bytes and
        // `bytes.len() <= limit`.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), output as *mut u8, bytes.len());
        }
    }
    if let Some((from, port)) = source {
        if address != 0
            && address_length != 0
            && !write_sockaddr_in(address, address_length, from, port)
        {
            return error::EFAULT;
        }
    }
    bytes.len() as u64
}

fn write_sockaddr_in(address: u64, length: u64, ip: nexus_net::Ipv4, port: u16) -> bool {
    let Some((size_at, _)) = crate::arch::syscall::user_range(length, 4, 4) else {
        return false;
    };
    let requested = unsafe { (size_at as *const u32).read_unaligned() }.min(16);
    if requested < 16 {
        return false;
    }
    let Some((out, _)) = crate::arch::syscall::user_range(address, 16, 16) else {
        return false;
    };
    unsafe {
        core::ptr::write_bytes(out as *mut u8, 0, 16);
        (out as *mut u16).write_unaligned(family::INET as u16);
        (out as *mut u8)
            .add(2)
            .cast::<u16>()
            .write_unaligned(port.to_be());
        core::ptr::copy_nonoverlapping(ip.as_ptr(), (out as *mut u8).add(4), 4);
        (size_at as *mut u32).write_unaligned(16);
    }
    true
}

/// `struct msghdr`, normalized across x86-64 (56 bytes) and i386 (28 bytes).
struct MessageHeader {
    name: u64,
    name_length: u64,
    vectors: u64,
    vector_count: u64,
    control: u64,
    control_length: u64,
}

fn header_of(at: u64, compat32: bool) -> Result<MessageHeader, u64> {
    if compat32 {
        const MSGHDR32: u64 = 28;
        let Some((pointer, _)) = crate::arch::syscall::user_range(at, MSGHDR32, MSGHDR32) else {
            return Err(error::EFAULT);
        };
        // SAFETY: twenty-eight bytes inside the user half, read as seven 32-bit words.
        unsafe {
            let words = pointer as *const u32;
            Ok(MessageHeader {
                name: u64::from(words.read_unaligned()),
                name_length: u64::from(words.add(1).read_unaligned()),
                vectors: u64::from(words.add(2).read_unaligned()),
                vector_count: u64::from(words.add(3).read_unaligned()),
                control: u64::from(words.add(4).read_unaligned()),
                control_length: u64::from(words.add(5).read_unaligned()),
            })
        }
    } else {
        const MSGHDR64: u64 = 56;
        let Some((pointer, _)) = crate::arch::syscall::user_range(at, MSGHDR64, MSGHDR64) else {
            return Err(error::EFAULT);
        };
        // SAFETY: fifty-six bytes inside the user half, read as the seven words a
        // `msghdr` is.
        unsafe {
            let words = pointer as *const u64;
            Ok(MessageHeader {
                name: words.read_unaligned(),
                name_length: u64::from((pointer as *const u32).add(2).read_unaligned()),
                vectors: words.add(2).read_unaligned(),
                vector_count: words.add(3).read_unaligned(),
                control: words.add(4).read_unaligned(),
                control_length: words.add(5).read_unaligned(),
            })
        }
    }
}

fn gather_vectors(vectors: u64, count: u64, compat32: bool) -> Result<Vec<(u64, u64)>, u64> {
    if count > MAX_VECTORS {
        return Err(EMSGSIZE);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let stride = if compat32 { 8u64 } else { 16u64 };
    let Some((base, _)) =
        crate::arch::syscall::user_range(vectors, count * stride, MAX_VECTORS * stride)
    else {
        return Err(error::EFAULT);
    };
    let mut list = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let (at, length) = if compat32 {
            // SAFETY: inside the checked iovec32 array.
            unsafe {
                let vector = (base as *const u32).add(index * 2);
                (
                    u64::from(vector.read_unaligned()),
                    u64::from(vector.add(1).read_unaligned()),
                )
            }
        } else {
            // SAFETY: inside the checked iovec64 array.
            unsafe {
                let vector = (base as *const u64).add(index * 2);
                (vector.read_unaligned(), vector.add(1).read_unaligned())
            }
        };
        if length != 0 {
            list.push((at, length));
        }
    }
    Ok(list)
}

/// The most `iovec`s one message may carry.
///
/// Linux's `UIO_MAXIOV`, and the same number on purpose: a program that works
/// against Linux and not here, because the two chose different limits, is a
/// worse outcome than either limit.
///
/// A count above it is **refused**, not cut down. Truncating would send the
/// first thousand vectors and report the byte count as though that were the
/// whole message -- and a caller that had handed over more would have no way to
/// tell that from a short write it should retry. `EMSGSIZE` is what Linux
/// answers and it says what happened.
const MAX_VECTORS: u64 = 1024;

/// `EMSGSIZE`: the message is too long to be sent as one.
const EMSGSIZE: u64 = (-90i64) as u64;

/// `SOL_SOCKET`, and the one control message type this understands.
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;
/// `struct cmsghdr` is a length, a level and a type: sixteen bytes, and then
/// the payload aligned to eight.
const CMSGHDR: u64 = 16;

/// `sendmsg(fd, message, flags)` for x86-64 callers.
pub fn sendmsg(descriptor: u64, message: u64, flags: u64) -> u64 {
    sendmsg_with(descriptor, message, flags, false)
}

/// `sendmsg(fd, message, flags)` for i386 callers (`msghdr32` / `iovec32`).
pub fn sendmsg32(descriptor: u64, message: u64, flags: u64) -> u64 {
    sendmsg_with(descriptor, message, flags, true)
}

fn sendmsg_with(descriptor: u64, message: u64, flags: u64, compat32: bool) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let header = match header_of(message, compat32) {
        Ok(header) => header,
        Err(reason) => return reason,
    };
    let vectors = match gather_vectors(header.vectors, header.vector_count, compat32) {
        Ok(vectors) => vectors,
        Err(reason) => return reason,
    };

    let mut payload: Vec<u8> = Vec::new();
    for (at, length) in vectors {
        let take = length.min(MAX_TRANSFER - payload.len() as u64);
        let Some((from, size)) = crate::arch::syscall::user_range(at, take, MAX_TRANSFER) else {
            return error::EFAULT;
        };
        // SAFETY: `from` and `size` were validated by `user_range`.
        payload.extend_from_slice(unsafe { core::slice::from_raw_parts(from as *const u8, size) });
        if payload.len() as u64 >= MAX_TRANSFER {
            break;
        }
    }

    if let Some(internet) = socket.internet_state() {
        let peer = if header.name == 0 || header.name_length == 0 {
            None
        } else {
            match ipv4_address_of(header.name, header.name_length) {
                Ok(peer) => Some(peer),
                Err(reason) => return reason,
            }
        };
        return send_internet(&internet, &payload, flags, peer);
    }

    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };

    // The handles first. They are *taken* from this process before a byte is
    // sent, so there is never a moment where the bytes have gone and the
    // handles are still here -- a receiver that read the message and found no
    // descriptors would have no way to ask again.
    let cmsg_header = if compat32 { 12u64 } else { CMSGHDR };
    let mut carried: Vec<ipc::Handle> = Vec::new();
    if header.control != 0 && header.control_length >= cmsg_header {
        let most = header
            .control_length
            .min(cmsg_header + (MAX_HANDLES as u64) * 4 + 8);
        let Some((at, _)) = crate::arch::syscall::user_range(header.control, most, most) else {
            return error::EFAULT;
        };
        // SAFETY: the range was checked to lie inside the user half.
        let (length, level, which) = unsafe {
            let words = at as *const u8;
            if compat32 {
                (
                    u64::from(words.cast::<u32>().read_unaligned()),
                    words.add(4).cast::<u32>().read_unaligned(),
                    words.add(8).cast::<u32>().read_unaligned(),
                )
            } else {
                (
                    words.cast::<u64>().read_unaligned(),
                    words.add(8).cast::<u32>().read_unaligned(),
                    words.add(12).cast::<u32>().read_unaligned(),
                )
            }
        };
        if level == SOL_SOCKET && which == SCM_RIGHTS && length >= cmsg_header {
            let count = ((length - cmsg_header) / 4) as usize;
            if count > MAX_HANDLES {
                return EINVAL;
            }
            for index in 0..count {
                // SAFETY: inside the range checked above -- `length` is bounded
                // by `most`, which the check covered.
                let fd = unsafe {
                    (at as *const u8)
                        .add(cmsg_header as usize + index * 4)
                        .cast::<u32>()
                        .read_unaligned()
                };
                match process.handles.take(fd, ipc::Rights::TRANSFER) {
                    Ok(handle) => carried.push(handle),
                    Err(_) => {
                        for handle in carried {
                            process.handles.restore(handle);
                        }
                        return error::EBADF;
                    }
                }
            }
        }
    }

    if !carried.is_empty() && stream.send_handles(carried).is_err() {
        return EPIPE;
    }
    if payload.is_empty() {
        return 0;
    }
    match stream.write(&payload) {
        Ok(written) => written as u64,
        Err(SocketError::Cancelled) => EINTR,
        Err(_) => EPIPE,
    }
}

/// `recvmsg(fd, message, flags)` for x86-64 callers.
pub fn recvmsg(descriptor: u64, message: u64, flags: u64) -> u64 {
    recvmsg_with(descriptor, message, flags, false)
}

/// `recvmsg(fd, message, flags)` for i386 callers (`msghdr32` / `iovec32`).
pub fn recvmsg32(descriptor: u64, message: u64, flags: u64) -> u64 {
    recvmsg_with(descriptor, message, flags, true)
}

fn recvmsg_with(descriptor: u64, message: u64, flags: u64, compat32: bool) -> u64 {
    /// `MSG_DONTWAIT`.
    const DONTWAIT: u64 = 0x40;

    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let header = match header_of(message, compat32) {
        Ok(header) => header,
        Err(reason) => return reason,
    };
    let targets = match gather_vectors(header.vectors, header.vector_count, compat32) {
        Ok(targets) => targets,
        Err(reason) => return reason,
    };
    let mut room = 0u64;
    for (_, length) in &targets {
        room = room.saturating_add(*length);
    }
    let room = room.min(MAX_TRANSFER);

    if let Some(internet) = socket.internet_state() {
        let (bytes, source) = match receive_internet_vec(&internet, room as usize, flags) {
            Ok(result) => result,
            Err(reason) => return reason,
        };
        let mut written = 0usize;
        for (at, length) in targets {
            if written >= bytes.len() {
                break;
            }
            let take = (length as usize).min(bytes.len() - written);
            let Some((into, size)) =
                crate::arch::syscall::user_range_for(&process, at, take as u64, MAX_TRANSFER, true)
            else {
                return error::EFAULT;
            };
            // SAFETY: `into` was checked to be writable for `size` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().add(written), into as *mut u8, size);
            }
            written += size;
        }
        if let Some((from, port)) = source {
            if header.name != 0 && header.name_length >= 16 {
                let Some((out, _)) =
                    crate::arch::syscall::user_range_for(&process, header.name, 16, 16, true)
                else {
                    return error::EFAULT;
                };
                // SAFETY: 16 writable bytes checked above.
                unsafe {
                    core::ptr::write_bytes(out as *mut u8, 0, 16);
                    (out as *mut u16).write_unaligned(family::INET as u16);
                    (out as *mut u8)
                        .add(2)
                        .cast::<u16>()
                        .write_unaligned(port.to_be());
                    core::ptr::copy_nonoverlapping(from.as_ptr(), (out as *mut u8).add(4), 4);
                }
            }
        }
        write_msghdr_out(
            &process,
            message,
            compat32,
            if source.is_some() && header.name != 0 {
                Some(16)
            } else {
                None
            },
            0,
        );
        return written as u64;
    }

    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };

    // Handles are looked for twice: once before the read and once after it.
    let cmsg_header = if compat32 { 12u64 } else { CMSGHDR };
    let mut control_written = 0u64;
    let take_handles = |stream: &Arc<Stream>| -> Option<u64> {
        if header.control == 0 || header.control_length < cmsg_header {
            return None;
        }
        let taken = stream
            .take_handles((((header.control_length - cmsg_header) / 4) as usize).min(MAX_HANDLES));
        if !taken.is_empty() {
            let length = cmsg_header + taken.len() as u64 * 4;
            let Some((at, _)) =
                crate::arch::syscall::user_range(header.control, length, header.control_length)
            else {
                let mut queue = stream.received_queue();
                for handle in taken.into_iter().rev() {
                    queue.push_front(handle);
                }
                return Some(u64::MAX);
            };
            // SAFETY: the range was checked to lie inside the user half and is
            // exactly `length` bytes.
            unsafe {
                let out = at as *mut u8;
                if compat32 {
                    out.cast::<u32>().write_unaligned(length as u32);
                    out.add(4).cast::<u32>().write_unaligned(SOL_SOCKET);
                    out.add(8).cast::<u32>().write_unaligned(SCM_RIGHTS);
                } else {
                    out.cast::<u64>().write_unaligned(length);
                    out.add(8).cast::<u32>().write_unaligned(SOL_SOCKET);
                    out.add(12).cast::<u32>().write_unaligned(SCM_RIGHTS);
                }
                for (index, handle) in taken.into_iter().enumerate() {
                    let fd = process.handles.restore(handle);
                    out.add(cmsg_header as usize + index * 4)
                        .cast::<u32>()
                        .write_unaligned(fd);
                }
            }
            return Some(length);
        }
        None
    };

    if let Some(length) = take_handles(&stream) {
        if length == u64::MAX {
            return error::EFAULT;
        }
        control_written = length;
    }

    let bytes = if room == 0 {
        Vec::new()
    } else if flags & DONTWAIT != 0 || control_written > 0 {
        // With descriptors in hand, a blocking read would be a program stuck
        // waiting for bytes it may already have everything it needs without.
        stream.read_now(room as usize)
    } else {
        match stream.read(room as usize) {
            Ok(bytes) => bytes,
            Err(SocketError::Cancelled) => return EINTR,
            Err(_) => return ENOTCONN,
        }
    };

    // And again, now that bytes have arrived: whatever was sent with them was
    // put in the queue first.
    if control_written == 0 {
        if let Some(length) = take_handles(&stream) {
            if length == u64::MAX {
                return error::EFAULT;
            }
            control_written = length;
        }
    }

    // Scattered back out across the caller's vectors.
    let mut written = 0usize;
    for (at, length) in targets {
        if written >= bytes.len() {
            break;
        }
        let take = (length as usize).min(bytes.len() - written);
        let Some((into, size)) = crate::arch::syscall::user_range(at, take as u64, MAX_TRANSFER)
        else {
            return error::EFAULT;
        };
        // SAFETY: the range was checked to lie inside the user half and `size`
        // is no larger than what is left to copy.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr().add(written), into as *mut u8, size);
        }
        written += size;
    }

    if header.control != 0 {
        write_msghdr_out(&process, message, compat32, None, control_written);
    }
    written as u64
}

fn write_msghdr_out(
    process: &crate::process::Process,
    message: u64,
    compat32: bool,
    name_length: Option<u32>,
    control_written: u64,
) {
    if compat32 {
        if let Some((pointer, _)) =
            crate::arch::syscall::user_range_for(process, message, 28, 28, true)
        {
            // SAFETY: 28-byte msghdr32 validated writable.
            unsafe {
                let words = pointer as *mut u32;
                if let Some(len) = name_length {
                    words.add(1).write_unaligned(len);
                }
                words.add(5).write_unaligned(control_written as u32);
                words.add(6).write_unaligned(0);
            }
        }
    } else if let Some((pointer, _)) =
        crate::arch::syscall::user_range_for(process, message, 56, 56, true)
    {
        // SAFETY: 56-byte msghdr64 validated writable.
        unsafe {
            if let Some(len) = name_length {
                (pointer as *mut u32).add(2).write_unaligned(len);
            }
            (pointer as *mut u64)
                .add(5)
                .write_unaligned(control_written);
            (pointer as *mut u32).add(12).write_unaligned(0);
        }
    }
}

/// `shutdown(fd, how)`.
///
/// For `AF_INET` TCP sockets, closing the write side (`SHUT_WR` or `SHUT_RDWR`)
/// queues a FIN once outgoing data drains. For `AF_UNIX` pipes, half-close is
/// still answered `ENOSYS`.
pub fn shutdown(descriptor: u64, how: u64) -> u64 {
    if how > 2 {
        return EINVAL;
    }
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if let Some(internet) = socket.internet_state() {
        return match *internet.state.lock() {
            crate::socket::InternetState::Tcp(id) => {
                if how != 0 {
                    crate::net::stream::close(id);
                }
                0
            }
            crate::socket::InternetState::Udp { peer: Some(_), .. } => 0,
            _ => ENOTCONN,
        };
    }
    crate::kprintln!("[linux] shutdown of one direction is not translated; answering ENOSYS");
    error::ENOSYS
}

/// `setsockopt(fd, level, option, ...)`.
///
/// Every option is accepted and none is honoured, with one exception that
/// matters: `SO_PEERCRED` is refused, because a program asking who is on the
/// other end of a connection is asking a security question and an invented
/// answer is worse than none.
pub fn setsockopt(descriptor: u64, level: u64, option: u64) -> u64 {
    /// `SO_PEERCRED`.
    const PEERCRED: u64 = 17;
    if let Err(reason) = socket_of(descriptor) {
        return reason;
    }
    if level == u64::from(SOL_SOCKET) && option == PEERCRED {
        return error::ENOSYS;
    }
    0
}

/// `getsockopt(fd, level, option, value, length)`.
///
/// Reports actual socket type and pending TCP connection errors (`SO_ERROR`),
/// which nonblocking `connect` callers inspect after `poll`/`epoll_wait` reports
/// `POLLOUT`.
pub fn getsockopt(descriptor: u64, level: u64, option: u64, value: u64, length: u64) -> u64 {
    const SO_TYPE: u64 = 3;
    const SO_ERROR: u64 = 4;
    const SO_SNDBUF: u64 = 7;
    const SO_RCVBUF: u64 = 8;
    const PEERCRED: u64 = 17;

    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if level == u64::from(SOL_SOCKET) && option == PEERCRED {
        return error::ENOSYS;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Some((length_at, _)) = crate::arch::syscall::user_range_for(&process, length, 4, 4, true)
    else {
        return error::EFAULT;
    };
    // SAFETY: four writable bytes checked above.
    let room = unsafe { (length_at as *const u32).read_unaligned() };
    if room < 4 {
        return EINVAL;
    }
    let Some((value_at, _)) = crate::arch::syscall::user_range_for(&process, value, 4, 4, true)
    else {
        return error::EFAULT;
    };

    let answer: i32 = if level == u64::from(SOL_SOCKET) {
        match option {
            SO_TYPE => {
                if socket
                    .internet_state()
                    .is_some_and(|internet| internet.datagram)
                {
                    kind::DGRAM as i32
                } else {
                    kind::STREAM as i32
                }
            }
            SO_ERROR => {
                if let Some(internet) = socket.internet_state() {
                    match *internet.state.lock() {
                        crate::socket::InternetState::Tcp(id)
                            if crate::net::stream::trouble(id).is_some() =>
                        {
                            111 // ECONNREFUSED
                        }
                        _ => 0,
                    }
                } else {
                    0
                }
            }
            SO_SNDBUF | SO_RCVBUF => 131_072,
            _ => 0,
        }
    } else {
        0
    };

    // SAFETY: both four-byte outputs were validated writable above.
    unsafe {
        (value_at as *mut i32).write_unaligned(answer);
        (length_at as *mut u32).write_unaligned(4);
    }
    0
}

/// `getsockname(fd, address, length)`.
pub fn getsockname(descriptor: u64, address: u64, length: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if let Some(internet) = socket.internet_state() {
        let interface = crate::net::interface();
        let (ip, port) = match *internet.state.lock() {
            crate::socket::InternetState::Udp { port, peer } => (
                if peer.is_some() || port.is_some() {
                    interface.ip
                } else {
                    [0; 4]
                },
                port.unwrap_or(0),
            ),
            crate::socket::InternetState::Tcp(id) => (
                interface.ip,
                crate::net::stream::local_port(id).unwrap_or(0),
            ),
            crate::socket::InternetState::New => ([0; 4], 0),
        };
        return if write_sockaddr_in(address, length, ip, port) {
            0
        } else {
            error::EFAULT
        };
    }
    unnamed_unix_address(address, length)
}

/// `getpeername(fd, address, length)`.
pub fn getpeername(descriptor: u64, address: u64, length: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    if let Some(internet) = socket.internet_state() {
        let peer = match *internet.state.lock() {
            crate::socket::InternetState::Udp { peer, .. } => peer,
            crate::socket::InternetState::Tcp(id) => crate::net::stream::peer(id),
            crate::socket::InternetState::New => None,
        };
        let Some((ip, port)) = peer else {
            return ENOTCONN;
        };
        return if write_sockaddr_in(address, length, ip, port) {
            0
        } else {
            error::EFAULT
        };
    }
    if socket.stream().is_none() {
        return ENOTCONN;
    }
    unnamed_unix_address(address, length)
}

fn unnamed_unix_address(address: u64, length: u64) -> u64 {
    if address == 0 || length == 0 {
        return EINVAL;
    }
    let Some((at, _)) = crate::arch::syscall::user_range(length, 4, 4) else {
        return error::EFAULT;
    };
    // SAFETY: four bytes inside the user half, read as `socklen_t`.
    let room = unsafe { core::ptr::read_unaligned(at as *const u32) };
    if room >= 2 {
        let Some((into, _)) = crate::arch::syscall::user_range(address, 2, 110) else {
            return error::EFAULT;
        };
        // SAFETY: two bytes inside the user half.
        unsafe { core::ptr::write_unaligned(into as *mut u16, family::UNIX as u16) };
    }
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(at as *mut u32, 2) };
    0
}
