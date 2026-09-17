//! Waiting for a descriptor to be ready.
//!
//! `poll` and `epoll_wait` are the two calls every event loop ever written
//! blocks in. A translation layer without them runs programs that do one thing
//! at a time: a program cannot serve two connections, cannot draw while reading
//! input, and cannot notice that the far end of a pipe has gone.
//!
//! # Level-triggered, because a level cannot be lost
//!
//! Both are answered by asking each descriptor whether it is ready *now*,
//! rather than by remembering that something happened to it. That is the same
//! decision [`crate::waitset`] made and for the same reason: an edge is a fact
//! about a moment, and an edge delivered while nobody was waiting is an edge
//! lost. A level costs a re-test and cannot be.
//!
//! It also means `epoll` here is `EPOLLET`-free. A program that asked for
//! edge-triggered behaviour is refused rather than quietly given
//! level-triggered, because the difference decides whether that program's loop
//! terminates.
//!
//! # What waiting actually waits on
//!
//! A Nexus wait set, built from whichever of the caller's descriptors can be
//! watched. That is what makes a blocking `poll` a *block* rather than a spin:
//! the thread is taken off the run queue and the pipe's own `Watchers` wake it.
//!
//! Descriptors that cannot be watched are the ones that are never not ready --
//! a file, the console -- so a `poll` containing one returns immediately and
//! never needs to sleep at all. There is nothing in between: a descriptor is
//! either watchable or always ready, so the set is never missing a wake-up it
//! needed.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::IrqSpinLock;
use crate::waitset::{WaitSet, Watched};

use super::linux::error;
use super::linux_files::{describe, Descriptor};

/// The bits `poll` reports, as `poll.h` numbers them.
pub mod event {
    /// There is something to read, or there never will be again.
    pub const IN: i16 = 0x001;
    /// An error condition. Always reported, whether or not it was asked for.
    pub const ERR: i16 = 0x008;
    /// The far end has gone. Always reported, like `ERR`.
    pub const HUP: i16 = 0x010;
    /// The descriptor names nothing. Always reported.
    pub const NVAL: i16 = 0x020;
    /// There is room to write.
    pub const OUT: i16 = 0x004;
}

/// The same, as `epoll.h` numbers them -- which is not the same numbering, and
/// the difference is a real source of bugs in programs that assume it is.
pub mod epoll {
    pub const IN: u32 = 0x001;
    pub const OUT: u32 = 0x004;
    pub const ERR: u32 = 0x008;
    pub const HUP: u32 = 0x010;
    /// Edge-triggered. Refused; see the note at the top of the file.
    pub const ET: u32 = 0x8000_0000;

    pub const CTL_ADD: u64 = 1;
    pub const CTL_DEL: u64 = 2;
    pub const CTL_MOD: u64 = 3;
}

/// The most descriptors one `poll` will walk.
///
/// The array comes from ring 3 and its length comes with it, so there has to be
/// a cap: without one a program could ask the kernel to walk a list of its own
/// choosing. Sixty-four is what a wait set holds, which is the real constraint
/// underneath.
const MAX_WATCHED: u64 = 64;

/// `struct pollfd` is eight bytes: an `int` and two `short`.
const POLLFD_BYTES: u64 = 8;
/// `struct epoll_event` is twelve, packed -- and the packing is part of the ABI
/// rather than something a compiler may choose.
const EPOLL_EVENT_BYTES: u64 = 12;

/// File exists: what `EPOLL_CTL_ADD` gets for a descriptor already in the set.
const EEXIST: u64 = (-17i64) as u64;
/// No such file: what `EPOLL_CTL_DEL` or `MOD` gets for one that is not.
const ENOENT: u64 = (-2i64) as u64;
/// Interrupted: the thread was asked to stop while it waited.
const EINTR: u64 = (-4i64) as u64;

/// What one descriptor in an `epoll` instance is watched for.
#[derive(Clone, Copy)]
struct Interest {
    /// The bits the program asked for.
    events: u32,
    /// The cookie it gave, handed back unchanged when the descriptor is ready.
    ///
    /// The whole point of `epoll` over `poll`: a program with fifty
    /// descriptors uses this to find out which one it was without a search.
    data: u64,
}

