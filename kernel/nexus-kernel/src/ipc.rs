//! Handles, rights, and channels.
//!
//! # Why handles and not names
//!
//! A system call that names a resource by a global name — a path, a process
//! identifier, a port number — has to answer "is the caller allowed to touch
//! this?" every time it is made, from an ambient authority that lives somewhere
//! else. Every such check is a place to get it wrong, and the answer usually
//! comes down to who the caller *is* rather than what it was *given*.
//!
//! A handle is the other arrangement. It is an index into a table that belongs
//! to one process, and holding it is the authority: there is no way to name a
//! channel a process was not handed, so there is no check to forget. The rights
//! recorded beside it say what may be done with it, so the same object can be
//! given to one process to read and another to write.
//!
//! This is what the Linux and Windows compatibility layers will be written on
//! top of. Both of those present descriptors and handles of their own, and both
//! will be tables in a translation layer above this one rather than a second
//! idea of authority inside the kernel.
//!
//! # What a channel is
//!
//! Two endpoints, each with an inbox. Writing to one endpoint appends to the
//! *other's* inbox and wakes whoever is waiting there. Messages are whole:
//! a read returns one message or blocks, never half of one and never two run
//! together, because a byte stream would push framing into every user of it.
//!
//! An endpoint holds a weak reference to its peer, which is what makes "the
//! other end has gone" a fact the kernel can state rather than a timeout. Two
//! strong references would keep a dead channel alive forever.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

/// Longest message a channel will carry.
///
/// Small deliberately. A message is copied twice — out of the sender and into
/// the receiver — so anything large should be shared memory with a handle to
/// it, once there is such a thing, rather than a bigger number here.
pub const MAX_MESSAGE: usize = 256;

/// Messages an endpoint will hold before writes start failing.
///
/// A bound rather than a policy: without one, a receiver that stops reading
/// turns into unbounded kernel allocation driven by a sender that need not be
/// cooperating.
pub const MAX_QUEUED: usize = 64;

/// What may be done with a handle.
///
/// Checked on every use, and narrowed rather than widened when a handle is
/// passed on: a process can give away less than it has and never more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    /// Read messages.
    pub const READ: Self = Self(1 << 0);
    /// Write messages.
    pub const WRITE: Self = Self(1 << 1);
    /// Close the handle.
    pub const CLOSE: Self = Self(1 << 2);
    /// Everything a freshly created endpoint carries.
    pub const ALL: Self = Self(0b111);

    /// Whether every right in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw bits, for reporting across the system-call boundary.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One message in flight.
type Message = Vec<u8>;

/// One end of a channel.
pub struct Endpoint {
    /// Messages written by the peer and not yet read here.
    inbox: IrqSpinLock<VecDeque<Message>>,
    /// Threads blocked in [`Endpoint::receive`] on this end.
    arrivals: WaitQueue,
    /// The other end, weakly, so that a channel both of whose ends are still
    /// referenced by their owners does not keep itself alive after they go.
    peer: IrqSpinLock<Weak<Endpoint>>,
}

/// Why a channel operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// The message is longer than [`MAX_MESSAGE`].
    TooLong,
    /// The peer's inbox is full.
    Full,
    /// The other end has been closed.
    PeerClosed,
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::TooLong => "the message is too long",
            Self::Full => "the receiving end is full",
            Self::PeerClosed => "the other end of the channel is closed",
        })
    }
}

