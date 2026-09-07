//! The Nexus Scheduler.
//!
//! Preemptive, priority-ordered, round-robin within each priority.
//!
//! # Scheduling policy
//!
//! Strict priority with round-robin inside a level: the highest non-empty run
//! queue always wins, and threads at that level take turns. For a desktop this
//! is the right default — input and compositing must never wait behind batch
//! work — with the accepted consequence that a busy high-priority thread can
//! starve everything below it. Ageing and deadline scheduling belong here later;
//! neither can be tuned honestly before there are real workloads to measure.
//!
//! # Locking
//!
//! The scheduler is behind one lock, and the actual stack switch happens
//! *after* that lock is released, with interrupts still masked. Holding it
//! across the switch would deadlock: the incoming thread would try to release a
//! lock the outgoing thread still owns. Masking interrupts across the gap is
//! what keeps the released-but-not-yet-switched window from being re-entered.

pub mod context;
pub mod thread;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};

use crate::arch::{self, interrupts, pit};
use crate::kprintln;
use crate::sync::IrqSpinLock;

use context::context_switch;
use thread::{Priority, Thread, ThreadEntry, ThreadId, ThreadState, TIME_SLICE_TICKS};

/// Scheduler state.
struct Scheduler {
    threads: BTreeMap<ThreadId, Box<Thread>>,
    /// One run queue per priority level, highest index checked first.
    ready: [VecDeque<ThreadId>; Priority::COUNT],
    current: ThreadId,
    /// The thread to run when no run queue has anything.
    ///
    /// It is deliberately kept *out* of the run queues: it must never compete
    /// with real work, and it must never be unavailable. Without it, a moment
    /// where every thread is asleep leaves the scheduler with nowhere to go.
    idle: ThreadId,
    next_id: u64,
    /// Next free slot in the kernel stack area.
    next_stack_index: u64,
    /// Set by the timer when the running thread has used up its slice.
    needs_reschedule: bool,
    context_switches: u64,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            threads: BTreeMap::new(),
            ready: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
            current: ThreadId(0),
            idle: ThreadId(0),
            next_id: 1,
            next_stack_index: 0,
            needs_reschedule: false,
            context_switches: 0,
        }
    }

    /// Take the highest-priority ready thread, if any.
    fn take_next_ready(&mut self) -> Option<ThreadId> {
        for level in (0..Priority::COUNT).rev() {
            if let Some(id) = self.ready[level].pop_front() {
                return Some(id);
            }
        }
        None
    }

    /// Put `id` on the back of its run queue.
    fn enqueue(&mut self, id: ThreadId) {
        if let Some(thread) = self.threads.get_mut(&id) {
            thread.state = ThreadState::Ready;
            let level = thread.priority.index();
            self.ready[level].push_back(id);
        }
    }
}

static SCHEDULER: IrqSpinLock<Scheduler> = IrqSpinLock::new(Scheduler::new());

/// Whether the scheduler has been started.
///
/// Read from the timer interrupt on every tick, so it is a plain atomic rather
/// than something that would need the scheduler lock.
static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The running thread's identifier, mirrored outside the lock.
///
/// The panic handler needs to name the thread that failed, and it cannot take
/// the scheduler lock to find out: the panic may well have happened while that
/// lock was held, and blocking there would cost the message entirely. An atomic
/// updated on every switch is readable from anywhere, at any time.
static CURRENT_THREAD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Bring up the scheduler, adopting the current context as the first thread.
///
/// # Safety
///
/// Call once, after the heap is available, from the context that booted the
/// kernel. That context keeps running; it simply becomes a scheduled thread.
pub unsafe fn init() -> Result<(), SpawnError> {
    {
        let mut scheduler = SCHEDULER.lock();
        let boot = Thread::boot_thread(ThreadId(0), "kernel-main");
        scheduler.threads.insert(ThreadId(0), boot);
        scheduler.current = ThreadId(0);
    }

    // The idle thread is created before anything else can block, because the
    // first moment every other thread is asleep is the moment it is needed.
    let idle_id = {
        let mut scheduler = SCHEDULER.lock();
        let id = ThreadId(scheduler.next_id);
        let stack_index = scheduler.next_stack_index;
        scheduler.next_id += 1;
        scheduler.next_stack_index += 1;

        let thread = Thread::new(
            id,
            "idle",
            thread::Priority::Background,
            idle_entry,
            0,
            stack_index,
        )
        .ok_or(SpawnError::OutOfMemory)?;

        scheduler.threads.insert(id, thread);
        // Deliberately not enqueued: `take_next_ready` never returns it, and
        // the scheduler falls back to it only when every queue is empty.
        scheduler.idle = id;
        id
    };

    RUNNING.store(true, core::sync::atomic::Ordering::Release);
    kprintln!(
        "[sched] scheduler started; boot context is thread #0 (kernel-main), idle is {idle_id}"
    );
    Ok(())
}

