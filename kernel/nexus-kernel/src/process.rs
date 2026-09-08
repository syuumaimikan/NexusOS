//! Processes.
//!
//! Until now a user program was an address space and a thread that happened to
//! run in it, which is most of what a process is and none of what makes the
//! word useful. A process is the thing that *holds* — an address space, a
//! handle table, and eventually threads, memory and a name. Anything a thread
//! is allowed to do that is not simply arithmetic, it is allowed to do because
//! of the process it belongs to.
//!
//! # Shared, and why
//!
//! Threads hold an [`Arc<Process>`]. A process outlives whichever of its
//! threads is reaped first, and the last reference going away is what frees the
//! address space — by which point no processor can still have it loaded, since
//! a thread must be switched away from before it can be reaped, and every
//! switch sets `cr3`.
//!
//! # Ending
//!
//! A process ends by saying so, with a number. Whoever holds a handle to it can
//! wait for that and read the number, which is the whole of what "the program
//! finished, and here is whether it worked" needs to be.
//!
//! The handle names a [`Completion`] and not the process itself, and the
//! difference matters: a handle to the process would keep its address space
//! alive for as long as anyone remembered it, so a parent that never closed one
//! would be a memory leak shaped like politeness. The completion is a name, an
//! identifier and an outcome -- three words and a wait queue -- and it outlives
//! the process by design.
//!
//! # What is deliberately absent
//!
//! No parent, no children, no process groups, no signals, and no way to end a
//! process other than its own thread returning. A process is created by asking
//! whoever is allowed to answer, not by a system call that trusts the asker.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::ipc::HandleTable;
use crate::memory::address_space::AddressSpace;
use crate::sched::wait::WaitQueue;

/// A process identifier.
///
/// Distinct from a thread identifier on purpose: they are different things and
/// will diverge as soon as a process has more than one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessId(pub u64);

impl core::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// What became of a process, and somewhere to wait for it.
///
/// Separate from [`Process`] so that holding one does not hold the address
/// space: a handle to a finished process should cost a name and a number, not
/// the memory the program was running in.
pub struct Completion {
    pub id: ProcessId,
    pub name: String,
    /// Taken by whoever gets to decide the status. Exclusion only; it says
    /// nothing about whether the status has been written yet.
    claimed: AtomicBool,
    /// The publication point. A reader that sees this set is guaranteed to see
    /// the status stored before it, which is why the two flags are not one.
    finished: AtomicBool,
    status: AtomicU64,
    /// Threads waiting for it to end.
    waiters: WaitQueue,
}

impl Completion {
    /// Record that the process ended, and wake everyone waiting.
    ///
    /// Idempotent, because a process with more than one thread will one day
    /// have more than one way to reach here, and the first ending is the one
    /// that counts.
    pub fn finish(&self, status: u64) {
        // Claimed first, so exactly one caller ever stores a status and a
        // second one cannot overwrite the first process's answer with its own.
        if self.claimed.swap(true, Ordering::Relaxed) {
            return;
        }
        self.status.store(status, Ordering::Relaxed);
        // Published second. A waiter that sees this cannot read a status that
        // has not been written, which is what the two flags buy.
        self.finished.store(true, Ordering::Release);
        while self.waiters.wake_one() {}
    }

    /// The status, if it has ended.
    #[must_use]
    pub fn status(&self) -> Option<u64> {
        if self.finished.load(Ordering::Acquire) {
            Some(self.status.load(Ordering::Relaxed))
        } else {
            None
        }
    }

    /// Block until it ends, and return the status.
    ///
    /// Rechecked after every wake-up rather than trusted, which is what makes a
    /// wake-up that arrives before the wait harmless.
    pub fn wait(&self) -> u64 {
        loop {
            if let Some(status) = self.status() {
                return status;
            }
            self.waiters.wait();
        }
    }
}

/// Everything a thread is allowed to reach by virtue of what it belongs to.
pub struct Process {
    pub id: ProcessId,
    pub name: String,
    /// The page tables its threads run on.
    pub address_space: Arc<AddressSpace>,
    /// The objects it may name, and what it may do with them.
    pub handles: HandleTable,
    /// Where its ending is recorded, and who is waiting for it.
    pub completion: Arc<Completion>,
}

/// Next process identifier. Never reused, so a stale identifier in a log names
/// nothing rather than something else.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Processes created and destroyed.
static CREATED: AtomicU64 = AtomicU64::new(0);
static DESTROYED: AtomicU64 = AtomicU64::new(0);

impl Process {
    /// Create a process around an address space.
    #[must_use]
    pub fn new(name: &str, address_space: Arc<AddressSpace>) -> Arc<Self> {
        CREATED.fetch_add(1, Ordering::Relaxed);
        let id = ProcessId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        Arc::new(Self {
            id,
            name: String::from(name),
            address_space,
            handles: HandleTable::new(),
            completion: Arc::new(Completion {
                id,
                name: String::from(name),
                claimed: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                status: AtomicU64::new(0),
                waiters: WaitQueue::new(),
            }),
        })
    }

    /// Physical root of its page tables.
    #[must_use]
    pub fn page_table_root(&self) -> u64 {
        self.address_space.root()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        DESTROYED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Processes created and destroyed since boot.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        CREATED.load(Ordering::Relaxed),
        DESTROYED.load(Ordering::Relaxed),
    )
}
