//! Threads, for a program built for Linux.
//!
//! A C library makes a thread with `clone`, and it makes a mutex with `futex`.
//! Neither is optional: glibc starts a thread before `main` in several
//! configurations, and every lock in every library is a futex with a fast path
//! in user space and this call underneath it. A translation layer without them
//! runs single-threaded programs and nothing else.
//!
//! # What a Linux thread is here
//!
//! A Nexus thread in the same Nexus process. That is not an analogy: `clone`
//! calls [`crate::sched::spawn_user_thread`], which puts another thread in the
//! process the caller already belongs to, holding the same `Arc<AddressSpace>`
//! and the same handle table. Two Linux threads see the same memory and the
//! same file descriptors because they are the same process, not because
//! anything here copies or synchronises anything.
//!
//! That is also the rule this directory is built on, applied to threads: there
//! is nothing a translated thread can do that a Nexus thread could not, and
//! nothing below this layer knows Linux exists.
//!
//! # What is refused, and why
//!
//! **`fork`.** A `clone` without `CLONE_VM` is a request for a second process
//! with a *copy* of this one's address space. Copying an address space means
//! copy-on-write, which means a fault handler that knows about it, and this
//! system has neither. Refused with `ENOSYS`, which is a real answer a real
//! program handles -- rather than quietly making a thread, which would be two
//! "processes" sharing one heap and would corrupt both.
//!
//! **A thread in another process.** There is no such request in Linux and there
//! is none here.
//!
//! # What a futex is, and what this one is not
//!
//! `FUTEX_WAIT` is "sleep unless the word at this address has already changed",
//! and the "unless" is the whole point: a waiter that checked and then slept
//! would miss a wake that landed in between, and would sleep for ever holding a
//! lock nobody can take. So the comparison and the decision to sleep happen
//! under one lock here, and a waker takes the same lock.
//!
//! What this is not is a futex keyed by *physical* address. Linux keys a shared
//! futex by the page it lives in, so two processes mapping one page share a
//! lock. This keys by process and virtual address, so a futex in shared memory
//! between two processes does not work -- and `FUTEX_WAIT` on one is refused
//! rather than silently private, because a lock that looks taken to one side
//! and free to the other is worse than a lock that fails.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

use super::linux::error;

/// What `clone` may be asked for.
pub mod flag {
    /// Share the address space. Without it this would be `fork`.
    pub const VM: u64 = 0x0000_0100;
    /// Share the file descriptor table.
    pub const FILES: u64 = 0x0000_0400;
    /// Share signal handlers.
    pub const SIGHAND: u64 = 0x0000_0800;
    /// Be a thread of the same thread group, rather than a child process.
    pub const THREAD: u64 = 0x0001_0000;
    /// Set the new thread's thread pointer from the `tls` argument.
    pub const SETTLS: u64 = 0x0008_0000;
    /// Write the new thread's identifier where the parent said.
    pub const PARENT_SETTID: u64 = 0x0010_0000;
    /// Clear a word in the child's memory when it exits, and wake anything
    /// waiting on it. Accepted and recorded; see `CHILD_CLEARED`.
    pub const CHILD_CLEARTID: u64 = 0x0020_0000;
    /// Write the new thread's identifier where the child can read it.
    pub const CHILD_SETTID: u64 = 0x0100_0000;
}

/// What `futex` may be asked to do.
mod op {
    pub const WAIT: u64 = 0;
    pub const WAKE: u64 = 1;
    /// The same two, promising the futex is not in shared memory. Every lock
    /// inside one process carries this, and it is the only kind that works
    /// here -- see the note at the top of the file.
    pub const PRIVATE: u64 = 128;
    /// `FUTEX_WAIT_BITSET`, which a C library uses for a timed wait with an
    /// absolute deadline. Treated as `FUTEX_WAIT` with its own timeout rules.
    pub const WAIT_BITSET: u64 = 9;
    pub const WAKE_BITSET: u64 = 10;
    /// Bits a caller may set that change nothing here.
    pub const CLOCK_REALTIME: u64 = 256;
}

/// Resource temporarily unavailable: what a `FUTEX_WAIT` whose word had already
/// changed returns.
const EAGAIN: u64 = (-11i64) as u64;
/// Timed out.
const ETIMEDOUT: u64 = (-110i64) as u64;
/// Operation not supported.
const EOPNOTSUPP: u64 = (-95i64) as u64;

