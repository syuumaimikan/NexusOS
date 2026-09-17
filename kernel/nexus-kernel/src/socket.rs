//! A connection between two programs on this machine.
//!
//! A Unix domain socket is three things a Nexus channel is not quite: it is a
//! *byte stream* rather than a sequence of messages, it is reached by a *name
//! in the filesystem* rather than by being handed an endpoint, and a server can
//! sit at that name and accept connections from programs it has never met.
//!
//! The last of those is the one that matters. Every channel in this system so
//! far exists because somebody with both ends handed one over — a parent to a
//! child, the compositor to a client. That is the right default and it is not
//! how a desktop protocol works: an X server or a Wayland compositor puts a
//! name in `/tmp` and serves whoever arrives, and a client that was started by
//! something else entirely connects to it.
//!
//! # What this is made of
//!
//! Two pipes and a queue of handles.
//!
//! A [`Stream`] is one side of a connection: a [`crate::pipe::PipeEnd`] it
//! writes into, another it reads from, and a place for handles the other side
//! has sent. Its peer holds the other ends of the same two pipes, crossed over.
//! There is no new buffering here and no second implementation of end-of-file —
//! a socket whose peer has gone reads zero because the pipe behind it does.
//!
//! # Passing a handle
//!
//! `SCM_RIGHTS` is how a Wayland client gives the compositor a buffer: the file
//! descriptor travels over the socket and arrives as a descriptor on the other
//! side. Here it arrives as a *handle*, through the same
//! [`crate::ipc::Handle`] that crosses a channel, with the rights it was sent
//! with and no more.
//!
//! One thing about it is not faithful, and it is written down rather than
//! discovered. On Linux a set of descriptors is attached to a particular byte
//! in the stream: a reader that reads up to that point and no further has not
//! received them yet. Here the handles are a separate queue, taken by the next
//! `recvmsg` that asks for them regardless of how many bytes have been read.
//! Every protocol the author knows of sends its descriptors with a message the
//! receiver reads whole, so the difference does not arise — but a protocol that
//! read a socket a byte at a time would see descriptors arrive early.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ipc::Handle;
use crate::pipe::{Pipe, PipeEnd, PipeError};
use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;
use crate::waitset::Watchers;

/// The most connections one listener will hold before refusing them.
///
/// A bound rather than a trust: the queue is filled by whoever connects, and a
/// listener that grew it without limit would be a server any program could make
/// the kernel allocate for.
pub const MAX_BACKLOG: usize = 32;

/// The most handles one side may have waiting to be taken.
///
/// The same argument. A peer that sent descriptors and never read replies would
/// otherwise keep this side's queue growing.
const MAX_HANDLES_WAITING: usize = 64;

/// Connections made and accepted, for the monitor.
static CONNECTED: AtomicU64 = AtomicU64::new(0);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static HANDLES_PASSED: AtomicU64 = AtomicU64::new(0);

/// Connections made, connections accepted, and handles passed across them.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    (
        CONNECTED.load(Ordering::Relaxed),
        ACCEPTED.load(Ordering::Relaxed),
        HANDLES_PASSED.load(Ordering::Relaxed),
    )
}

/// Why a socket operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    /// The other side has gone.
    Disconnected,
    /// The listener's queue is full.
    Backlog,
    /// The calling thread was asked to stop while it waited.
    Cancelled,
    /// More handles are waiting than this side will hold.
    TooManyHandles,
}

/// One side of a connection.
pub struct Stream {
    /// What this side writes into. The peer reads the other end of it.
    outgoing: Arc<PipeEnd>,
    /// What this side reads. The peer writes the other end.
    incoming: Arc<PipeEnd>,
    /// Handles the peer has sent and this side has not taken.
    received: IrqSpinLock<VecDeque<Handle>>,
    /// The other side, weakly: two sides that hold each other strongly are a
    /// connection that never closes, even when both programs have let go.
    peer: IrqSpinLock<Weak<Stream>>,
    /// Woken when a handle arrives, which the pipes below cannot report.
    handles_arrived: WaitQueue,
    watchers: Watchers,
}

impl Stream {
    /// A connected pair.
    ///
    /// Two pipes, crossed: what the first writes, the second reads. Made here
    /// rather than by either side, because a connection is a thing that exists
    /// between two programs and neither of them can make one alone.
    #[must_use]
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let (first_read, second_write) = Pipe::pair();
        let (second_read, first_write) = Pipe::pair();