impl Endpoint {
    /// Create a connected pair.
    #[must_use]
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let first = Arc::new(Self::detached());
        let second = Arc::new(Self::detached());
        *first.peer.lock() = Arc::downgrade(&second);
        *second.peer.lock() = Arc::downgrade(&first);
        CHANNELS_CREATED.fetch_add(1, Ordering::Relaxed);
        (first, second)
    }

    fn detached() -> Self {
        Self {
            inbox: IrqSpinLock::new(VecDeque::new()),
            arrivals: WaitQueue::new(),
            peer: IrqSpinLock::new(Weak::new()),
        }
    }

    /// Whether the other end still exists.
    #[must_use]
    pub fn peer_open(&self) -> bool {
        self.peer.lock().strong_count() > 0
    }

    /// Append `message` to the peer's inbox and wake a reader there.
    ///
    /// Writing puts the message on the *other* end, which is what makes a
    /// channel a channel rather than a shared queue: a process cannot read back
    /// what it wrote.
    pub fn send(&self, message: &[u8]) -> Result<usize, ChannelError> {
        if message.len() > MAX_MESSAGE {
            return Err(ChannelError::TooLong);
        }

        let Some(peer) = self.peer.lock().upgrade() else {
            return Err(ChannelError::PeerClosed);
        };

        {
            let mut inbox = peer.inbox.lock();
            if inbox.len() >= MAX_QUEUED {
                return Err(ChannelError::Full);
            }
            inbox.push_back(message.to_vec());
        }

        // Outside the inbox lock, and after the push, so a thread this wakes
        // finds the message already there.
        peer.arrivals.wake_one();
        MESSAGES_SENT.fetch_add(1, Ordering::Relaxed);
        Ok(message.len())
    }

    /// Take the next message, if one is waiting.
    #[must_use]
    pub fn try_receive(&self) -> Option<Message> {
        let message = self.inbox.lock().pop_front();
        if message.is_some() {
            MESSAGES_RECEIVED.fetch_add(1, Ordering::Relaxed);
        }
        message
    }

    /// Take the next message, blocking until one arrives.
    ///
    /// Returns `None` only when there is nothing to wait for: the peer has gone
    /// and the inbox is empty, so no message can ever arrive. Blocking forever
    /// on a channel nobody holds the other end of would be a hang, and a hang
    /// the kernel could have known about is a bug.
    pub fn receive(&self) -> Option<Message> {
        loop {
            if let Some(message) = self.try_receive() {
                return Some(message);
            }
            if !self.peer_open() {
                return None;
            }

            // The condition is re-checked inside the wait, under the queue's
            // lock, which is what closes the window between the check above and
            // the block below.
            self.arrivals.wait_until(|| {
                !self.inbox.lock().is_empty() || self.peer.lock().strong_count() == 0
            });
        }
    }

    /// Messages waiting to be read here.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.inbox.lock().len()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Anyone blocked on the peer is waiting for a message that can no
        // longer come; waking them lets `receive` notice and report it rather
        // than waiting for a write that will never happen.
        if let Some(peer) = self.peer.lock().upgrade() {
            peer.arrivals.wake_one();
        }
    }
}

/// A kernel object a handle can refer to.
///
/// An enum rather than a trait object: there is one kind so far, the set is
/// closed and small, and a match that must be updated when a kind is added is
/// worth more here than dynamic dispatch.
#[derive(Clone)]
pub enum Object {
    /// One end of a channel.
    Channel(Arc<Endpoint>),
}

impl Object {
    /// A short name, for diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Channel(_) => "channel",
        }
    }
}

/// An object together with what its holder may do to it.
#[derive(Clone)]
pub struct Handle {
    pub object: Object,
    pub rights: Rights,
}

/// A process's handles.
///
/// The numbers are per-process and mean nothing outside it. They are never
/// reused within a table, so a handle closed and then used again is reported as
/// a bad handle rather than silently naming whatever took its place.
pub struct HandleTable {
    entries: IrqSpinLock<BTreeMap<u32, Handle>>,
    next: AtomicU32,
}

/// Why a handle could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    /// No such handle in this process.
    NotFound,
    /// The handle does not carry the rights the operation needs.
    Denied,
}

impl core::fmt::Display for HandleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "no such handle",
            Self::Denied => "the handle does not carry that right",
        })
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: IrqSpinLock::new(BTreeMap::new()),
            // Handle zero is never issued, so a zeroed variable is not a valid
            // handle by accident.
            next: AtomicU32::new(1),
        }
    }

    /// Add `object` with `rights`, returning the handle.
    pub fn insert(&self, object: Object, rights: Rights) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().insert(id, Handle { object, rights });
        id
    }

    /// The channel endpoint `id` names, if it names one and carries `needed`.
    pub fn channel(&self, id: u32, needed: Rights) -> Result<Arc<Endpoint>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Channel(endpoint) => Ok(Arc::clone(endpoint)),
        }
    }

    /// What `id` may be used for.
    pub fn rights(&self, id: u32) -> Result<Rights, HandleError> {
        self.entries
            .lock()
            .get(&id)
            .map(|handle| handle.rights)
            .ok_or(HandleError::NotFound)
    }

    /// Drop `id`, releasing its reference to the object.
    ///
    /// The last handle to an endpoint going away is what tells the other end
    /// the channel is closed, so this is not merely bookkeeping.
    pub fn close(&self, id: u32) -> Result<(), HandleError> {
        self.entries
            .lock()
            .remove(&id)
            .map(|_| ())
            .ok_or(HandleError::NotFound)
    }

    /// How many handles are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// A line per handle, for diagnostics.
    pub fn describe(&self) -> Vec<(u32, &'static str, u32)> {
        self.entries
            .lock()
            .iter()
            .map(|(id, handle)| (*id, handle.object.kind(), handle.rights.bits()))
            .collect()
    }
}

/// Channels created since boot.
static CHANNELS_CREATED: AtomicU64 = AtomicU64::new(0);
/// Messages written.
static MESSAGES_SENT: AtomicU64 = AtomicU64::new(0);
/// Messages read.
static MESSAGES_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// Channels created, messages sent, messages received.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    (
        CHANNELS_CREATED.load(Ordering::Relaxed),
        MESSAGES_SENT.load(Ordering::Relaxed),
        MESSAGES_RECEIVED.load(Ordering::Relaxed),
    )
}
