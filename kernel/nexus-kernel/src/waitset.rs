//! Waiting for whichever of several things happens first.
//!
//! Every blocking call so far names one object. A thread reads *this* channel,
//! or waits for *that* process, and while it does it can do nothing else. That
//! is enough for a program with one thing to do and it is not enough for a
//! server: something holding channels to four clients cannot serve the second
//! one while blocked on the first, and a thread per client is a thread per
//! client -- the arrangement that stops scaling first and hides deadlocks in
//! the meantime.
//!
//! A wait set is the answer, and it is the same answer as everywhere else here:
//! an object, held by a handle. A process puts handles into one with a key of
//! its own choosing, waits on the set, and is told which keys became ready.
//!
//! # Level-triggered, on purpose
//!
//! Waiting re-tests every member rather than remembering which of them signalled.
//! An edge -- "a message arrived" -- is a fact about a moment, and a set that
//! stored edges would have to be right about every one of them forever: an edge
//! delivered while nobody was waiting is an edge lost, and a client that never
//! gets served again. A level -- "there is a message waiting" -- is a fact about
//! now, and re-reading it costs a lock per member and cannot be lost.
//!
//! So a signal from an object is only ever a hint that something may have
//! changed. It does not have to be accurate, it does not have to be delivered
//! once, and a spurious one costs a re-poll. That is a large amount of
//! correctness bought with a small amount of work.
//!
//! # What can be waited on
//!
//! Channels and processes: the two things in this system that a thread can
//! block on. A channel is ready when it holds a message *or* its peer has gone,
//! because both are things the holder must act on and a set that reported only
//! the first would hang on a client that died. A process is ready when it has
//! ended.
//!
//! Memory objects and directories are absent because they are never not ready:
//! there is nothing to wait for.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::ipc::Endpoint;
use crate::process::Completion;
use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

/// Most members one set will hold.
///
/// A bound rather than a trust: the members are added from ring 3, they are
/// polled one after another while a lock is held, and a set with no ceiling
/// would let a process make the kernel walk a list of its own choosing.
pub const MAX_MEMBERS: usize = 64;

/// Something a set can wait on.
#[derive(Clone)]
pub enum Watched {
    /// Ready when a message is waiting, or the peer has gone.
    Channel(Arc<Endpoint>),
    /// Ready when the process has ended.
    Process(Arc<Completion>),
}

impl Watched {
    /// Whether it is ready *now*.
    fn ready(&self) -> bool {
        match self {
            Self::Channel(endpoint) => endpoint.queued() > 0 || !endpoint.peer_open(),
            Self::Process(completion) => completion.status().is_some(),
        }
    }

    /// Start telling `set` when this may have become ready.
    fn watch(&self, set: &Arc<WaitSet>) {
        let weak = Arc::downgrade(set);
        match self {
            Self::Channel(endpoint) => endpoint.watch(weak),
            Self::Process(completion) => completion.watch(weak),
        }
    }
}

/// Why a member could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitSetError {
    /// The set already holds as many members as it will.
    Full,
    /// That key is already in this set.
    DuplicateKey,
    /// No member with that key.
    NoSuchKey,
}

impl core::fmt::Display for WaitSetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Full => "the wait set is full",
            Self::DuplicateKey => "that key is already in this wait set",
            Self::NoSuchKey => "no member of this wait set has that key",
        })
    }
}

/// One thing a set is watching, and what to call it.
struct Member {
    /// The caller's name for it, returned when it is ready.
    ///
    /// The caller's and not the kernel's, because the caller is the one who has
    /// to recognise it: a handle number would make the answer a thing to look
    /// up, and a pointer would make it a thing to trust.
    key: u64,
    what: Watched,
}

/// A set of things, and somewhere to wait for any of them.
pub struct WaitSet {
    members: IrqSpinLock<Vec<Member>>,
    /// Woken by anything that may have made a member ready.
    changed: WaitQueue,
}