/// The most threads one translated process may have at once.
///
/// A number, because there has to be one: each thread holds a kernel stack, and
/// a program in a loop calling `clone` would otherwise take the machine's
/// memory a stack at a time. Sixty-four is more than any of the programs run
/// here use and small enough that the refusal arrives while the machine is
/// still usable.
const MAX_THREADS: usize = 64;

/// Threads created and futex waits served, for the monitor.
static CREATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many threads each translated process has, so the cap can be enforced.
static COUNTS: IrqSpinLock<BTreeMap<u64, usize>> = IrqSpinLock::new(BTreeMap::new());

/// What a thread was asked to clear and wake when it exits.
///
/// `CLONE_CHILD_CLEARTID` is how a C library's `pthread_join` works: the kernel
/// writes zero to a word in the exiting thread's memory and wakes anything
/// waiting on it as a futex. Without it a program that joins a thread waits for
/// ever, which is not a slower join -- it is a program that stops.
static CLEARED: IrqSpinLock<BTreeMap<u64, u64>> = IrqSpinLock::new(BTreeMap::new());

/// Everything waiting on a futex, keyed by process and address.
///
/// One queue per address rather than one for the machine: waking a mutex would
/// otherwise wake every thread blocked on every lock in every program, which is
/// correct and is also the reason the thundering herd has a name.
static FUTEXES: IrqSpinLock<BTreeMap<(u64, u64), Arc<WaitQueue>>> =
    IrqSpinLock::new(BTreeMap::new());

/// Threads created, futex waits, and futex wakes.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        CREATED.load(Ordering::Relaxed),
        WAITS.load(Ordering::Relaxed),
        WAKES.load(Ordering::Relaxed),
    )
}

/// Forget what a process held. Called when it ends.
pub fn forget(process: u64) {
    COUNTS.lock().remove(&process);
    FUTEXES.lock().retain(|(owner, _), _| *owner != process);
}

/// `clone(flags, stack, parent_tid, child_tid, tls)`.
///
/// Note the argument order: x86-64's `clone` takes `tls` in `r8` and `child_tid`
/// in `r10`, which is *not* the order the manual page lists the C wrapper's
/// arguments in. Getting it wrong gives a thread whose thread pointer is an
/// address the library meant as somewhere to write an identifier, and what that
/// looks like is a fault on the first `errno`.
///
/// Returns the new thread's identifier to the caller and zero to the new
/// thread, which is how each of them knows which it is.
pub fn clone(
    flags: u64,
    stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
    frame: &crate::arch::syscall::Frame,
) -> u64 {
    use flag as f;

    // Without `CLONE_VM` this is `fork`, and the note at the top of the file
    // says why that is a refusal and not a thread.
    if flags & f::VM == 0 {
        crate::kprintln!(
            "[linux] clone without CLONE_VM is fork, which is not translated; answering ENOSYS"
        );
        return error::ENOSYS;
    }
    // A thread of the same group, sharing descriptors. A `clone` with `VM` but
    // not `THREAD` is a process sharing an address space -- something that
    // exists on Linux and that nothing this layer runs asks for. Refused rather
    // than treated as a thread, because the two differ in what `exit_group`
    // does to them.
    if flags & f::THREAD == 0 {
        crate::kprintln!("[linux] clone with CLONE_VM but not CLONE_THREAD is not translated");
        return error::ENOSYS;
    }
    // Linux requires these of a `CLONE_THREAD` and refuses without them, so a
    // request missing one is not a narrower thread -- it is a request no Linux
    // would have honoured either. Answering `EINVAL` is what Linux answers, and
    // is the difference between a program finding out and a program getting
    // something it did not ask for: a thread that did not share descriptors
    // would be a thread whose `printf` went nowhere.
    if flags & f::SIGHAND == 0 || flags & f::FILES == 0 {
        crate::kprintln!(
            "[linux] clone with CLONE_THREAD needs CLONE_SIGHAND and CLONE_FILES; answering EINVAL"
        );
        return error::EINVAL;
    }
    // A stack is required. Linux would take zero to mean "the same stack as the
    // parent", which for a thread means two threads writing one stack.
    if stack == 0 || stack >= nexus_abi::layout::USER_SPACE_END {
        return error::EINVAL;
    }

    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    {
        let mut counts = COUNTS.lock();
        // One, not zero: the thread making this call is the process's first and
        // was never counted here.
        let count = counts.entry(process.id.0).or_insert(1);
        if *count >= MAX_THREADS {
            crate::kprintln!(
                "[linux] {} asked for more than {MAX_THREADS} threads; refusing",
                process.name.as_str()
            );
            return (-11i64) as u64; // EAGAIN, which is what Linux answers
        }
        *count += 1;
    }

    // The child's registers: its parent's, with the two differences that make
    // it a child. See `crate::user::resume`.
    let mut child = *frame;
    child.rax = 0;
    child.rsp = stack;

    let thread_pointer = if flags & f::SETTLS != 0 {
        if tls >= nexus_abi::layout::USER_SPACE_END {
            return error::EFAULT;
        }
        Some(tls)
    } else {
        None
    };

    let started = crate::sched::spawn_user_thread(Arc::clone(&process), child, thread_pointer);
    let Ok(id) = started else {
        COUNTS
            .lock()
            .entry(process.id.0)
            .and_modify(|count| *count = count.saturating_sub(1));
        return error::ENOMEM;
    };

    // Where the library is to find the new thread's identifier. Both of these
    // are writes into the *shared* address space, so they can be done from here
    // whichever thread they are for.
    if flags & f::PARENT_SETTID != 0 && !write_word(parent_tid, id.0) {
        crate::kprintln!("[linux] clone could not write the child identifier for the parent");
    }
    if flags & f::CHILD_SETTID != 0 && !write_word(child_tid, id.0) {
        crate::kprintln!("[linux] clone could not write the child identifier for the child");
    }
    // And what to clear when it ends, which is how `pthread_join` finds out.
    if flags & f::CHILD_CLEARTID != 0 && child_tid != 0 {
        CLEARED.lock().insert(id.0, child_tid);
    }

    CREATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    id.0
}

