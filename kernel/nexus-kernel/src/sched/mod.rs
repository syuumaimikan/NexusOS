//! The Nexus Scheduler.
//!
//! Preemptive, priority-ordered, round-robin within each priority, across every
//! processor.
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
//! # What is shared and what is not
//!
//! The thread table and the run queues are shared, behind one lock. Which
//! thread is *running*, which thread to fall back to, and whether a preemption
//! is due are per-processor, held in [`percpu`] and reachable without a lock —
//! an interrupt handler cannot afford to take one to find out what it
//! interrupted.
//!
//! Shared run queues rather than per-processor ones is a deliberate first step.
//! It is what makes every processor able to run work at all, which is the
//! correctness question; per-processor queues with balancing between them is a
//! scalability question, and answering it before there is contention to measure
//! would be guessing. One lock on a four-core desktop taking a thousand
//! decisions a second is not where the time goes.
//!
//! # Locking
//!
//! The scheduler is behind one lock, and the actual stack switch happens
//! *after* that lock is released, with interrupts still masked. Holding it
//! across the switch would deadlock: the incoming thread would try to release a
//! lock the outgoing thread still owns. Masking interrupts across the gap is
//! what keeps the released-but-not-yet-switched window from being re-entered.
//!
//! # Handing over
//!
//! A thread cannot finish leaving a processor by itself. Between announcing
//! where it is going — ready, asleep, or done — and the stack switch that takes
//! it there, it is still executing. In that window another processor must not
//! be able to act on the announcement: switching to a thread whose stack
//! pointer has not been saved yet puts two cores on one stack, and freeing a
//! finished thread's stack pulls the ground from under it.
//!
//! So the departure is completed by the *incoming* thread, in
//! [`finish_switch`], which runs on the same processor immediately after the
//! switch — at which point the outgoing thread has provably stopped. Until then
//! the outgoing thread is flagged [`Thread::switching_out`] and is invisible to
//! waking and reaping.

pub mod context;
pub mod thread;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;

use crate::arch::{self, interrupts, percpu, time};
use crate::kprintln;
use crate::sync::IrqSpinLock;

use context::context_switch;
use thread::{Priority, Thread, ThreadEntry, ThreadId, ThreadState, UserStart, TIME_SLICE_TICKS};

/// Scheduler state shared by every processor.
struct Scheduler {
    threads: BTreeMap<ThreadId, Box<Thread>>,
    /// One run queue per priority level, highest index checked first.
    ///
    /// The idle threads are deliberately kept *out* of these: an idle thread
    /// must never compete with real work, and must never be unavailable. Each
    /// processor reaches its own through [`percpu::idle_thread`].
    ready: [VecDeque<ThreadId>; Priority::COUNT],
    next_id: u64,
    /// Next free slot in the kernel stack area.
    next_stack_index: u64,
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
            next_id: 1,
            next_stack_index: 0,
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

    /// Create a thread and add it to the table, without making it runnable.
    fn create(
        &mut self,
        name: &str,
        priority: Priority,
        entry: ThreadEntry,
        argument: usize,
    ) -> Result<ThreadId, SpawnError> {
        let id = ThreadId(self.next_id);
        let stack_index = self.next_stack_index;

        let thread = Thread::new(id, name, priority, entry, argument, stack_index)
            .ok_or(SpawnError::OutOfMemory)?;

        // Only after the allocation succeeded, so a failure does not burn an
        // identifier or a stack slot.
        self.next_id += 1;
        self.next_stack_index += 1;
        self.threads.insert(id, thread);
        Ok(id)
    }

    /// Adopt the context calling this as a thread, on whatever stack it is
    /// already using.
    fn adopt(&mut self, name: &str, priority: Priority) -> ThreadId {
        let id = ThreadId(self.next_id);
        self.next_id += 1;
        self.threads
            .insert(id, Thread::boot_thread(id, name, priority));
        id
    }
}

static SCHEDULER: IrqSpinLock<Scheduler> = IrqSpinLock::new(Scheduler::new());