/// Every `epoll` instance, by process and by the descriptor that names it.
static SETS: IrqSpinLock<BTreeMap<(u64, u32), BTreeMap<u32, Interest>>> =
    IrqSpinLock::new(BTreeMap::new());

/// Calls that blocked, and calls that found something already ready.
static WAITED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IMMEDIATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Waits that blocked, and waits answered without blocking.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        WAITED.load(Ordering::Relaxed),
        IMMEDIATE.load(Ordering::Relaxed),
    )
}

/// Forget a process's `epoll` instances. Called when it ends.
pub fn forget(process: u64) {
    SETS.lock().retain(|(owner, _), _| *owner != process);
}

/// Forget one descriptor: it has been closed.
///
/// Closing a descriptor takes it out of every `epoll` instance in the process,
/// which is what Linux does and is not a courtesy -- a set still holding a
/// closed descriptor would report a number the program can no longer use, and
/// the number may by then name something else entirely.
pub fn forget_descriptor(process: u64, descriptor: u32) {
    let mut sets = SETS.lock();
    sets.remove(&(process, descriptor));
    for ((owner, _), interests) in sets.iter_mut() {
        if *owner == process {
            interests.remove(&descriptor);
        }
    }
}

/// What a descriptor can do right now, as `poll` bits.
///
/// The two "always" bits are why this returns a set rather than a boolean:
/// `POLLHUP` and `POLLERR` are reported whether or not the caller asked for
/// them, because a program that did not ask about a hangup still has to stop
/// waiting for data that is not coming.
fn ready_now(descriptor: u64) -> i16 {
    match describe(descriptor) {
        // Never not ready. Standard input is "readable" in the sense that
        // matters: a read of it returns immediately, with zero.
        Ok(Descriptor::Console(stream)) => {
            if stream == 0 {
                event::IN
            } else {
                event::OUT
            }
        }
        // A file is always both. There is nothing to wait for: a read of it
        // returns bytes or end of file, and neither blocks.
        Ok(Descriptor::File(_, _)) => event::IN | event::OUT,
        Ok(Descriptor::Pipe(end)) => {
            let mut bits = 0;
            if end.is_writing() {
                if end.pipe().room() > 0 {
                    bits |= event::OUT;
                }
                if !end.pipe().readers_open() {
                    // Nothing will ever read this, which is an error condition
                    // the writer has to notice rather than a reason to wait.
                    bits |= event::ERR;
                }
            } else {
                if end.pipe().queued() > 0 {
                    bits |= event::IN;
                }
                if !end.pipe().writers_open() {
                    // The far end has gone. `POLLHUP` *and* `POLLIN`: there may
                    // still be bytes buffered, and a reader told only about the
                    // hangup would throw them away.
                    bits |= event::HUP | event::IN;
                }
            }
            bits
        }
        // A window's channel. Readable when the compositor has said something.
        Ok(Descriptor::Other(crate::ipc::Object::Channel(endpoint))) => {
            let mut bits = event::OUT;
            if endpoint.queued() > 0 {
                bits |= event::IN;
            }
            if !endpoint.peer_open() {
                bits |= event::HUP;
            }
            bits
        }
        // A socket: readable when bytes or handles have arrived, or when the
        // peer has gone; writable when there is room. A listening one is
        // "readable" when somebody is waiting to be accepted, which is the
        // convention every program that polls a listener relies on.
        Ok(Descriptor::Other(crate::ipc::Object::Socket(socket))) => {
            if let Some(listener) = socket.listener() {
                if listener.ready() {
                    event::IN
                } else {
                    0
                }
            } else if let Some(stream) = socket.stream() {
                let mut bits = 0;
                if stream.readable() {
                    bits |= event::IN;
                }
                if stream.writable() {
                    bits |= event::OUT;
                }
                if !stream.connected() {
                    bits |= event::HUP | event::IN;
                }
                bits
            } else {
                // Made, and not yet connected or listening. Nothing can happen
                // to it until the program does something with it.
                0
            }
        }
        Ok(Descriptor::Other(_)) => event::OUT,
        Err(_) => event::NVAL,
    }
}