impl Default for WaitSet {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitSet {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            members: IrqSpinLock::new(Vec::new()),
            changed: WaitQueue::new(),
        }
    }

    /// Add something to watch, under a key of the caller's choosing.
    ///
    /// Takes `Arc<Self>` because the object being watched has to be handed a
    /// weak reference back: a set that could not be reached from what it is
    /// waiting on would have nothing to wake it.
    pub fn add(self: &Arc<Self>, key: u64, what: Watched) -> Result<(), WaitSetError> {
        {
            let mut members = self.members.lock();
            if members.len() >= MAX_MEMBERS {
                return Err(WaitSetError::Full);
            }
            if members.iter().any(|member| member.key == key) {
                return Err(WaitSetError::DuplicateKey);
            }
            members.push(Member {
                key,
                what: what.clone(),
            });
        }

        // Registered after the member is in the list, and outside its lock. A
        // signal that arrives in between is not lost: it wakes the queue, and
        // waiting re-polls the list from scratch rather than trusting what it
        // was told.
        what.watch(self);
        Ok(())
    }

    /// Stop watching whatever has this key.
    ///
    /// The object keeps its weak reference to this set until it next signals
    /// and finds nothing to report, which costs one pointless wake-up and no
    /// correctness: waiting polls the member list, and this is no longer in it.
    pub fn remove(&self, key: u64) -> Result<(), WaitSetError> {
        let mut members = self.members.lock();
        let before = members.len();
        members.retain(|member| member.key != key);
        if members.len() == before {
            return Err(WaitSetError::NoSuchKey);
        }
        Ok(())
    }

    /// How many things this set is watching.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.lock().len()
    }

    /// Whether it is watching nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many times this set has been signalled.
    ///
    /// Read *before* testing anything else, and handed back to
    /// [`wait_since`](Self::wait_since). What that buys is the same thing the
    /// wait queue's own counter buys: a signal that lands between the test and
    /// the wait is seen as a change rather than slept through.
    #[must_use]
    pub fn change_count(&self) -> u64 {
        self.changed.generation()
    }

    /// Wait until this set is signalled, or until `deadline_ticks`.
    ///
    /// Different from [`wait`](Self::wait) in what ends it: that one returns
    /// when a *member* is ready, and this one returns when anything signalled
    /// the set at all. It is for a caller that is waiting on more than this set
    /// -- the network thread waits on its card as well -- and therefore has to
    /// be woken to go and look at the other thing, whether or not this one has
    /// anything to report.
    ///
    /// Without it, such a caller blocks for ever on a set nobody writes to
    /// while the thing it was really waiting for arrives on the other side.
    pub fn wait_since(&self, seen: u64, deadline_ticks: Option<u64>) {
        match deadline_ticks {
            Some(deadline) => self.changed.wait_if_unchanged_until(seen, deadline),
            None => self.changed.wait_if_unchanged(seen),
        }
    }

    /// The keys of every member that is ready now.
    #[must_use]
    pub fn poll(&self) -> Vec<u64> {
        self.members
            .lock()
            .iter()
            .filter(|member| member.what.ready())
            .map(|member| member.key)
            .collect()
    }

    /// Something may have become ready.
    ///
    /// Called by the objects being watched. Everyone is woken rather than one:
    /// two threads waiting on the same set are waiting for different keys as
    /// far as this can tell, and waking one of them would leave the other
    /// asleep on something that had already happened.
    pub fn signal(&self) {
        self.changed.wake_all();
    }

    /// Block until at least one member is ready, and return every ready key.
    ///
    /// Returns an empty list only when the set is empty, because then nothing
    /// can ever make it ready and waiting would be waiting forever -- the same
    /// judgement `receive` makes about a channel whose peer has gone.
    pub fn wait(&self) -> Vec<u64> {
        self.wait_until(None)
    }

    /// The same, giving up at `deadline_ticks` if one is given.
    ///
    /// A timeout returns an empty list, which is deliberately the same answer
    /// as an empty set and as a cancelled thread. All three mean "nothing of
    /// yours is ready", and a caller that must distinguish them can: it knows
    /// what it put in the set, and it can read the clock.
    ///
    /// What this is for is the program that has work to do on a schedule as
    /// well as on an event -- a clock in a status bar, a spinner, anything that
    /// has to redraw while nothing is happening. Without it such a program has
    /// to poll the set and sleep, which costs a wake-up every interval whether
    /// or not the interval was the thing it was waiting for.
    #[must_use]
    pub fn wait_until(&self, deadline_ticks: Option<u64>) -> Vec<u64> {
        loop {
            // Before the poll, so a signal that lands between the poll and the
            // block is seen as a change rather than missed.
            let seen = self.changed.generation();

            let ready = self.poll();
            if !ready.is_empty() {
                return ready;
            }
            if self.is_empty() {
                return Vec::new();
            }
            // Asked to stop. Reported as an empty set for the same reason a
            // closed channel is: nothing is coming.
            if crate::sched::cancelled() {
                return Vec::new();
            }

            match deadline_ticks {
                Some(deadline) => {
                    // Tested after the poll, not before: a deadline that has
                    // passed while something *is* ready should report the thing
                    // that is ready. The caller asked to be woken by then, not
                    // to be told nothing happened.
                    if crate::arch::time::ticks() >= deadline {
                        return Vec::new();
                    }
                    self.changed.wait_if_unchanged_until(seen, deadline);
                }
                None => self.changed.wait_if_unchanged(seen),
            }
        }
    }
}

/// The list of sets an object tells when it may have become ready.
///
/// Weak, in both directions of the argument. A set that is dropped must not
/// keep a channel alive, and a channel must not be kept from closing because
/// something was once interested in it; and a set holds strong references to
/// its members, so anything else here would be a cycle that never frees.
pub struct Watchers {
    sets: IrqSpinLock<Vec<Weak<WaitSet>>>,
}

impl Default for Watchers {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchers {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sets: IrqSpinLock::new(Vec::new()),
        }
    }

    /// Start telling `set`.
    pub fn add(&self, set: Weak<WaitSet>) {
        self.sets.lock().push(set);
    }

    /// Tell every set that something may have changed here.
    ///
    /// Sets that have been dropped are cleared out as they are found, which is
    /// the only place this list ever shrinks -- and the only place it needs to,
    /// since a dead set costs one failed upgrade and then nothing.
    pub fn signal(&self) {
        // Collected under the lock and woken outside it. `signal` on a set
        // takes that set's own queue lock, and holding this one across it would
        // put two unrelated locks in an order nothing else has to respect.
        let live: Vec<Arc<WaitSet>> = {
            let mut sets = self.sets.lock();
            sets.retain(|set| set.strong_count() > 0);
            sets.iter().filter_map(Weak::upgrade).collect()
        };
        for set in live {
            set.signal();
        }
    }
}