/// Whether the scheduler has been started.
///
/// Read from the timer interrupt on every tick, and by application processors
/// waiting to join, so it is a plain atomic rather than something that would
/// need the scheduler lock.
static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Bring up the scheduler, adopting the current context as the first thread.
///
/// # Safety
///
/// Call once, after the heap is available, from the context that booted the
/// kernel. That context keeps running; it simply becomes a scheduled thread.
pub unsafe fn init() -> Result<(), SpawnError> {
    let idle_id = {
        let mut scheduler = SCHEDULER.lock();

        // Thread zero is fixed rather than allocated: the boot context is
        // reported as `#0` everywhere from the first line of the log onwards.
        let boot = Thread::boot_thread(ThreadId(0), "kernel-main", Priority::Normal);
        scheduler.threads.insert(ThreadId(0), boot);

        // The idle thread is created before anything else can block, because
        // the first moment every other thread is asleep is the moment it is
        // needed.
        scheduler.create("idle-cpu0", Priority::Background, idle_entry, 0)?
    };

    percpu::set_current_thread(0);
    percpu::set_idle_thread(idle_id.0);

    RUNNING.store(true, core::sync::atomic::Ordering::Release);
    kprintln!(
        "[sched] scheduler started; boot context is thread #0 (kernel-main), idle is {idle_id}"
    );
    Ok(())
}

/// Join the scheduler from an application processor. Never returns.
///
/// The context that calls this *becomes* the processor's idle thread rather
/// than getting a freshly allocated one. It is already sitting in exactly the
/// loop an idle thread runs, on a stack the boot processor allocated for it, so
/// a second stack would be waste.
pub fn run_idle_on_this_processor(cpu_index: usize) -> ! {
    // Application processors are started before the scheduler exists, because
    // bringing them up needs only the APIC and the heap while the scheduler
    // wants the display and the rest of early init behind it. Waiting here
    // costs this core nothing -- it has no work until there is a scheduler to
    // give it any -- and is far less delicate than reordering bring-up.
    while !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        arch::wait_for_interrupt();
    }

    let id = {
        let mut scheduler = SCHEDULER.lock();
        scheduler.adopt(&format!("idle-cpu{cpu_index}"), Priority::Background)
    };

    percpu::set_current_thread(id.0);
    percpu::set_idle_thread(id.0);
    kprintln!("[sched] processor {cpu_index} joined the scheduler as thread {id}");

    idle_entry(0);
    unreachable!("the idle thread returned")
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
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let id = scheduler.create(name, priority, entry, argument)?;
        scheduler.enqueue(id);
        id
    };

    // An idle processor should pick this up now rather than at the end of a
    // slice it is not using.
    percpu::request_reschedule_everywhere();
    Ok(id)
}

/// Create a thread that begins executing in ring 3 at `entry`.
///
/// The thread starts in the kernel like any other and leaves for user mode as
/// its first act, because something has to set up the transition and only code
/// already running on the thread's own kernel stack can.
pub fn spawn_user(name: &str, entry: u64, stack_top: u64) -> Result<ThreadId, SpawnError> {
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let id = scheduler.create(name, Priority::Normal, user_trampoline, 0)?;
        if let Some(thread) = scheduler.threads.get_mut(&id) {
            thread.user_start = Some(UserStart { entry, stack_top });
        }
        scheduler.enqueue(id);
        id
    };

    percpu::request_reschedule_everywhere();
    Ok(id)
}

/// The kernel side of a user thread: hand the processor to ring 3.
fn user_trampoline(_argument: usize) {
    let start = {
        let scheduler = SCHEDULER.lock();
        scheduler
            .threads
            .get(&ThreadId(percpu::current_thread()))
            .and_then(|thread| thread.user_start)
    };

    let Some(start) = start else {
        kprintln!("[sched] a user thread had no entry point; not entering ring 3");
        return;
    };

    // `schedule` has already pointed this processor's `rsp0` and syscall stack
    // at this thread's kernel stack, which is what the first interrupt or
    // system call out of ring 3 will land on.
    //
    // SAFETY: the caller of `spawn_user` mapped both addresses into the user
    // half. This never returns, so nothing after it can observe a half-left
    // kernel.
    unsafe { crate::user::enter(start.entry, start.stack_top) }
}

/// The thread running on this processor, if it has joined the scheduler.
///
/// Reads per-processor state rather than the scheduler itself, so it is safe to
/// call from a panic handler or an interrupt.
#[must_use]
pub fn current_id() -> Option<ThreadId> {
    match percpu::current_thread() {
        percpu::NO_THREAD => None,
        id => Some(ThreadId(id)),
    }
}

/// Entry point of every newly created thread.
///
/// A new thread's fabricated stack returns here rather than into
/// [`context_switch`]'s caller, because it has no caller to return to. This
/// reads what the thread is supposed to run, runs it, and retires the thread
/// when it comes back.
extern "sysv64" fn thread_trampoline() -> ! {
    // A first run arrives here instead of returning from `context_switch`, so
    // this is where the thread it displaced gets released.
    finish_switch();

    let (entry, argument) = {
        let scheduler = SCHEDULER.lock();
        let current = ThreadId(percpu::current_thread());
        scheduler
            .threads
            .get(&current)
            .and_then(|thread| thread.entry)
            .expect("a newly started thread must have an entry point")
    };

    entry(argument);
    exit()
}