/// A thread is ending: clear what it was asked to clear, and wake the joiner.
///
/// Called from the exit path rather than from a thread's own code, because a
/// thread that has called `exit` does not run again and this has to happen
/// after it cannot touch the word itself.
pub fn ending(thread: u64) {
    let Some(address) = CLEARED.lock().remove(&thread) else {
        return;
    };
    if !write_word(address, 0) {
        return;
    }
    // And the wake, which is the half that makes `pthread_join` return. The
    // word going to zero is what the joiner compares against; the wake is what
    // stops it sleeping through the change.
    let Some(process) = crate::sched::current_process() else {
        return;
    };
    wake(process.id.0, address, u64::MAX);
}

/// `futex(address, operation, expected, timeout, ...)`.
///
/// Two operations, which between them are every lock a C library has: wait
/// until the word changes, and wake whoever is waiting.
pub fn futex(address: u64, operation: u64, expected: u64, timeout: u64) -> u64 {
    // The bits that say "private" and "against the wall clock" change nothing
    // here and are cleared before the operation is read: a translation that did
    // not clear them would answer `ENOSYS` to every lock a real program takes,
    // because every one of them is private.
    let bare = operation & !(op::PRIVATE | op::CLOCK_REALTIME);

    // Shared futexes are refused rather than treated as private. See the note
    // at the top of the file: a lock that looks taken to one process and free
    // to another is worse than one that fails.
    if operation & op::PRIVATE == 0 && matches!(bare, op::WAIT | op::WAIT_BITSET) {
        crate::kprintln!("[linux] a shared futex is not translated; answering EOPNOTSUPP");
        return EOPNOTSUPP;
    }

    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    if !address.is_multiple_of(4) || address >= nexus_abi::layout::USER_SPACE_END {
        return error::EINVAL;
    }

    match bare {
        op::WAIT | op::WAIT_BITSET => wait(process.id.0, address, expected, timeout),
        op::WAKE | op::WAKE_BITSET => wake(process.id.0, address, expected),
        _ => {
            crate::kprintln!("[linux] futex operation {bare} is not translated");
            error::ENOSYS
        }
    }
}

