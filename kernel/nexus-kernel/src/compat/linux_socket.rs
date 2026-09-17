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
    /// The internet. Refused; see the note at the top.
    pub const INET: u64 = 2;
    pub const INET6: u64 = 10;
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
/// Interrupted.
const EINTR: u64 = (-4i64) as u64;
/// Broken connection.
const EPIPE: u64 = (-32i64) as u64;
/// The path is longer than a `sockaddr_un` holds.
const ENAMETOOLONG: u64 = (-36i64) as u64;

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

/// `socket(family, kind, protocol)`.
pub fn socket(family: u64, kind: u64, protocol: u64) -> u64 {
    if family == family::INET || family == family::INET6 {
        crate::kprintln!(
            "[linux] a socket in the internet family is not translated yet; answering EAFNOSUPPORT"
        );
        return EAFNOSUPPORT;
    }
    if family != family::UNIX {
        return EAFNOSUPPORT;
    }
    let bare = kind & !(kind::NONBLOCK | kind::CLOEXEC);
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
    let Some((pointer, _)) = crate::arch::syscall::user_range(fds, 8, 8) else {
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
    if address != 0 && length != 0 {
        if let Some((at, _)) = crate::arch::syscall::user_range(length, 8, 8) {
            // SAFETY: eight bytes inside the user half.
            let room = unsafe { core::ptr::read_unaligned(at as *const u32) };
            if room >= 2 {
                if let Some((into, _)) = crate::arch::syscall::user_range(address, 2, SOCKADDR_UN) {
                    // SAFETY: two bytes inside the user half.
                    unsafe { core::ptr::write_unaligned(into as *mut u16, family::UNIX as u16) };
                }
            }
            // SAFETY: as above; the length the caller gets back is what was
            // written, which is what `accept` promises.
            unsafe { core::ptr::write_unaligned(at as *mut u32, 2) };
        }
    }
    u64::from(handle)
}

/// `read`, `recv` and `recvfrom` on a connected socket.
pub fn receive(descriptor: u64, buffer: u64, count: u64, flags: u64) -> u64 {
    /// `MSG_DONTWAIT`: do not block, whatever the socket was opened as.
    const DONTWAIT: u64 = 0x40;

    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
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
pub fn send(descriptor: u64, buffer: u64, count: u64, _flags: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
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
    match stream.write(&held) {
        Ok(written) => written as u64,
        Err(SocketError::Cancelled) => EINTR,
        Err(_) => EPIPE,
    }
}

/// `struct msghdr`, as x86-64 lays it out.
///
/// Fifty-six bytes: a name and its length, an iovec array and its count, a
/// control buffer and its length, and a flags word. Read field by field rather
/// than as a structure, because the file it comes from is the caller's memory
/// and carries no alignment guarantee.
struct MessageHeader {
    vectors: u64,
    vector_count: u64,
    control: u64,
    control_length: u64,
}

fn header_of(at: u64) -> Result<MessageHeader, u64> {
    const MSGHDR: u64 = 56;
    let Some((pointer, _)) = crate::arch::syscall::user_range(at, MSGHDR, MSGHDR) else {
        return Err(error::EFAULT);
    };
    // SAFETY: fifty-six bytes inside the user half, read as the seven words a
    // `msghdr` is.
    unsafe {
        let words = pointer as *const u64;
        Ok(MessageHeader {
            vectors: words.add(2).read_unaligned(),
            vector_count: words.add(3).read_unaligned(),
            control: words.add(4).read_unaligned(),
            control_length: words.add(5).read_unaligned(),
        })
    }
}

/// `SOL_SOCKET`, and the one control message type this understands.
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;
/// `struct cmsghdr` is a length, a level and a type: sixteen bytes, and then
/// the payload aligned to eight.
const CMSGHDR: u64 = 16;

/// `sendmsg(fd, message, flags)`.
///
/// The gather is flattened and sent as one run of bytes, which is what a stream
/// socket does with it anyway. What makes this different from `send` is the
/// control buffer: `SCM_RIGHTS` carries descriptors, and descriptors here are
/// handles.
pub fn sendmsg(descriptor: u64, message: u64, _flags: u64) -> u64 {
    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let header = match header_of(message) {
        Ok(header) => header,
        Err(reason) => return reason,
    };

    // The handles first. They are *taken* from this process before a byte is
    // sent, so there is never a moment where the bytes have gone and the
    // handles are still here -- a receiver that read the message and found no
    // descriptors would have no way to ask again.
    let mut carried: Vec<ipc::Handle> = Vec::new();
    if header.control != 0 && header.control_length >= CMSGHDR {
        let most = header
            .control_length
            .min(CMSGHDR + (MAX_HANDLES as u64) * 4 + 8);
        let Some((at, _)) = crate::arch::syscall::user_range(header.control, most, most) else {
            return error::EFAULT;
        };
        // SAFETY: the range was checked to lie inside the user half.
        let (length, level, which) = unsafe {
            let words = at as *const u8;
            (
                words.cast::<u64>().read_unaligned(),
                words.add(8).cast::<u32>().read_unaligned(),
                words.add(12).cast::<u32>().read_unaligned(),
            )
        };
        if level == SOL_SOCKET && which == SCM_RIGHTS && length >= CMSGHDR {
            let count = ((length - CMSGHDR) / 4) as usize;
            if count > MAX_HANDLES {
                return EINVAL;
            }
            for index in 0..count {
                // SAFETY: inside the range checked above -- `length` is bounded
                // by `most`, which the check covered.
                let fd = unsafe {
                    (at as *const u8)
                        .add(CMSGHDR as usize + index * 4)
                        .cast::<u32>()
                        .read_unaligned()
                };
                // Moved, not copied: the sender gives it up here. That is what
                // a handle crossing a boundary means everywhere else in this
                // system, and `SCM_RIGHTS` is the one place Linux copies
                // instead -- so this is the stricter of the two, and a sender
                // that wanted to keep its own duplicates it first.
                match process.handles.take(fd, ipc::Rights::TRANSFER) {
                    Ok(handle) => carried.push(handle),
                    Err(_) => {
                        // Put back whatever was already taken, so a bad
                        // descriptor halfway through is a refusal rather than
                        // a program that has lost handles.
                        for handle in carried {
                            process.handles.restore(handle);
                        }
                        return error::EBADF;
                    }
                }
            }
        }
    }

    // Then the bytes, gathered flat.
    let mut payload: Vec<u8> = Vec::new();
    if header.vector_count > 0 {
        const IOVEC: u64 = 16;
        let count = header.vector_count.min(64);
        let Some((base, _)) =
            crate::arch::syscall::user_range(header.vectors, count * IOVEC, 64 * IOVEC)
        else {
            return error::EFAULT;
        };
        for index in 0..count as usize {
            // SAFETY: inside the range checked above.
            let (at, length) = unsafe {
                let vector = (base as *const u64).add(index * 2);
                (vector.read_unaligned(), vector.add(1).read_unaligned())
            };
            if length == 0 {
                continue;
            }
            let take = length.min(MAX_TRANSFER - payload.len() as u64);
            let Some((from, size)) = crate::arch::syscall::user_range(at, take, MAX_TRANSFER)
            else {
                return error::EFAULT;
            };
            // SAFETY: as above.
            payload
                .extend_from_slice(unsafe { core::slice::from_raw_parts(from as *const u8, size) });
            if payload.len() as u64 >= MAX_TRANSFER {
                break;
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

/// `recvmsg(fd, message, flags)`.
pub fn recvmsg(descriptor: u64, message: u64, flags: u64) -> u64 {
    /// `MSG_DONTWAIT`.
    const DONTWAIT: u64 = 0x40;

    let socket = match socket_of(descriptor) {
        Ok(socket) => socket,
        Err(reason) => return reason,
    };
    let Some(stream) = socket.stream() else {
        return ENOTCONN;
    };
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let header = match header_of(message) {
        Ok(header) => header,
        Err(reason) => return reason,
    };

    // How much the caller has room for, across its vectors.
    const IOVEC: u64 = 16;
    let count = header.vector_count.min(64);
    let mut room = 0u64;
    let mut targets: Vec<(u64, u64)> = Vec::new();
    if count > 0 {
        let Some((base, _)) =
            crate::arch::syscall::user_range(header.vectors, count * IOVEC, 64 * IOVEC)
        else {
            return error::EFAULT;
        };
        for index in 0..count as usize {
            // SAFETY: inside the range checked above.
            let (at, length) = unsafe {
                let vector = (base as *const u64).add(index * 2);
                (vector.read_unaligned(), vector.add(1).read_unaligned())
            };
            if length == 0 {
                continue;
            }
            targets.push((at, length));
            room = room.saturating_add(length);
        }
    }
    let room = room.min(MAX_TRANSFER);

    // Handles are looked for twice: once before the read and once after it.
    //
    // They are a separate queue from the byte stream -- see the note at the top
    // of the file -- and a sender puts them in *before* the bytes. A receiver
    // that arrived first would find the queue empty, block for the bytes, and
    // return a message with no descriptors on it although the sender had
    // attached one. That is exactly what happened: the server read the marker
    // and reported no control message, and the descriptor it was sent was still
    // sitting in the queue.
    //
    // Looking again after the read closes it, because the bytes cannot arrive
    // before the handles that were sent ahead of them.
    let mut control_written = 0u64;
    let take_handles = |stream: &Arc<Stream>| -> Option<u64> {
        if header.control == 0 || header.control_length < CMSGHDR {
            return None;
        }
        let taken = stream
            .take_handles((((header.control_length - CMSGHDR) / 4) as usize).min(MAX_HANDLES));
        if !taken.is_empty() {
            let length = CMSGHDR + taken.len() as u64 * 4;
            let Some((at, _)) =
                crate::arch::syscall::user_range(header.control, length, header.control_length)
            else {
                // The handles have already left the stream, so they are put
                // back into this side's own queue rather than lost: the peer
                // sent them, and a bad control pointer is this side's mistake.
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
                out.cast::<u64>().write_unaligned(length);
                out.add(8).cast::<u32>().write_unaligned(SOL_SOCKET);
                out.add(12).cast::<u32>().write_unaligned(SCM_RIGHTS);
                for (index, handle) in taken.into_iter().enumerate() {
                    let fd = process.handles.restore(handle);
                    out.add(CMSGHDR as usize + index * 4)
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

    // And the control length the caller is told about.
    if header.control != 0 {
        const MSGHDR: u64 = 56;
        if let Some((pointer, _)) = crate::arch::syscall::user_range(message, MSGHDR, MSGHDR) {
            // SAFETY: as in `header_of`.
            unsafe {
                (pointer as *mut u64)
                    .add(5)
                    .write_unaligned(control_written);
            }
        }
    }
    written as u64
}

/// `shutdown(fd, how)`.
///
/// Accepted and recorded only as far as it can be: this closes nothing, because
/// a direction of a connection cannot be closed here without a third state in
/// the pipe underneath. A program using it to signal end of stream will find
/// the other side still waiting, so it is answered `ENOSYS` rather than zero --
/// the one thing worse than not having it is appearing to.
pub fn shutdown(_descriptor: u64, _how: u64) -> u64 {
    crate::kprintln!("[linux] shutdown of one direction is not translated; answering ENOSYS");
    error::ENOSYS
}

/// `getsockopt` and `setsockopt`.
///
/// Every option is accepted and none is honoured, with one exception that
/// matters: `SO_PEERCRED` is refused, because a program asking who is on the
/// other end of a connection is asking a security question and an invented
/// answer is worse than none.
pub fn setsockopt(_descriptor: u64, level: u64, option: u64) -> u64 {
    /// `SO_PEERCRED`.
    const PEERCRED: u64 = 17;
    if level == u64::from(SOL_SOCKET) && option == PEERCRED {
        return error::ENOSYS;
    }
    0
}