/// Release the thread this processor switched away from.
///
/// Runs on the incoming thread, immediately after the stack switch and with
/// interrupts still masked. By this point the outgoing thread has stopped
/// executing, so what it announced before the switch can finally be acted on.
fn finish_switch() {
    let Some(previous) = percpu::take_previous() else {
        return;
    };
    let previous = ThreadId(previous);
    let idle = percpu::idle_thread();

    let mut scheduler = SCHEDULER.lock();
    let Some(thread) = scheduler.threads.get_mut(&previous) else {
        return;
    };
    thread.switching_out = false;

    match thread.state {
        // Only now is the stack it was standing on out of use, which is what
        // makes reaping it safe.
        ThreadState::Exiting => thread.state = ThreadState::Finished,
        // Runnable again, and off every queue until this moment. An idle
        // thread stays off them: it is reached only through the fallback in
        // `schedule`, and queueing it would let it be picked as ordinary work.
        ThreadState::Ready if previous.0 != idle => {
            let level = thread.priority.index();
            scheduler.ready[level].push_back(previous);
        }
        // Sleeping, or an idle thread standing down: nothing to do beyond the
        // flag, which is what makes it wakeable again.
        _ => {}
    }
}

/// Retire the running thread. Never returns.
pub fn exit() -> ! {
    {
        let mut scheduler = SCHEDULER.lock();
        let current = ThreadId(percpu::current_thread());
        if let Some(thread) = scheduler.threads.get_mut(&current) {
            // Not `Finished`: this thread is still standing on its own stack.
            // The next thread to run here marks it finished, once it has left.
            thread.state = ThreadState::Exiting;
            // Set with the state, not later in `schedule`: the two must become
            // true together, or another processor can act on the announcement
            // in the gap.
            thread.switching_out = true;
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
    let deadline = time::ticks() + time::ms_to_ticks(milliseconds);

    {
        let mut scheduler = SCHEDULER.lock();
        let current = ThreadId(percpu::current_thread());
        if let Some(thread) = scheduler.threads.get_mut(&current) {
            thread.state = ThreadState::Sleeping {
                until_tick: deadline,
            };
            // Set here rather than in `schedule`, and this one is load-bearing:
            // a deadline that has already passed makes the thread wakeable the
            // instant the state is visible, and a processor that woke it would
            // queue a thread still running here, whose saved stack pointer is
            // from its previous switch. Announcing the departure and the fact
            // that it is not complete has to be one step.
            thread.switching_out = true;
        }
    }

    schedule();
}

/// Wake every thread whose sleep deadline has passed.
///
/// Called by the processor that owns the clock. Any processor could do it, but
/// having all of them scan the thread table every millisecond would be four
/// times the lock traffic for the same answer.
pub fn wake_sleepers() {
    if !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }

    let now = time::ticks();
    let woke_something = {
        let mut scheduler = SCHEDULER.lock();
        let woken: alloc::vec::Vec<ThreadId> = scheduler
            .threads
            .values()
            .filter(|thread| thread.is_wakeable(now))
            .map(|thread| thread.id)
            .collect();
        let any = !woken.is_empty();
        for id in woken {
            scheduler.enqueue(id);
        }
        any
    };

    // A thread that just became runnable should not wait for a slice to expire,
    // least of all when some processor is sitting in idle. Which processor that
    // is, is not knowable from here.
    if woke_something {
        percpu::request_reschedule_everywhere();
    }
}

/// Account for one timer tick on this processor.
///
/// Called from the timer interrupt on every processor, so it does the minimum:
/// it takes the scheduler lock briefly and never switches. The switch happens
/// after the handler has acknowledged the interrupt.
pub fn tick() {
    if !RUNNING.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(current) = current_id() else {
        // A processor that has not joined the scheduler yet. It still takes
        // timer interrupts; it simply has no thread to charge them to.
        return;
    };

    let mut scheduler = SCHEDULER.lock();
    if let Some(thread) = scheduler.threads.get_mut(&current) {
        thread.ticks_run += 1;
        thread.slice_remaining = thread.slice_remaining.saturating_sub(1);
        if thread.slice_remaining == 0 {
            percpu::set_needs_reschedule(true);
        }
    }
}

/// Whether the thread running on this processor should be preempted.
#[must_use]
pub fn needs_reschedule() -> bool {
    RUNNING.load(core::sync::atomic::Ordering::Acquire) && percpu::needs_reschedule()
}

/// Pick the next thread for this processor and switch to it.
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
        percpu::set_needs_reschedule(false);

        let Some(current) = current_id() else {
            return;
        };
        let idle = ThreadId(percpu::idle_thread());

        let switch = {
            let mut scheduler = SCHEDULER.lock();

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
                    idle
                }
            };

            if next == current {
                if let Some(thread) = scheduler.threads.get_mut(&current) {
                    thread.state = ThreadState::Running;
                    thread.slice_remaining = TIME_SLICE_TICKS;
                }
                return;
            }

            // The outgoing thread announces where it is going, and is flagged
            // as not yet gone. It is deliberately *not* put on a run queue
            // here: it is still executing on this stack, and a queued thread
            // can be taken by another processor at any moment.
            {
                let thread = scheduler
                    .threads
                    .get_mut(&current)
                    .expect("the running thread must exist");
                if matches!(thread.state, ThreadState::Running) {
                    thread.state = ThreadState::Ready;
                }
                thread.switching_out = true;
            }
            percpu::set_previous(current.0);

            let outgoing_stack_slot = {
                let thread = scheduler
                    .threads
                    .get_mut(&current)
                    .expect("the running thread must exist");
                &mut thread.stack_pointer as *mut u64
            };

            let (incoming_stack, incoming_kernel_stack) = {
                let thread = scheduler
                    .threads
                    .get_mut(&next)
                    .expect("a queued thread must exist");
                thread.state = ThreadState::Running;
                thread.slice_remaining = TIME_SLICE_TICKS;
                thread.switches += 1;
                (thread.stack_pointer, thread.kernel_stack_top())
            };

            // Where the processor lands when it comes back from ring 3, by
            // either door. Set on every switch rather than only for user
            // threads: leaving a stale pointer here would mean an interrupt
            // from user mode landing on a stack belonging to some other thread,
            // and the cost of writing two words is nothing next to finding
            // that.
            //
            // A thread with no kernel stack of its own -- the boot context, and
            // the idle threads -- never runs in ring 3, so the processor's own
            // stack is the honest answer for it.
            let kernel_stack = incoming_kernel_stack.unwrap_or_else(percpu::kernel_stack_top);
            percpu::set_syscall_stack_top(kernel_stack);
            // SAFETY: `kernel_stack` is the top of a mapped stack, and only this
            // processor writes its own TSS.
            unsafe { arch::gdt::set_kernel_stack(percpu::cpu_index() as usize, kernel_stack) };

            percpu::set_current_thread(next.0);
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

            // Execution resumes here when something switches back to this
            // thread, on whatever processor picked it up. Whatever *that*
            // processor displaced is this thread's to release.
            finish_switch();
        }
    });
}