/// The queue for one futex, made if it is not there.
fn queue_for(process: u64, address: u64) -> Arc<WaitQueue> {
    Arc::clone(
        FUTEXES
            .lock()
            .entry((process, address))
            .or_insert_with(|| Arc::new(WaitQueue::new())),
    )
}

/// `FUTEX_WAIT`: sleep unless the word has already changed.
///
/// The check and the decision to sleep are not two steps a wake can land
/// between. [`WaitQueue`] is built for exactly this: a generation is read
/// *before* the word is compared, and the sleep is conditional on the
/// generation not having moved -- so a wake that arrives after the comparison
/// and before the sleep is one this thread sees rather than one it sleeps
/// through. Getting this wrong does not produce a slow program; it produces a
/// thread asleep for ever holding a lock.
fn wait(process: u64, address: u64, expected: u64, timeout: u64) -> u64 {
    let queue = queue_for(process, address);
    WAITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // A timeout is a `struct timespec` in the caller's memory, or null for "no
    // timeout". Read once, before anything blocks.
    let deadline = if timeout == 0 {
        None
    } else {
        let Some(seconds) = read_word(timeout) else {
            return error::EFAULT;
        };
        let Some(nanoseconds) = read_word(timeout + 8) else {
            return error::EFAULT;
        };
        // In ticks, because that is what a wait queue's deadline is measured
        // in. A machine whose timer has not started yet has no rate, and a
        // deadline computed from a rate of zero is one that has already passed
        // -- so such a wait is untimed rather than instantaneous.
        let rate = crate::arch::time::frequency_hz();
        if rate == 0 {
            None
        } else {
            let milliseconds = seconds
                .saturating_mul(1000)
                .saturating_add(nanoseconds / 1_000_000);
            Some(
                crate::arch::time::ticks().saturating_add(milliseconds.saturating_mul(rate) / 1000),
            )
        }
    };

    loop {
        let seen = queue.generation();

        // The comparison, against the caller's memory. A word that is not what
        // the caller expected means somebody has already changed it, and going
        // to sleep would be sleeping through a change that has happened.
        let Some(current) = read_u32(address) else {
            return error::EFAULT;
        };
        if u64::from(current) != expected & 0xFFFF_FFFF {
            return EAGAIN;
        }

        // A thread told to stop must not stay blocked on something that may
        // never arrive -- the same rule every blocking call in this kernel
        // follows.
        if crate::sched::cancelled() {
            return (-4i64) as u64; // EINTR
        }
        if let Some(deadline) = deadline {
            if crate::arch::time::ticks() >= deadline {
                return ETIMEDOUT;
            }
            // The clock is a second waker, so a wait with a deadline ends when
            // the deadline passes and not only when somebody wakes it -- which
            // is the one thing a timed wait exists not to do.
            queue.wait_if_unchanged_until(seen, deadline);
        } else {
            queue.wait_if_unchanged(seen);
        }

        // Round again: a wake is permission to look, not a promise that the
        // word changed. Several threads waiting on one lock are all woken and
        // all but one find it taken again.
        let Some(current) = read_u32(address) else {
            return error::EFAULT;
        };
        if u64::from(current) != expected & 0xFFFF_FFFF {
            return 0;
        }
    }
}

/// `FUTEX_WAKE`: wake up to `count` of them.
///
/// The count is not honoured exactly: this wakes everything on the queue and
/// returns how many there were. Waking one when one was asked for needs a queue
/// that can hand out a single waiter, and [`WaitQueue`] wakes all of them --
/// which is correct, because every waiter re-checks the word and goes back to
/// sleep if it is still taken, and is more expensive than it needs to be. That
/// is a performance difference and not a correctness one, and it is written
/// down rather than left to be discovered under contention.
fn wake(process: u64, address: u64, count: u64) -> u64 {
    let queue = {
        let futexes = FUTEXES.lock();
        futexes.get(&(process, address)).map(Arc::clone)
    };
    let Some(queue) = queue else {
        // Nothing has ever waited here, which is the common case for an
        // uncontended lock: the library's fast path did not call this at all,
        // and the one time it did there was nobody to wake.
        return 0;
    };
    let woken = queue.wake_all();
    WAKES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    (woken as u64).min(count)
}

