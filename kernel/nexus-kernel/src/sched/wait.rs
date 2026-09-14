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
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::IrqSpinLock;

use super::thread::ThreadId;

/// A list of threads waiting for something to become true.
pub struct WaitQueue {
    waiters: IrqSpinLock<VecDeque<ThreadId>>,
    /// Bumped by every wake, under the queue's lock.
    ///
    /// A condition worth waiting on lives behind its own locks -- an inbox, a
    /// peer, a status word -- and testing it while holding this queue's lock
    /// would mean taking those in the opposite order from the sender, which is
    /// a deadlock rather than a race. So the condition is tested outside, and
    /// this counter is what makes that safe: a waiter reads it before testing,
    /// and blocks only if it has not moved since. A waker that ran in the gap
    /// moved it, and the waiter loops round to test again instead of sleeping
    /// through the very thing it was waiting for.
    generation: AtomicU64,
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
            generation: AtomicU64::new(0),
        }
    }

    /// What the queue's wake counter reads now.
    ///
    /// Take this *before* testing a condition, and hand it to
    /// [`wait_if_unchanged`](Self::wait_if_unchanged).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Block, unless something has been woken since `seen`.
    ///
    /// There is deliberately no plain `wait()` here. One existed, and it was a
    /// lost wake-up waiting to happen: a caller that tested its condition and
    /// then called it had a window in which the thing it was waiting for could
    /// arrive, wake an empty queue, and leave it asleep for ever. Every waiter
    /// takes the counter first, so that window closes.
    ///
    /// The comparison happens under the queue's lock, together with joining it,
    /// so a waker is either entirely before this -- and the counter has moved,
    /// and this returns without blocking -- or entirely after, and finds the
    /// thread queued. There is no third case, which is the whole point.
    pub fn wait_if_unchanged(&self, seen: u64) {
        let Some(current) = super::current_id() else {
            return;
        };

        {
            let mut waiters = self.waiters.lock();
            if self.generation.load(Ordering::Acquire) != seen {
                return;
            }
            super::mark_blocked(current, None);
            waiters.push_back(current);
        }

        super::schedule();
    }

    /// Block, unless something has been woken since `seen`, and give up at
    /// `deadline_ticks` whether or not anything does.
    ///
    /// The same handshake as [`wait_if_unchanged`](Self::wait_if_unchanged),
    /// with the clock added as a second waker. What it is for is the caller who
    /// has something to do *anyway*: a program drawing a clock is waiting for a
    /// keystroke that may never arrive and still has to redraw every second,
    /// and the alternative -- a short sleep in a loop, re-checking -- is the
    /// polling this whole module exists to avoid.
    ///
    /// A deadline already past still blocks briefly and is woken on the next
    /// tick. Callers get a wake-up that is at worst one tick late, never one
    /// that never comes.
    pub fn wait_if_unchanged_until(&self, seen: u64, deadline_ticks: u64) {
        let Some(current) = super::current_id() else {
            return;
        };

        {
            let mut waiters = self.waiters.lock();
            if self.generation.load(Ordering::Acquire) != seen {
                return;
            }
            super::mark_blocked(current, Some(deadline_ticks));
            waiters.push_back(current);
        }

        super::schedule();

        // Woken by the clock rather than by this queue leaves the thread still
        // listed here, and a stale waiter is not free: `wake_one` would spend
        // its wake on a thread that is already running and leave a real waiter
        // asleep. So it takes itself off. The list is short -- the number of
        // threads waiting on one object -- and this runs once per timed wait.
        let mut waiters = self.waiters.lock();
        if let Some(at) = waiters.iter().position(|id| *id == current) {
            waiters.remove(at);
        }
    }

    /// Block until `condition` holds, or until this thread is asked to stop.
    ///
    /// The condition is checked before waiting and after every wake-up, which
    /// is what makes a spurious wake-up harmless and a wake-up that arrives
    /// just before the wait harmless too.
    ///
    /// It can also return with the condition *false*, when the calling thread's
    /// process has been asked to stop. Callers must re-check what they were
    /// waiting for rather than assume this returning means they got it -- which
    /// they already had to, because a wake-up here has never been a promise.
    /// Without this a killed thread wakes, finds its condition still false, and
    /// goes back to sleep forever: waking it is not the same as letting it
    /// leave.
    pub fn wait_until(&self, mut condition: impl FnMut() -> bool) {
        loop {
            // Read first, test second, block third. Reading the counter after
            // the test would leave exactly the window this closes: a waker
            // between the two would be neither seen by the test nor recorded in
            // the counter, and the thread would sleep with its condition true.
            let seen = self.generation();
            if condition() {
                return;
            }
            if super::cancelled() {
                return;
            }
            self.wait_if_unchanged(seen);
        }
    }

    /// Wake the thread that has been waiting longest, if any.
    ///
    /// Returns whether one was woken. Safe from an interrupt handler: it takes
    /// two short locks and never switches.
    pub fn wake_one(&self) -> bool {
        let woken = {
            let mut waiters = self.waiters.lock();
            // Under the same lock as the comparison in `wait_if_unchanged`, so
            // that "the counter moved" and "the thread was queued" cannot both
            // be false for the same wake.
            self.generation.fetch_add(1, Ordering::Release);
            waiters.pop_front()
        };
        match woken {
            Some(id) => {
                super::wake_blocked(id);
                true
            }
            None => false,
        }
    }

    /// Wake everyone waiting.
    ///
    /// What a condition that becomes true *for good* wants: a process ending,
    /// a channel closing. Waking one thread there would leave the rest asleep
    /// on something that will never happen again.
    pub fn wake_all(&self) -> usize {
        let woken = {
            let mut waiters = self.waiters.lock();
            self.generation.fetch_add(1, Ordering::Release);
            core::mem::take(&mut *waiters)
        };
        let count = woken.len();
        for id in woken {
            super::wake_blocked(id);
        }
        count
    }

    /// How many threads are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.waiters.lock().len()
    }
}