/// Reclaim finished threads.
///
/// Deliberately not done inside [`exit`]: a thread cannot free the stack it is
/// standing on. A thread only reaches [`ThreadState::Finished`] from
/// [`finish_switch`], which runs after it has stopped executing, so anything
/// found here is safe to drop without asking which processor it was last on.
pub fn reap_finished() -> usize {
    let mut scheduler = SCHEDULER.lock();

    let finished: alloc::vec::Vec<ThreadId> = scheduler
        .threads
        .values()
        .filter(|thread| thread.state == ThreadState::Finished)
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
    pub running: usize,
    pub context_switches: u64,
}

/// Read the current scheduler statistics.
#[must_use]
pub fn stats() -> SchedulerStats {
    let scheduler = SCHEDULER.lock();
    let mut ready = 0;
    let mut sleeping = 0;
    let mut finished = 0;
    let mut running = 0;
    for thread in scheduler.threads.values() {
        match thread.state {
            ThreadState::Ready => ready += 1,
            ThreadState::Sleeping { .. } => sleeping += 1,
            // Counted with the finished: it is done, and the only thing left is
            // the processor it is leaving noticing.
            ThreadState::Finished | ThreadState::Exiting => finished += 1,
            ThreadState::Running => running += 1,
        }
    }

    SchedulerStats {
        threads: scheduler.threads.len(),
        ready,
        sleeping,
        finished,
        running,
        context_switches: scheduler.context_switches,
    }
}

/// Print a line per thread, for diagnostics.
pub fn dump_threads() {
    let here = percpu::current_thread();
    let scheduler = SCHEDULER.lock();
    kprintln!(
        "[sched] {} threads, {} context switches",
        scheduler.threads.len(),
        scheduler.context_switches
    );
    for thread in scheduler.threads.values() {
        // Marks the thread running on *this* processor. The others are running
        // on their own, and are shown as `Running` without a marker.
        let marker = if thread.id.0 == here { '*' } else { ' ' };
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