/// What a descriptor can be watched *on*, if anything.
///
/// `None` for the ones that are never not ready. A caller that gets `None` for
/// every member has nothing to wait for and must not block.
fn watchable(descriptor: u64) -> Option<Watched> {
    match describe(descriptor) {
        Ok(Descriptor::Pipe(end)) => Some(Watched::Pipe(end)),
        Ok(Descriptor::Other(crate::ipc::Object::Channel(endpoint))) => {
            Some(Watched::Channel(endpoint))
        }
        Ok(Descriptor::Other(crate::ipc::Object::Socket(socket))) => {
            if let Some(listener) = socket.listener() {
                Some(Watched::Listener(listener))
            } else {
                socket.stream().map(Watched::Stream)
            }
        }
        _ => None,
    }
}

/// Turn a timeout in milliseconds into a tick deadline.
///
/// `None` for "wait for ever", which is what a negative timeout means to
/// `poll`. A machine whose timer has not started has no rate, and a deadline
/// computed from a rate of zero is one that has already passed -- so such a
/// wait is untimed rather than instantaneous.
fn deadline_of(milliseconds: i64) -> Option<u64> {
    if milliseconds < 0 {
        return None;
    }
    let rate = crate::arch::time::frequency_hz();
    if rate == 0 {
        return None;
    }
    Some(
        crate::arch::time::ticks()
            .saturating_add((milliseconds as u64).saturating_mul(rate) / 1000),
    )
}

/// Block until one of `members` may have become ready, or until the deadline.
///
/// Returns false when nothing could be waited on, which is a caller's cue to
/// stop rather than spin: a set with no members never signals.
fn sleep_on(members: &[Watched], deadline: Option<u64>) -> bool {
    if members.is_empty() {
        return false;
    }
    let set = Arc::new(WaitSet::new());
    for (key, member) in members.iter().enumerate() {
        // A member that will not go in is one this wait will not be woken by,
        // and the deadline still applies -- so it is dropped rather than made
        // into a failure.
        let _ = set.add(key as u64, member.clone());
    }
    // The generation is read before anything is tested, so a signal that lands
    // between the test the caller already did and this wait is seen as a change
    // rather than slept through.
    let seen = set.change_count();
    set.wait_since(seen, deadline);
    true
}

/// `poll(fds, count, timeout)`.
pub fn poll(fds: u64, count: u64, timeout: i64) -> u64 {
    if count > MAX_WATCHED {
        return error::EINVAL;
    }
    if count == 0 {
        // A `poll` of nothing is a sleep, and Linux treats it as one.
        if timeout > 0 {
            crate::sched::sleep_ms(timeout as u64);
        }
        return 0;
    }
    let Some((pointer, _)) = crate::arch::syscall::user_range(
        fds,
        count.saturating_mul(POLLFD_BYTES),
        MAX_WATCHED.saturating_mul(POLLFD_BYTES),
    ) else {
        return error::EFAULT;
    };

    // The descriptors and what was asked about each, read once. Read again on
    // every round would let a program change the array under the kernel.
    let mut watching: Vec<(u64, i16)> = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        // SAFETY: the range was checked to lie inside the user half and is
        // exactly `count` `struct pollfd` long.
        let entry = unsafe { (pointer as *const [i32; 2]).add(index).read_unaligned() };
        let descriptor = entry[0];
        let asked = (entry[1] & 0xFFFF) as i16;
        watching.push((
            if descriptor < 0 {
                // A negative descriptor is how a program says "skip this one"
                // without rebuilding its array. It is not an error.
                u64::MAX
            } else {
                descriptor as u64
            },
            asked,
        ));
    }

    let deadline = deadline_of(timeout);
    loop {
        let mut ready = 0u64;
        for (index, (descriptor, asked)) in watching.iter().enumerate() {
            let bits = if *descriptor == u64::MAX {
                0
            } else {
                // The three below are reported whether or not they were asked
                // for; everything else only if it was.
                let now = ready_now(*descriptor);
                now & (asked | event::ERR | event::HUP | event::NVAL)
            };
            if bits != 0 {
                ready += 1;
            }
            // SAFETY: as above; `revents` is the second `short` of the entry.
            unsafe {
                let out = (pointer as *mut i16).add(index * 4 + 3);
                out.write_unaligned(bits);
            }
        }
        if ready > 0 {
            IMMEDIATE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return ready;
        }
        if timeout == 0 {
            return 0;
        }
        if crate::sched::cancelled() {
            return EINTR;
        }
        if let Some(deadline) = deadline {
            if crate::arch::time::ticks() >= deadline {
                return 0;
            }
        }

        let members: Vec<Watched> = watching
            .iter()
            .filter(|(descriptor, _)| *descriptor != u64::MAX)
            .filter_map(|(descriptor, _)| watchable(*descriptor))
            .collect();
        WAITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !sleep_on(&members, deadline) {
            // Nothing here can ever become ready, and nothing is ready now. A
            // caller that waited would wait for ever, so it is told nothing
            // happened -- which is the truth.
            return 0;
        }
    }
}