/// The idle thread's body.
///
/// Halts until an interrupt arrives, then reschedules if anything became
/// runnable. It never sleeps, never blocks and never exits, which is what makes
/// it a dependable fallback.
fn idle_entry(_argument: usize) {
    loop {
        arch::wait_for_interrupt();
        if needs_reschedule() {
            schedule();
        }
    }
}

/// Why a thread could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// No memory for the thread's stack.
    OutOfMemory,
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("out of memory for a thread stack")
    }
}

/// Create a thread that will run `entry(argument)` and make it runnable.
pub fn spawn(
    name: &str,
    priority: Priority,
    entry: ThreadEntry,
    argument: usize,
) -> Result<ThreadId, SpawnError> {
    let mut scheduler = SCHEDULER.lock();

    let id = ThreadId(scheduler.next_id);
    let stack_index = scheduler.next_stack_index;
    scheduler.next_id += 1;
    scheduler.next_stack_index += 1;

    let thread = Thread::new(id, name, priority, entry, argument, stack_index)
        .ok_or(SpawnError::OutOfMemory)?;

    scheduler.threads.insert(id, thread);
    scheduler.enqueue(id);
    Ok(id)
}

/// The identifier of the thread running on this processor.
///
/// Reads a mirror of the scheduler's state rather than the scheduler itself, so
/// it is safe to call from a panic handler or an interrupt.
#[must_use]
pub fn current_id() -> ThreadId {
    ThreadId(CURRENT_THREAD.load(core::sync::atomic::Ordering::Relaxed))
}

/// Entry point of every newly created thread.
///
/// A new thread's fabricated stack returns here rather than into
/// [`context_switch`]'s caller, because it has no caller to return to. This
/// reads what the thread is supposed to run, runs it, and retires the thread
/// when it comes back.
extern "sysv64" fn thread_trampoline() -> ! {
    let (entry, argument) = {
        let scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        scheduler
            .threads
            .get(&current)
            .and_then(|thread| thread.entry)
            .expect("a newly started thread must have an entry point")
    };

    entry(argument);
    exit()
}

/// Retire the running thread. Never returns.
pub fn exit() -> ! {
    {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        if let Some(thread) = scheduler.threads.get_mut(&current) {
            thread.state = ThreadState::Finished;
        }
    }

    // Hand the processor to someone else. The finished thread is not requeued,
    // so this never comes back.
    schedule();
    unreachable!("a finished thread was scheduled again")
}

/// Give up the rest of this thread's time slice.
pub fn yield_now() {
    schedule();
}

/// Block this thread for `milliseconds`.
///
/// The thread leaves the run queues entirely and is put back when the tick
/// counter passes its deadline, so a sleeping thread costs nothing.
pub fn sleep_ms(milliseconds: u64) {
    let frequency = pit::frequency_hz().max(1);
    let ticks = (milliseconds * frequency).div_ceil(1000);
    let deadline = pit::ticks() + ticks;

    {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        if let Some(thread) = scheduler.threads.get_mut(&current) {
            thread.state = ThreadState::Sleeping {
                until_tick: deadline,
            };
        }
    }

    schedule();
}

/// Account for one timer tick, waking sleepers and marking the running thread
/// for preemption when its slice runs out.
///
/// Called from the timer interrupt, so it does the minimum: it takes the
/// scheduler lock briefly and never switches. The switch happens after the
/// handler has acknowledged the interrupt.
pub fn tick() {
    if !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }

    let now = pit::ticks();
    let mut scheduler = SCHEDULER.lock();

    // Wake anything whose deadline has passed.
    let woken: alloc::vec::Vec<ThreadId> = scheduler
        .threads
        .values()
        .filter(|thread| thread.is_wakeable(now))
        .map(|thread| thread.id)
        .collect();
    let woke_something = !woken.is_empty();
    for id in woken {
        scheduler.enqueue(id);
    }
    // A thread that just became runnable should not wait for the current
    // slice to expire, least of all when the processor is sitting in idle.
    if woke_something {
        scheduler.needs_reschedule = true;
    }

    let current = scheduler.current;
    if let Some(thread) = scheduler.threads.get_mut(&current) {
        thread.ticks_run += 1;
        thread.slice_remaining = thread.slice_remaining.saturating_sub(1);
        if thread.slice_remaining == 0 {
            scheduler.needs_reschedule = true;
        }
    }
}

/// Whether the running thread should be preempted at the first opportunity.
#[must_use]
pub fn needs_reschedule() -> bool {
    if !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        return false;
    }
    SCHEDULER.lock().needs_reschedule
}