        let first = Arc::new(Self {
            outgoing: first_write,
            incoming: first_read,
            received: IrqSpinLock::new(VecDeque::new()),
            peer: IrqSpinLock::new(Weak::new()),
            handles_arrived: WaitQueue::new(),
            watchers: Watchers::new(),
        });
        let second = Arc::new(Self {
            outgoing: second_write,
            incoming: second_read,
            received: IrqSpinLock::new(VecDeque::new()),
            peer: IrqSpinLock::new(Weak::new()),
            handles_arrived: WaitQueue::new(),
            watchers: Watchers::new(),
        });
        *first.peer.lock() = Arc::downgrade(&second);
        *second.peer.lock() = Arc::downgrade(&first);
        CONNECTED.fetch_add(1, Ordering::Relaxed);
        (first, second)
    }

    /// Start telling `set` when this side may have become ready.
    ///
    /// Both pipes and this side's own handle queue: a connection is ready when
    /// bytes arrive, when the peer goes, *or* when a handle arrives with no
    /// bytes at all — which is a thing a protocol does.
    pub fn watch(&self, set: Weak<crate::waitset::WaitSet>) {
        self.incoming.pipe().watch(set.clone());
        self.outgoing.pipe().watch(set.clone());
        self.watchers.add(set);
    }

    /// Whether a read would return without waiting.
    #[must_use]
    pub fn readable(&self) -> bool {
        self.incoming.readable() || !self.received.lock().is_empty()
    }

    /// Whether a write would return without waiting.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.outgoing.writable()
    }

    /// Whether the other side is still there.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.peer.lock().strong_count() > 0
    }

    /// Take up to `wanted` bytes, blocking while none are there.
    ///
    /// # Errors
    ///
    /// [`SocketError::Cancelled`] if the thread is asked to stop.
    pub fn read(&self, wanted: usize) -> Result<alloc::vec::Vec<u8>, SocketError> {
        match self.incoming.read(wanted) {
            Ok(bytes) => Ok(bytes),
            Err(PipeError::Cancelled) => Err(SocketError::Cancelled),
            Err(_) => Err(SocketError::Disconnected),
        }
    }

    /// Take whatever bytes are there without waiting.
    #[must_use]
    pub fn read_now(&self, wanted: usize) -> alloc::vec::Vec<u8> {
        if !self.incoming.readable() {
            return alloc::vec::Vec::new();
        }
        self.incoming.read(wanted).unwrap_or_default()
    }

    /// Put bytes in.
    ///
    /// # Errors
    ///
    /// [`SocketError::Disconnected`] when the other side has gone, which is
    /// what `EPIPE` means on a socket as much as on a pipe.
    pub fn write(&self, data: &[u8]) -> Result<usize, SocketError> {
        match self.outgoing.write(data) {
            Ok(written) => Ok(written),
            Err(PipeError::NoReader) => Err(SocketError::Disconnected),
            Err(PipeError::Cancelled) => Err(SocketError::Cancelled),
            Err(PipeError::WrongEnd) => Err(SocketError::Disconnected),
        }
    }

    /// Send handles to the other side.
    ///
    /// They go into the *peer's* queue, which is what makes this a transfer
    /// rather than a copy: this side has already given them up by the time this
    /// is called, and if the peer has gone they are dropped here rather than
    /// left in limbo.
    ///
    /// # Errors
    ///
    /// [`SocketError::Disconnected`] if there is no peer, and
    /// [`SocketError::TooManyHandles`] if its queue is full.
    pub fn send_handles(&self, handles: alloc::vec::Vec<Handle>) -> Result<(), SocketError> {
        if handles.is_empty() {
            return Ok(());
        }
        let Some(peer) = self.peer.lock().upgrade() else {
            return Err(SocketError::Disconnected);
        };
        {
            let mut queue = peer.received.lock();
            if queue.len() + handles.len() > MAX_HANDLES_WAITING {
                return Err(SocketError::TooManyHandles);
            }
            let count = handles.len();
            queue.extend(handles);
            HANDLES_PASSED.fetch_add(count as u64, Ordering::Relaxed);
        }
        // Outside the queue's lock, and after the push, so a thread this wakes
        // finds them already there.
        peer.handles_arrived.wake_all();
        peer.watchers.signal();
        Ok(())
    }

    /// Take up to `most` handles the peer sent.
    #[must_use]
    pub fn take_handles(&self, most: usize) -> alloc::vec::Vec<Handle> {
        let mut queue = self.received.lock();
        let take = most.min(queue.len());
        queue.drain(..take).collect()
    }

    /// The queue of handles this side has been sent, to put some back.
    ///
    /// For a receiver that took handles out and then could not deliver them --
    /// a bad control buffer, say. They belong to this side by then, so they go
    /// back to the front of this queue rather than across the connection: the
    /// peer has already given them up.
    pub fn received_queue(&self) -> crate::sync::IrqSpinLockGuard<'_, VecDeque<Handle>> {
        self.received.lock()
    }
}

/// A name something is listening at, and the connections waiting there.
pub struct Listener {
    /// The path it was bound to, for reporting.
    name: String,
    /// Connections made and not yet accepted. Each is the *server's* side.
    waiting: IrqSpinLock<VecDeque<Arc<Stream>>>,
    arrivals: WaitQueue,
    watchers: Watchers,
}

