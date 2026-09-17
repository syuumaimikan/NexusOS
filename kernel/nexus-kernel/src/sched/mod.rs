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
pub mod wait;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;

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
        let idle = scheduler.create("idle-cpu0", Priority::Background, idle_entry, 0)?;
        if let Some(thread) = scheduler.threads.get_mut(&idle) {
            thread.is_idle = true;
        }
        idle
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
        let id = scheduler.adopt(&format!("idle-cpu{cpu_index}"), Priority::Background);
        if let Some(thread) = scheduler.threads.get_mut(&id) {
            thread.is_idle = true;
        }
        id
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
pub fn spawn_user(
    name: &str,
    entry: u64,
    stack_top: u64,
    process: Arc<crate::process::Process>,
) -> Result<ThreadId, SpawnError> {
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let id = scheduler.create(name, Priority::Normal, user_trampoline, 0)?;
        if let Some(thread) = scheduler.threads.get_mut(&id) {
            thread.user_start = Some(UserStart {
                entry,
                stack_top,
                registers: None,
            });
            thread.process = Some(process);
        }
        scheduler.enqueue(id);
        id
    };

    percpu::request_reschedule_everywhere();
    Ok(id)
}

/// Add a thread to a process that already has one.
///
/// The difference from [`spawn_user`] is the whole of what a thread is: the
/// address space is not new, the handle table is not new, and the process is not
/// new -- only the register state and the kernel stack are. Two threads of one
/// process see the same memory because they are given the same `Arc`, not
/// because anything copies anything.
///
/// `registers` is the state the thread begins in, which for a `clone` is its
/// parent's with a zero in `rax` and a stack of its own. `thread_pointer` is
/// what `fs` is to point at while it runs, which a C library sets for every
/// thread it makes and which is per-thread state the switch now carries.
///
/// The name is the process's, so that the log says which program a thread
/// belongs to rather than giving every thread of every program the same word.
pub fn spawn_user_thread(
    process: Arc<crate::process::Process>,
    registers: crate::arch::syscall::Frame,
    thread_pointer: Option<u64>,
) -> Result<ThreadId, SpawnError> {
    let name = crate::alloc::string::String::from(process.name.as_str());
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let id = scheduler.create(&name, Priority::Normal, user_trampoline, 0)?;
        if let Some(thread) = scheduler.threads.get_mut(&id) {
            thread.user_start = Some(UserStart {
                entry: registers.rip,
                stack_top: registers.rsp,
                registers: Some(registers),
            });
            // Taken from the caller if it named one, and otherwise the same one
            // the calling thread is using -- which is what a `clone` without
            // `CLONE_SETTLS` means, and is not the same as none.
            thread.thread_pointer = thread_pointer.unwrap_or_else(||
                // SAFETY: reading this processor's `IA32_FS_BASE`, which is the
                // calling thread's because it is the one running.
                unsafe { arch::syscall::read_msr(FS_BASE) });
            thread.process = Some(process);
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
    // system call out of ring 3 will land on. It has also installed this
    // thread's thread pointer, so a cloned thread's `fs` is already its own
    // before its first instruction runs.
    match start.registers {
        // A thread that begins in the middle of its parent's system call rather
        // than at an entry point. See `spawn_user_thread`.
        //
        // SAFETY: every address in the frame came from a thread that was
        // running in ring 3 in this address space, and the stack pointer was
        // checked to be a user address before the thread was created. This
        // never returns.
        Some(registers) => unsafe { crate::user::resume(&registers) },
        // A thirty-two bit program goes through a different door: a code
        // segment that makes the processor decode thirty-two bit instructions.
        // Which door is a property of the *process*, decided when it was
        // started, and is the same comparison the system-call boundary makes.
        //
        // SAFETY: the caller of `spawn_user` mapped both addresses into the user
        // half. This never returns, so nothing after it can observe a half-left
        // kernel.
        None => {
            let thirty_two_bit = current_process()
                .is_some_and(|process| process.personality == crate::process::Personality::Linux32);
            if thirty_two_bit {
                unsafe { crate::user::enter32(start.entry, start.stack_top) }
            } else {
                unsafe { crate::user::enter(start.entry, start.stack_top) }
            }
        }
    }
}

/// The process the calling thread belongs to, if it is a user thread.
///
/// The one place a system call can find out what its caller is allowed to do,
/// so it is here rather than reached for through the thread table by every
/// caller that needs it.
#[must_use]
pub fn current_process() -> Option<Arc<crate::process::Process>> {
    let current = current_id()?;
    let scheduler = SCHEDULER.lock();
    scheduler
        .threads
        .get(&current)
        .and_then(|thread| thread.process.clone())
}

/// Whether the calling thread's process has been asked to stop.
///
/// Every place a thread can wait consults this, and so does the system-call
/// boundary. A thread that finds it set leaves: it is the only thing that can,
/// because it is the only thing that knows what it is holding.
#[must_use]
pub fn cancelled() -> bool {
    current_process().is_some_and(|process| process.completion.is_cancelled())
}

/// Leave, if this thread's process has been asked to stop.
///
/// Called from the two places a thread can be made to notice: the system-call
/// boundary, and the way back to ring 3 from the timer. It never returns when
/// the flag is set -- the thread records the killed status and retires, which
/// is the only safe way for a thread to be stopped, because it is the only
/// thing that knows what it is holding.
pub fn stop_if_asked() {
    // The reference to the process is confined to this block, and that is not
    // tidiness. `exit` never returns, so nothing after it runs -- destructors
    // included. An `Arc<Process>` held across it is a reference never given
    // back, which is an address space never freed.
    {
        let Some(process) = current_process() else {
            return;
        };
        if !process.completion.is_cancelled() {
            return;
        }

        process.completion.finish(crate::process::KILLED);
        crate::kprintln!(
            "[sys ] process {} \"{}\" stopped because it was asked to",
            process.id,
            process.name.as_str()
        );
    }

    crate::arch::interrupts::disable();
    exit()
}

/// Every thread of `process` that is still going.
///
/// Identifiers rather than references, because what a caller does with the
/// answer is stop them or count them, and both of those take the scheduler's
/// lock again -- so holding anything that borrows the table across it is how a
/// deadlock gets written.
///
/// A thread that has decided to exit is *not* included, and that is the whole
/// difference between this and a list of rows in the table. A finished thread
/// stays in the table until the reaper takes it, which is some time later, and
/// a caller asking "is anyone else left" so that the last thread out can end
/// the program would be told yes by a thread that ended a millisecond ago --
/// and the program would never finish.
#[must_use]
pub fn live_threads_of_process(process: u64) -> alloc::vec::Vec<u64> {
    let scheduler = SCHEDULER.lock();
    scheduler
        .threads
        .iter()
        .filter(|(_, thread)| {
            !matches!(thread.state, ThreadState::Exiting | ThreadState::Finished)
                && thread
                    .process
                    .as_ref()
                    .is_some_and(|owner| owner.id.0 == process)
        })
        .map(|(id, _)| id.0)
        .collect()
}

/// Wake every thread of `process`, so each can notice it has been asked to stop.
///
/// Returns how many were woken. Waking a thread that is not blocked is a
/// no-op, and a thread woken with a stale entry still in some wait queue is
/// fine: the queue pops it later and finds it already awake, which is a case
/// that already happens whenever two wakers race.
pub fn wake_process_threads(process: crate::process::ProcessId) -> usize {
    let waking: alloc::vec::Vec<ThreadId> = {
        let scheduler = SCHEDULER.lock();
        scheduler
            .threads
            .iter()
            .filter(|(_, thread)| {
                thread
                    .process
                    .as_ref()
                    .is_some_and(|owner| owner.id == process)
            })
            .map(|(id, _)| *id)
            .collect()
    };

    // Outside the lock, because waking takes it again.
    let mut woken = 0;
    for id in waking {
        wake_blocked(id);
        woken += 1;
    }
    woken
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
    // this is where the thread it displaced gets released. Interrupts are still
    // masked, exactly as they are at the same point on the ordinary path, so
    // nothing can schedule again before the hand-off is complete.
    finish_switch();

    // And now the thread is a thread like any other, which means preemptible.
    // The ordinary path re-enables on the way out of `without_interrupts`;
    // there is no such caller here, so it happens explicitly.
    interrupts::enable();

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

/// Mark the running thread blocked, ready to be switched away from.
///
/// Called by [`wait::WaitQueue`] while it holds its own lock, so that joining
/// the queue and leaving the run queues look like one step to anyone waking it.
/// The `switching_out` flag goes on with the state, for the same reason it does
/// in `sleep_ms`: between here and the stack switch the thread is still
/// executing, and a waker acting on the announcement in that window would queue
/// a thread that has not saved its stack pointer.
/// `until_tick` is when the clock should wake it regardless, if ever. A thread
/// blocked with a deadline is woken by `wake_sleepers` exactly as a sleeper is,
/// and by its queue exactly as a blocked thread is; the first of the two to
/// arrive wins and the other finds it already awake.
pub(super) fn mark_blocked(id: ThreadId, until_tick: Option<u64>) {
    let mut scheduler = SCHEDULER.lock();
    if let Some(thread) = scheduler.threads.get_mut(&id) {
        thread.state = ThreadState::Blocked { until_tick };
        thread.switching_out = true;
    }
}

/// Make a blocked thread runnable again.
///
/// The enqueue is conditional, and this is the whole of the handshake with the
/// scheduler: a thread that has not yet finished leaving its processor is
/// marked ready and left alone, and `finish_switch` queues it once it has
/// stopped. Exactly one of the two does it.
pub(super) fn wake_blocked(id: ThreadId) {
    let mut scheduler = SCHEDULER.lock();
    let Some(thread) = scheduler.threads.get_mut(&id) else {
        return;
    };
    if !matches!(thread.state, ThreadState::Blocked { .. }) {
        // Already awake: two wakers raced, or the thread was woken and has not
        // reached its condition check yet. Both are ordinary.
        return;
    }

    thread.state = ThreadState::Ready;
    if thread.switching_out {
        return;
    }
    let level = thread.priority.index();
    scheduler.ready[level].push_back(id);
    drop(scheduler);

    // Whichever processor is idle should take it now rather than at the end of
    // a slice it is not using.
    percpu::request_reschedule_everywhere();
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
                // Thread-local storage, saved here rather than written back by
                // whoever set it: a thread may change its own `fs` at any point
                // between two switches, and the register is the only place that
                // knows. See `Thread::thread_pointer`.
                //
                // SAFETY: reading this processor's `IA32_FS_BASE`.
                thread.thread_pointer = unsafe { arch::syscall::read_msr(FS_BASE) };
                // And the vector registers, for the same reason and with the
                // same consequence if it is skipped: the outgoing thread's are
                // in the processor, and nothing else knows them.
                //
                // SAFETY: the unit is enabled on every processor before
                // anything is scheduled on it -- see `arch::fpu::enable`.
                unsafe { thread.floating_point.save() };
                &mut thread.stack_pointer as *mut u64
            };

            let (incoming_stack, incoming_kernel_stack, incoming_root, incoming_fs) = {
                let thread = scheduler
                    .threads
                    .get_mut(&next)
                    .expect("a queued thread must exist");
                thread.state = ThreadState::Running;
                thread.slice_remaining = TIME_SLICE_TICKS;
                thread.switches += 1;
                // Restored here, under the scheduler's lock and before the
                // stack switch, because after the switch this is the *other*
                // thread's code and the entry it would have to reach for is
                // gone.
                //
                // SAFETY: as in the save above; the image is one `fxsave` wrote
                // or `State::new` built.
                unsafe { thread.floating_point.restore() };
                (
                    thread.stack_pointer,
                    thread.kernel_stack_top(),
                    thread.page_table_root(),
                    thread.thread_pointer,
                )
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

            // The incoming thread's address space, or the kernel's when it
            // has none. Always set, never left alone: a processor that kept the
            // previous thread's `cr3` would be running kernel code in a space
            // that is about to be freed, and would see the wrong user memory
            // the moment it looked at any.
            //
            // `activate_root` skips the write when it is already right, which
            // matters: writing `cr3` discards every non-global translation, so
            // doing it on switches that stay inside one space would throw away
            // a working set for nothing.
            let root = incoming_root.unwrap_or_else(crate::memory::address_space::kernel_root);
            // SAFETY: every space maps the whole kernel upper half, which is
            // where this code and its stack live.
            unsafe { crate::memory::address_space::activate_root(root) };

            // And the incoming thread's thread pointer. Always written, never
            // skipped when it is zero: a thread that has never set one must not
            // be handed the last thread's, which is the whole reason this is
            // part of the switch.
            //
            // SAFETY: writing this processor's `IA32_FS_BASE`, which is only
            // ever a user address -- `arch_prctl` checks that before it accepts
            // one, and a thread that has not set one has zero here.
            unsafe { arch::syscall::write_msr(FS_BASE, incoming_fs) };

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

/// `IA32_FS_BASE`: what `fs`-relative addressing is relative to.
///
/// Named here as well as in the compatibility layer because the two use it for
/// different halves of the same job -- that one sets it when a program asks,
/// and this one moves it from thread to thread so that what a program asked for
/// stays true.
const FS_BASE: u32 = 0xC000_0100;

/// Reclaim finished threads.
///
/// Deliberately not done inside [`exit`]: a thread cannot free the stack it is
/// standing on. A thread only reaches [`ThreadState::Finished`] from
/// [`finish_switch`], which runs after it has stopped executing, so anything
/// found here is safe to drop without asking which processor it was last on.
pub fn reap_finished() -> usize {
    // Taken out of the table under the lock, and dropped *outside* it. The
    // distinction is not tidiness: dropping a thread can drop the last
    // reference to its process, which drops its handle table, which drops the
    // endpoints in it -- and an endpoint's `Drop` wakes whoever was waiting on
    // the other end, which takes this very lock. Doing it in here deadlocked
    // the machine the first time a process held a channel someone was blocked
    // on, and the symptom was a system that ran every test correctly and then
    // simply stopped.
    let reaped: alloc::vec::Vec<alloc::boxed::Box<Thread>> = {
        let mut scheduler = SCHEDULER.lock();

        let finished: alloc::vec::Vec<ThreadId> = scheduler
            .threads
            .values()
            .filter(|thread| thread.state == ThreadState::Finished)
            .map(|thread| thread.id)
            .collect();

        finished
            .into_iter()
            .filter_map(|id| scheduler.threads.remove(&id))
            .collect()
    };

    // Dropping each `Box<Thread>` drops its `KernelStack`, which unmaps the
    // pages and returns the frames, and its process, which may be the last
    // reference to an address space.
    reaped.len()
}

/// A snapshot of scheduler activity.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerStats {
    pub threads: usize,
    pub ready: usize,
    pub sleeping: usize,
    /// Waiting on a wait queue, which no amount of time will end.
    pub blocked: usize,
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
    let mut blocked = 0;
    let mut finished = 0;
    let mut running = 0;
    for thread in scheduler.threads.values() {
        match thread.state {
            ThreadState::Ready => ready += 1,
            ThreadState::Sleeping { .. } => sleeping += 1,
            ThreadState::Blocked { .. } => blocked += 1,
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
        blocked,
        finished,
        running,
        context_switches: scheduler.context_switches,
    }
}

/// Check the invariant that binds thread state to the run queues.
///
/// A thread whose state is [`ThreadState::Ready`] must be on a run queue,
/// unless it is a processor's idle thread, which is reached by a different
/// route entirely. A `Ready` thread that is on no queue is invisible to the
/// scheduler: it is not running, nothing will ever pick it, and nothing else
/// about the system looks wrong. That failure has been seen once and not
/// reproduced, so this is a permanent check rather than a temporary one.
///
/// Returns the number of threads in that state, and reports them.
pub fn check_run_queues() -> usize {
    let scheduler = SCHEDULER.lock();

    let queued: usize = scheduler.ready.iter().map(VecDeque::len).sum();
    let ready: usize = scheduler
        .threads
        .values()
        .filter(|thread| thread.state == ThreadState::Ready)
        .count();

    // Idle threads sit in `Ready` while their processor runs something else,
    // and are deliberately never queued.
    let idle_ready = scheduler
        .threads
        .values()
        .filter(|thread| thread.state == ThreadState::Ready && thread.is_idle)
        .count();

    let expected = ready.saturating_sub(idle_ready);
    if expected == queued {
        return 0;
    }

    kprintln!("[sched] INVARIANT: {expected} threads are ready but {queued} are queued");
    for thread in scheduler.threads.values() {
        if thread.state == ThreadState::Ready {
            kprintln!(
                "[sched]   {} {:<16} {:?} ready, {} ticks, {} switches, switching_out {}",
                thread.id,
                thread.name.as_str(),
                thread.priority,
                thread.ticks_run,
                thread.switches,
                thread.switching_out
            );
        }
    }
    expected.abs_diff(queued)
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
