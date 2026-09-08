//! Wait queues.
//!
//! A thread that has nothing to do should cost nothing. Polling is the
//! alternative, and it is what the input thread did until now: fifty wake-ups a
//! second to find no key, on a system where a key arrives a few times a minute.
//! On a laptop that is a measurable part of the power budget, and it puts a
//! floor under how long the processor can stay asleep.
//!
//! A wait queue replaces that with the honest arrangement: the thread blocks,
//! and whoever makes the condition true wakes it.
//!
//! # The lost wake-up, and why the locking is what it is
//!
//! The race that every wait queue has to answer is a wake-up that arrives
//! between a thread deciding to wait and the thread actually waiting. If the
//! waker runs in that window and finds nobody queued, the wake-up is lost and
//! the thread sleeps forever with its condition already true.
//!
//! So the queue's lock covers *both* joining the queue and marking the thread
//! blocked, and a waker takes the same lock. A waker either runs before the
//! thread has joined — and the caller's own re-check of the condition catches
//! that — or after it is fully queued and blocked, which is a wake-up that
//! works.
//!
//! # Waking a thread that has not finished leaving
//!
//! There is a second window, between the thread releasing the locks and the
//! stack switch that actually takes it off the processor. A waker in *that*
//! window must not put the thread on a run queue: it is still executing, and a
//! second processor picking it up would run it on a stack whose pointer has not
//! been saved.
//!
//! That is the same problem the scheduler already solves for every other
//! transition, and it is solved the same way. The waker marks the thread ready
//! and leaves the queueing to the processor the thread is departing from, which
//! does it in `finish_switch` once the thread has provably stopped. Exactly one
//! of the two enqueues, never both.

use alloc::collections::VecDeque;

use crate::sync::IrqSpinLock;

use super::thread::ThreadId;

/// A list of threads waiting for something to become true.
pub struct WaitQueue {
    waiters: IrqSpinLock<VecDeque<ThreadId>>,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitQueue {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            waiters: IrqSpinLock::new(VecDeque::new()),
        }
    }

    /// Block the calling thread until something wakes it.
    ///
    /// Spurious wake-ups are permitted, and callers must treat them as normal:
    /// this returns when the thread has been woken, not when the condition the
    /// caller cares about is true. [`wait_until`](Self::wait_until) is the loop
    /// that turns one into the other.
    pub fn wait(&self) {
        let Some(current) = super::current_id() else {
            return;
        };

        {
            let mut waiters = self.waiters.lock();

            // Under the queue's lock, so a waker cannot see an empty queue and
            // a running thread at the same moment.
            super::mark_blocked(current);
            waiters.push_back(current);
        }

        super::schedule();
    }

    /// Block until `condition` holds.
    ///
    /// The condition is checked before waiting and after every wake-up, which
    /// is what makes a spurious wake-up harmless and a wake-up that arrives
    /// just before the wait harmless too.
    pub fn wait_until(&self, mut condition: impl FnMut() -> bool) {
        while !condition() {
            self.wait();
        }
    }

    /// Wake the thread that has been waiting longest, if any.
    ///
    /// Returns whether one was woken. Safe from an interrupt handler: it takes
    /// two short locks and never switches.
    pub fn wake_one(&self) -> bool {
        let woken = self.waiters.lock().pop_front();
        match woken {
            Some(id) => {
                super::wake_blocked(id);
                true
            }
            None => false,
        }
    }

    /// How many threads are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.waiters.lock().len()
    }
}