/// `epoll_create1(flags)`.
///
/// The instance is a real Nexus wait set, held by an ordinary handle -- so
/// closing the descriptor releases it like any other, and the interest list
/// kept beside it goes with the process.
pub fn create(flags: u64) -> u64 {
    /// `EPOLL_CLOEXEC`, which every current program passes and which has
    /// nothing to change here yet.
    const CLOEXEC: u64 = 0o2_000_000;
    if flags & !CLOEXEC != 0 {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let set = Arc::new(WaitSet::new());
    let descriptor = process.handles.insert(
        crate::ipc::Object::WaitSet(set),
        crate::ipc::Rights::READ | crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE,
    );
    SETS.lock()
        .insert((process.id.0, descriptor), BTreeMap::new());
    u64::from(descriptor)
}

/// `epoll_ctl(epfd, operation, fd, event)`.
pub fn control(set: u64, operation: u64, descriptor: u64, event: u64) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Ok(set) = u32::try_from(set) else {
        return error::EBADF;
    };
    let Ok(descriptor) = u32::try_from(descriptor) else {
        return error::EBADF;
    };
    // The descriptor has to name something, and an `epoll` instance cannot
    // watch itself -- which would be a set whose readiness depended on its own.
    if u64::from(descriptor) == u64::from(set) {
        return error::EINVAL;
    }
    if matches!(operation, epoll::CTL_ADD | epoll::CTL_MOD)
        && ready_now(u64::from(descriptor)) & event::NVAL != 0
    {
        return error::EBADF;
    }

    let interest = if operation == epoll::CTL_DEL {
        Interest { events: 0, data: 0 }
    } else {
        let Some((pointer, _)) =
            crate::arch::syscall::user_range(event, EPOLL_EVENT_BYTES, EPOLL_EVENT_BYTES)
        else {
            return error::EFAULT;
        };
        // SAFETY: twelve bytes inside the user half, read as the packed
        // `struct epoll_event` the ABI defines.
        let events = unsafe { (pointer as *const u32).read_unaligned() };
        // SAFETY: as above; the cookie follows the bits, unaligned.
        let data = unsafe { (pointer as *const u8).add(4).cast::<u64>().read_unaligned() };
        if events & epoll::ET != 0 {
            crate::kprintln!("[linux] an edge-triggered epoll is not translated; answering EINVAL");
            return error::EINVAL;
        }
        Interest { events, data }
    };

    let mut sets = SETS.lock();
    let Some(interests) = sets.get_mut(&(process.id.0, set)) else {
        return error::EBADF;
    };
    match operation {
        epoll::CTL_ADD => {
            if interests.contains_key(&descriptor) {
                return EEXIST;
            }
            interests.insert(descriptor, interest);
            0
        }
        epoll::CTL_MOD => {
            if !interests.contains_key(&descriptor) {
                return ENOENT;
            }
            interests.insert(descriptor, interest);
            0
        }
        epoll::CTL_DEL => {
            if interests.remove(&descriptor).is_none() {
                return ENOENT;
            }
            0
        }
        _ => error::EINVAL,
    }
}