impl Listener {
    /// A listener at `name`.
    #[must_use]
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: String::from(name),
            waiting: IrqSpinLock::new(VecDeque::new()),
            arrivals: WaitQueue::new(),
            watchers: Watchers::new(),
        })
    }

    /// What it is called.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Start telling `set` when a connection arrives.
    pub fn watch(&self, set: Weak<crate::waitset::WaitSet>) {
        self.watchers.add(set);
    }

    /// Whether an `accept` would return without waiting.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.waiting.lock().is_empty()
    }

    /// Connect to this listener, returning the caller's side.
    ///
    /// # Errors
    ///
    /// [`SocketError::Backlog`] when the queue is full, which is what a server
    /// that is not accepting fast enough looks like from outside.
    pub fn connect(self: &Arc<Self>) -> Result<Arc<Stream>, SocketError> {
        let (theirs, mine) = Stream::pair();
        {
            let mut waiting = self.waiting.lock();
            if waiting.len() >= MAX_BACKLOG {
                return Err(SocketError::Backlog);
            }
            waiting.push_back(theirs);
        }
        self.arrivals.wake_all();
        self.watchers.signal();
        Ok(mine)
    }

    /// Take the next connection, blocking until one arrives.
    ///
    /// # Errors
    ///
    /// [`SocketError::Cancelled`] if the thread is asked to stop.
    pub fn accept(&self) -> Result<Arc<Stream>, SocketError> {
        loop {
            let seen = self.arrivals.generation();
            if let Some(stream) = self.waiting.lock().pop_front() {
                ACCEPTED.fetch_add(1, Ordering::Relaxed);
                return Ok(stream);
            }
            if crate::sched::cancelled() {
                return Err(SocketError::Cancelled);
            }
            self.arrivals.wait_if_unchanged(seen);
        }
    }

    /// Take the next connection if there is one, without waiting.
    #[must_use]
    pub fn accept_now(&self) -> Option<Arc<Stream>> {
        let taken = self.waiting.lock().pop_front();
        if taken.is_some() {
            ACCEPTED.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }
}

/// What a socket is at the moment.
///
/// One object rather than three, because a program's descriptor does not change
/// when it calls `listen` or `connect` — the socket it made is the socket it
/// now has a connection on, and a handle table that had to swap the object
/// underneath a number would be doing something a capability system should not.
pub enum State {
    /// Made, and not yet anything else.
    New,
    /// Given a name, and not yet listening at it.
    Bound(String),
    /// Listening.
    Listening(Arc<Listener>),
    /// Connected to a peer.
    Connected(Arc<Stream>),
}

/// A socket, in whatever state it has reached.
pub struct Socket {
    state: IrqSpinLock<State>,
}

impl Default for Socket {
    fn default() -> Self {
        Self::new()
    }
}

impl Socket {
    /// A socket that is not yet anything.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: IrqSpinLock::new(State::New),
        }
    }

    /// The connection, if it has one.
    #[must_use]
    pub fn stream(&self) -> Option<Arc<Stream>> {
        match &*self.state.lock() {
            State::Connected(stream) => Some(Arc::clone(stream)),
            _ => None,
        }
    }

    /// The listener, if it is one.
    #[must_use]
    pub fn listener(&self) -> Option<Arc<Listener>> {
        match &*self.state.lock() {
            State::Listening(listener) => Some(Arc::clone(listener)),
            _ => None,
        }
    }

    /// The name it was bound to, if it has one.
    #[must_use]
    pub fn bound_to(&self) -> Option<String> {
        match &*self.state.lock() {
            State::Bound(name) => Some(name.clone()),
            State::Listening(listener) => Some(String::from(listener.name())),
            _ => None,
        }
    }

    /// Whether it has reached any state at all beyond being made.
    #[must_use]
    pub fn is_new(&self) -> bool {
        matches!(&*self.state.lock(), State::New)
    }

    /// Give it a name.
    pub fn bind(&self, name: &str) -> bool {
        let mut state = self.state.lock();
        if matches!(&*state, State::New) {
            *state = State::Bound(String::from(name));
            true
        } else {
            false
        }
    }

    /// Start listening at the name it was given.
    pub fn listen(&self, listener: Arc<Listener>) -> bool {
        let mut state = self.state.lock();
        if matches!(&*state, State::Bound(_)) {
            *state = State::Listening(listener);
            true
        } else {
            false
        }
    }

    /// Attach a connection.
    pub fn connected(&self, stream: Arc<Stream>) -> bool {
        let mut state = self.state.lock();
        if matches!(&*state, State::New | State::Bound(_)) {
            *state = State::Connected(stream);
            true
        } else {
            false
        }
    }
}