/// Pick the next thread and switch to it.
///
/// Returns once this thread is scheduled again — or never, for a thread that
/// has finished.
pub fn schedule() {
    if !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }

    // Interrupts stay masked across the whole decision *and* the switch. The
    // lock is released before the switch, and the gap between the two must not
    // be re-entered by a timer interrupt that would try to schedule again.
    interrupts::without_interrupts(|| {
        let switch = {
            let mut scheduler = SCHEDULER.lock();
            scheduler.needs_reschedule = false;

            let current = scheduler.current;
            let current_state = scheduler
                .threads
                .get(&current)
                .map(|thread| thread.state)
                .unwrap_or(ThreadState::Finished);

            let next = match scheduler.take_next_ready() {
                Some(id) => id,
                None => {
                    // Nothing is queued. A thread that can still run simply
                    // keeps the processor rather than paying for a switch to
                    // idle and straight back.
                    if matches!(current_state, ThreadState::Running) {
                        if let Some(thread) = scheduler.threads.get_mut(&current) {
                            thread.slice_remaining = TIME_SLICE_TICKS;
                        }
                        return;
                    }
                    // The running thread has blocked or finished, so the
                    // processor goes to idle until something wakes.
                    scheduler.idle
                }
            };

            if next == current {
                if let Some(thread) = scheduler.threads.get_mut(&current) {
                    thread.slice_remaining = TIME_SLICE_TICKS;
                }
                return;
            }

            // Requeue the outgoing thread unless it is sleeping or finished.
            //
            // The idle thread is excluded: it is reached only through the
            // fallback above, and putting it on a run queue would let it be
            // selected as ordinary work. It is marked ready so its state stays
            // truthful for diagnostics.
            if matches!(current_state, ThreadState::Running | ThreadState::Ready) {
                if current == scheduler.idle {
                    if let Some(thread) = scheduler.threads.get_mut(&current) {
                        thread.state = ThreadState::Ready;
                    }
                } else {
                    scheduler.enqueue(current);
                }
            }

            let outgoing_stack_slot = {
                let thread = scheduler
                    .threads
                    .get_mut(&current)
                    .expect("the running thread must exist");
                &mut thread.stack_pointer as *mut u64
            };

            let incoming_stack = {
                let thread = scheduler
                    .threads
                    .get_mut(&next)
                    .expect("a queued thread must exist");
                thread.state = ThreadState::Running;
                thread.slice_remaining = TIME_SLICE_TICKS;
                thread.switches += 1;
                thread.stack_pointer
            };

            scheduler.current = next;
            CURRENT_THREAD.store(next.0, core::sync::atomic::Ordering::Relaxed);
            scheduler.context_switches += 1;

            Some((outgoing_stack_slot, incoming_stack))
        };

        // The scheduler lock is released here, before the switch.
        if let Some((save_to, load)) = switch {
            // SAFETY: `save_to` points into a `Box<Thread>` that the scheduler
            // owns and keeps alive; `load` is either a stack saved by a
            // previous switch or one fabricated by `Thread::prepare_stack`.
            // Interrupts are masked, as this function requires.
            unsafe { context_switch(save_to, load) };
        }
    });
}

/// Reclaim finished threads.
///
/// Deliberately not done inside [`exit`]: a thread cannot free the stack it is
/// standing on. Reaping happens later, from another thread, once the finished
/// thread is no longer running anywhere.
pub fn reap_finished() -> usize {
    let mut scheduler = SCHEDULER.lock();
    let current = scheduler.current;

    let finished: alloc::vec::Vec<ThreadId> = scheduler
        .threads
        .values()
        .filter(|thread| thread.state == ThreadState::Finished && thread.id != current)
        .map(|thread| thread.id)
        .collect();

    let count = finished.len();
    for id in finished {
        // Dropping the `Box<Thread>` drops its `KernelStack`, which unmaps the
        // pages and returns the frames.
        scheduler.threads.remove(&id);
    }
    count
}

/// A snapshot of scheduler activity.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerStats {
    pub threads: usize,
    pub ready: usize,
    pub sleeping: usize,
    pub finished: usize,
    pub context_switches: u64,
}

/// Read the current scheduler statistics.
#[must_use]
pub fn stats() -> SchedulerStats {
    let scheduler = SCHEDULER.lock();
    let mut ready = 0;
    let mut sleeping = 0;
    let mut finished = 0;
    for thread in scheduler.threads.values() {
        match thread.state {
            ThreadState::Ready => ready += 1,
            ThreadState::Sleeping { .. } => sleeping += 1,
            ThreadState::Finished => finished += 1,
            ThreadState::Running => {}
        }
    }

    SchedulerStats {
        threads: scheduler.threads.len(),
        ready,
        sleeping,
        finished,
        context_switches: scheduler.context_switches,
    }
}

/// Print a line per thread, for diagnostics.
pub fn dump_threads() {
    let scheduler = SCHEDULER.lock();
    kprintln!(
        "[sched] {} threads, {} context switches",
        scheduler.threads.len(),
        scheduler.context_switches
    );
    for thread in scheduler.threads.values() {
        let marker = if thread.id == scheduler.current {
            '*'
        } else {
            ' '
        };
        kprintln!(
            "[sched] {marker}{} {:<16} {:?} {:?}, {} ticks, {} switches",
            thread.id,
            thread.name.as_str(),
            thread.priority,
            thread.state,
            thread.ticks_run,
            thread.switches
        );
    }
}