/// `epoll_wait(epfd, events, maxevents, timeout)`.
pub fn wait(set: u64, events: u64, most: u64, timeout: i64) -> u64 {
    if most == 0 || most > MAX_WATCHED {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Ok(set) = u32::try_from(set) else {
        return error::EBADF;
    };
    let Some((pointer, _)) = crate::arch::syscall::user_range(
        events,
        most.saturating_mul(EPOLL_EVENT_BYTES),
        MAX_WATCHED.saturating_mul(EPOLL_EVENT_BYTES),
    ) else {
        return error::EFAULT;
    };

    let deadline = deadline_of(timeout);
    loop {
        // The interest list, copied out from under the lock: testing a
        // descriptor takes the handle table's lock, and holding two is how a
        // deadlock is written.
        let interests: Vec<(u32, Interest)> = {
            let sets = SETS.lock();
            let Some(interests) = sets.get(&(process.id.0, set)) else {
                return error::EBADF;
            };
            interests.iter().map(|(fd, want)| (*fd, *want)).collect()
        };

        let mut reported = 0u64;
        for (descriptor, want) in &interests {
            if reported >= most {
                break;
            }
            let now = ready_now(u64::from(*descriptor));
            if now & event::NVAL != 0 {
                // A descriptor that has stopped naming anything. Reported as an
                // error rather than dropped silently, so the program finds out.
                write_event(pointer, reported as usize, epoll::ERR, want.data);
                reported += 1;
                continue;
            }
            // `poll`'s bits and `epoll`'s are not the same numbering, so this
            // is a translation and not a cast.
            let mut bits = 0u32;
            if now & event::IN != 0 && want.events & epoll::IN != 0 {
                bits |= epoll::IN;
            }
            if now & event::OUT != 0 && want.events & epoll::OUT != 0 {
                bits |= epoll::OUT;
            }
            // As in `poll`, these two are reported whether or not they were
            // asked for.
            if now & event::HUP != 0 {
                bits |= epoll::HUP;
            }
            if now & event::ERR != 0 {
                bits |= epoll::ERR;
            }
            if bits != 0 {
                write_event(pointer, reported as usize, bits, want.data);
                reported += 1;
            }
        }
        if reported > 0 {
            IMMEDIATE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return reported;
        }
        if timeout == 0 {
            return 0;
        }
        if crate::sched::cancelled() {
            return EINTR;
        }
        if let Some(deadline) = deadline {
            if crate::arch::time::ticks() >= deadline {
                return 0;
            }
        }

        let members: Vec<Watched> = interests
            .iter()
            .filter_map(|(descriptor, _)| watchable(u64::from(*descriptor)))
            .collect();
        WAITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !sleep_on(&members, deadline) {
            return 0;
        }
    }
}

/// Write one `struct epoll_event` into the caller's array.
fn write_event(pointer: u64, index: usize, events: u32, data: u64) {
    // SAFETY: the caller checked the array lies inside the user half and is at
    // least `most` entries long, and `index` is below `most`.
    unsafe {
        let at = (pointer as *mut u8).add(index * EPOLL_EVENT_BYTES as usize);
        at.cast::<u32>().write_unaligned(events);
        at.add(4).cast::<u64>().write_unaligned(data);
    }
}

/// The wait set behind an `epoll` descriptor, kept alive by its handle.
///
/// Not used to wait -- `sleep_on` builds a fresh set from the members each
/// time, because the interest list changes and a set that had been built once
/// would be watching whatever was in it when it was made. This exists so that
/// the handle names something real, and so that closing it means something.
#[allow(dead_code)]
fn instance(process: &crate::process::Process, descriptor: u32) -> Option<Arc<WaitSet>> {
    process
        .handles
        .object(descriptor, crate::ipc::Rights::READ)
        .ok()
        .and_then(|object| match object {
            crate::ipc::Object::WaitSet(set) => Some(set),
            _ => None,
        })
}