/// Read a thirty-two bit word out of the caller's memory.
fn read_u32(address: u64) -> Option<u32> {
    let (pointer, _) = crate::arch::syscall::user_range(address, 4, 4)?;
    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps. An unmapped address faults, which is the
    // caller's own page fault and not a kernel bug.
    Some(unsafe { core::ptr::read_volatile(pointer as *const u32) })
}

/// And a sixty-four bit one.
fn read_word(address: u64) -> Option<u64> {
    let (pointer, _) = crate::arch::syscall::user_range(address, 8, 8)?;
    // SAFETY: as above.
    Some(unsafe { core::ptr::read_unaligned(pointer as *const u64) })
}

/// Write a thirty-two bit word into it, which is what a thread identifier is.
fn write_word(address: u64, value: u64) -> bool {
    let Some((pointer, _)) = crate::arch::syscall::user_range(address, 4, 4) else {
        return false;
    };
    // SAFETY: as above. A thread identifier is an `int` in the ABI, so four
    // bytes and not eight -- writing eight would step on whatever the library
    // put next to it.
    unsafe { core::ptr::write_volatile(pointer as *mut u32, value as u32) };
    true
}

/// `set_robust_list(head, length)`.
///
/// Recorded and otherwise ignored, which is honest about what it is for. The
/// robust list is how Linux unlocks a mutex whose owner died holding it: the
/// kernel walks the list at thread exit and marks each lock. Nothing here walks
/// it, so a program whose thread dies holding a lock leaves it held.
///
/// Accepted rather than refused because every glibc thread calls it at startup
/// and refusing would stop threads that are not going to die holding anything.
/// The list is kept so that the honest thing can be done with it later, and so
/// that a caller reading it back gets what it set.
pub fn set_robust_list(head: u64, length: u64) -> u64 {
    /// What glibc's `struct robust_list_head` is, and the only length Linux
    /// accepts. A different one is a different structure and is refused.
    const HEAD_SIZE: u64 = 24;
    if length != HEAD_SIZE {
        return error::EINVAL;
    }
    ROBUST_LISTS
        .lock()
        .insert(crate::arch::percpu::current_thread(), head);
    0
}

/// `get_robust_list`, so that what was set can be read back.
pub fn get_robust_list(out_head: u64, out_length: u64) -> u64 {
    let head = ROBUST_LISTS
        .lock()
        .get(&crate::arch::percpu::current_thread())
        .copied()
        .unwrap_or(0);
    let Some((pointer, _)) = crate::arch::syscall::user_range(out_head, 8, 8) else {
        return error::EFAULT;
    };
    let Some((length, _)) = crate::arch::syscall::user_range(out_length, 8, 8) else {
        return error::EFAULT;
    };
    // SAFETY: both ranges were checked to lie inside the user half.
    unsafe {
        core::ptr::write_unaligned(pointer as *mut u64, head);
        core::ptr::write_unaligned(length as *mut u64, 24);
    }
    0
}

/// What each thread said its robust list was. See [`set_robust_list`].
static ROBUST_LISTS: IrqSpinLock<BTreeMap<u64, u64>> = IrqSpinLock::new(BTreeMap::new());

/// Forget a thread's robust list, and anything it was to clear.
///
/// `process` is given rather than looked up, because this runs on the way out
/// and the count it decrements is the one [`clone`] checks against
/// [`MAX_THREADS`]. Without the decrement a program that made and joined
/// threads in a loop -- which is what a thread pool does -- would be refused
/// its sixty-fifth thread and never get another, having at no point had more
/// than two.
pub fn forget_thread(process: u64, thread: u64) {
    ROBUST_LISTS.lock().remove(&thread);
    CLEARED.lock().remove(&thread);
    COUNTS
        .lock()
        .entry(process)
        .and_modify(|count| *count = count.saturating_sub(1));
}

/// Every thread of a process other than the calling one.
///
/// What `exit_group` needs: it ends the *whole* program, and a program with a
/// thread still running is not ended. Collected as identifiers rather than held
/// as references, because stopping a thread takes the scheduler's lock and
/// holding a reference into its table across that is how a deadlock is written.
#[must_use]
pub fn siblings(process: u64) -> Vec<u64> {
    crate::sched::live_threads_of_process(process)
        .into_iter()
        .filter(|thread| *thread != crate::arch::percpu::current_thread())
        .collect()
}
