//! Signals, for a program built for Linux.
//!
//! A signal is the one thing in the Unix interface that runs a program's code
//! at a moment the program did not choose. Everything else here is a call a
//! program made and an answer it gets back; this arranges for a function to be
//! entered *between* two of the program's own instructions, on the program's
//! own stack, and for the program to carry on afterwards as though nothing had
//! happened.
//!
//! `rt_sigaction` used to be refused. That was the right answer while there was
//! no delivery: accepting it would have promised a handler that never runs, and
//! a program that installed one for a fault and then took the fault would sit
//! in a loop rather than dying with a message. It is not the right answer any
//! more, because the promise can now be kept.
//!
//! # How a handler is entered
//!
//! The same way Linux does it, because there is no other way that leaves the
//! program able to continue:
//!
//! 1. The whole register state is written onto the *user's* stack, below
//!    wherever it was, as a [`Frame`](crate::arch::syscall::Frame) inside a
//!    structure the program can also read as a `ucontext`.
//!    [`crate::arch::syscall::Frame`] exists because `clone` needed it; this is
//!    the second thing that could not be built without it.
//! 2. A return address is pushed that points at a few bytes of code the kernel
//!    put in the program's address space, which do nothing but call
//!    `rt_sigreturn`. A handler is an ordinary C function and ends with `ret`,
//!    so *something* has to be at the top of its stack.
//! 3. `rip` becomes the handler, `rdi` the signal number, and the program
//!    resumes.
//! 4. `rt_sigreturn` reads the frame back off the stack and returns to where
//!    the program was.
//!
//! The third step is where a signal differs from a call: `rsp` moves down by a
//! hundred and twenty-eight bytes first. That is the *red zone* — the space
//! below `rsp` that a leaf function is entitled to use without reserving it —
//! and a kernel that wrote a signal frame over it would corrupt the locals of
//! whatever was running.
//!
//! # When one is delivered
//!
//! On the way back to ring 3 from a system call. Not from an interrupt: a
//! thread interrupted in the middle of a `memcpy` has a register state that
//! is perfectly resumable, and delivering there is what makes a signal able to
//! interrupt a long computation rather than only a long wait. That is a real
//! difference from Linux and it means a program that spins without making a
//! system call cannot be signalled. It is written down rather than discovered.
//!
//! # What is not here
//!
//! Signals raised by the *kernel*: there is no `SIGSEGV` on a fault, no
//! `SIGPIPE` on a write to a pipe nobody reads, no `SIGCHLD`. A fault still
//! ends the process, which is what it did before. What works is a signal a
//! program sends to itself or to one of its own threads, which is what a C
//! library's `raise`, `abort` and `pthread_kill` are — and what every
//! cancellation mechanism is built on.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::arch::syscall::Frame;
use crate::sync::IrqSpinLock;

use super::linux::error;

/// How many signals there are. Linux's `_NSIG`.
pub const COUNT: usize = 64;

/// The ones with a meaning worth naming here.
pub mod number {
    pub const HUP: u64 = 1;
    pub const INT: u64 = 2;
    pub const QUIT: u64 = 3;
    pub const ILL: u64 = 4;
    pub const ABRT: u64 = 6;
    pub const FPE: u64 = 8;
    /// Cannot be caught, blocked or ignored. Not on Linux and not here.
    pub const KILL: u64 = 9;
    pub const USR1: u64 = 10;
    pub const SEGV: u64 = 11;
    pub const USR2: u64 = 12;
    pub const PIPE: u64 = 13;
    pub const ALRM: u64 = 14;
    pub const TERM: u64 = 15;
    pub const CHLD: u64 = 17;
    /// Cannot be caught either.
    pub const STOP: u64 = 19;
}

/// What a signal is called, for the log.
///
/// A number in a log line is a number somebody has to go and look up. The ones
/// without a name here are the real-time signals from thirty-two up, which have
/// no name to give: what they mean is whatever the program using them decided.
#[must_use]
pub fn name(signal: u64) -> &'static str {
    match signal {
        number::HUP => "SIGHUP",
        number::INT => "SIGINT",
        number::QUIT => "SIGQUIT",
        number::ILL => "SIGILL",
        number::ABRT => "SIGABRT",
        number::FPE => "SIGFPE",
        number::KILL => "SIGKILL",
        number::USR1 => "SIGUSR1",
        number::SEGV => "SIGSEGV",
        number::USR2 => "SIGUSR2",
        number::PIPE => "SIGPIPE",
        number::ALRM => "SIGALRM",
        number::TERM => "SIGTERM",
        number::CHLD => "SIGCHLD",
        number::STOP => "SIGSTOP",
        _ => "a signal",
    }
}

/// What a program can ask to happen instead of a handler.
mod disposition {
    /// Whatever the signal does by default.
    pub const DEFAULT: u64 = 0;
    /// Nothing at all.
    pub const IGNORE: u64 = 1;
}

/// Flags on a `sigaction` this layer reads.
mod flag {
    /// The handler takes three arguments rather than one.
    pub const SIGINFO: u64 = 0x0000_0004;
    /// The handler runs once and the disposition goes back to default.
    pub const RESETHAND: u64 = 0x8000_0000;
    /// There is a return trampoline in the structure. Every current libc sets
    /// this and supplies its own; see [`restorer_of`].
    pub const RESTORER: u64 = 0x0400_0000;
}

/// `struct sigaction` on x86-64: handler, flags, restorer, mask. Thirty-two
/// bytes, and the order is the kernel's rather than the one in the manual page.
const SIGACTION_BYTES: u64 = 32;

/// The red zone: what a leaf function may use below `rsp` without asking.
///
/// A hundred and twenty-eight bytes, and the number is the ABI's. A signal
/// frame written over it corrupts the locals of whatever was interrupted, which
/// is a failure with no pattern at all.
const RED_ZONE: u64 = 128;

/// What one process has arranged for each signal.
#[derive(Clone, Copy)]
struct Action {
    /// The handler, or [`disposition::DEFAULT`] or [`disposition::IGNORE`].
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

impl Action {
    const fn default_action() -> Self {
        Self {
            handler: disposition::DEFAULT,
            flags: 0,
            restorer: 0,
            mask: 0,
        }
    }
}

/// What each process has arranged, and what is waiting to be delivered.
struct Signals {
    actions: [Action; COUNT],
    /// Signals raised and not yet delivered, as a bitmask from one.
    pending: u64,
    /// Signals the program has asked not to receive yet.
    blocked: u64,
    /// Where the return trampoline was put in this process's address space.
    ///
    /// Zero until the first handler is entered: a program that never catches a
    /// signal does not need a page of its address space spent on one.
    trampoline: u64,
    /// The signals whose handlers are running, innermost last.
    ///
    /// A stack rather than a number, because a handler can be interrupted by a
    /// different signal and `rt_sigreturn` has to know which one it is
    /// returning from -- it is the only thing that can put the right bit back
    /// into the mask. Nothing in the frame says: the frame is the *program's*
    /// registers, from before the handler was entered.
    handling: Vec<u64>,
}

impl Signals {
    fn new() -> Self {
        Self {
            actions: [Action::default_action(); COUNT],
            pending: 0,
            blocked: 0,
            trampoline: 0,
            handling: Vec::new(),
        }
    }
}

/// Every translated process's signal state.
static STATE: IrqSpinLock<BTreeMap<u64, Signals>> = IrqSpinLock::new(BTreeMap::new());

/// Handlers installed, and signals delivered.
static INSTALLED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DELIVERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Handlers installed, and signals delivered to one.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        INSTALLED.load(Ordering::Relaxed),
        DELIVERED.load(Ordering::Relaxed),
    )
}

/// Forget a process's signal state. Called when it ends.
pub fn forget(process: u64) {
    STATE.lock().remove(&process);
}

/// The bit for a signal, or `None` if there is no such signal.
fn bit(signal: u64) -> Option<u64> {
    if signal == 0 || signal as usize > COUNT {
        None
    } else {
        Some(1u64 << (signal - 1))
    }
}

/// `rt_sigaction(signal, new, old, sigsetsize)`.
pub fn sigaction(signal: u64, new: u64, old: u64, set_size: u64) -> u64 {
    // The mask is sixty-four bits and a caller that thinks otherwise is a
    // caller compiled against a different kernel. Linux checks this too.
    if set_size != 8 {
        return error::EINVAL;
    }
    let Some(_) = bit(signal) else {
        return error::EINVAL;
    };
    // Neither of these can be caught, and a program that thinks it has caught
    // one would be wrong in a way that matters: `SIGKILL` is how a system stops
    // a program that will not stop itself.
    if signal == number::KILL || signal == number::STOP {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    let mut state = STATE.lock();
    let signals = state.entry(process.id.0).or_insert_with(Signals::new);
    let index = (signal - 1) as usize;

    if old != 0 {
        let was = signals.actions[index];
        let Some((at, _)) = crate::arch::syscall::user_range(old, SIGACTION_BYTES, SIGACTION_BYTES)
        else {
            return error::EFAULT;
        };
        // SAFETY: thirty-two bytes inside the user half, written as the four
        // words a `sigaction` is.
        unsafe {
            let words = at as *mut u64;
            words.write_unaligned(was.handler);
            words.add(1).write_unaligned(was.flags);
            words.add(2).write_unaligned(was.restorer);
            words.add(3).write_unaligned(was.mask);
        }
    }

    if new != 0 {
        let Some((at, _)) = crate::arch::syscall::user_range(new, SIGACTION_BYTES, SIGACTION_BYTES)
        else {
            return error::EFAULT;
        };
        // SAFETY: as above, read rather than written.
        let action = unsafe {
            let words = at as *const u64;
            Action {
                handler: words.read_unaligned(),
                flags: words.add(1).read_unaligned(),
                restorer: words.add(2).read_unaligned(),
                mask: words.add(3).read_unaligned(),
            }
        };
        if action.handler != disposition::DEFAULT
            && action.handler != disposition::IGNORE
            && action.handler >= nexus_abi::layout::USER_SPACE_END
        {
            // A handler outside the user half would be this program asking the
            // kernel to jump into the kernel on its behalf.
            return error::EFAULT;
        }
        signals.actions[index] = action;
        if action.handler > disposition::IGNORE {
            INSTALLED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
    0
}

/// `rt_sigprocmask(how, new, old, sigsetsize)`.
pub fn sigprocmask(how: u64, new: u64, old: u64, set_size: u64) -> u64 {
    /// Add to the mask, take away from it, or replace it.
    const BLOCK: u64 = 0;
    const UNBLOCK: u64 = 1;
    const SETMASK: u64 = 2;

    if set_size != 8 {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let mut state = STATE.lock();
    let signals = state.entry(process.id.0).or_insert_with(Signals::new);

    if old != 0 {
        let Some((at, _)) = crate::arch::syscall::user_range(old, 8, 8) else {
            return error::EFAULT;
        };
        // SAFETY: eight bytes inside the user half.
        unsafe { core::ptr::write_unaligned(at as *mut u64, signals.blocked) };
    }
    if new != 0 {
        let Some((at, _)) = crate::arch::syscall::user_range(new, 8, 8) else {
            return error::EFAULT;
        };
        // SAFETY: as above.
        let wanted = unsafe { core::ptr::read_unaligned(at as *const u64) };
        // `SIGKILL` and `SIGSTOP` cannot be blocked, which is the other half of
        // their not being catchable.
        let cannot = bit(number::KILL).unwrap_or(0) | bit(number::STOP).unwrap_or(0);
        signals.blocked = match how {
            BLOCK => signals.blocked | (wanted & !cannot),
            UNBLOCK => signals.blocked & !wanted,
            SETMASK => wanted & !cannot,
            _ => return error::EINVAL,
        };
    }
    0
}

/// `kill(pid, signal)` and `tgkill(tgid, tid, signal)`.
///
/// Only this process: there is no way here to name another one by its
/// identifier and be entitled to signal it, and a call that could would be
/// ambient authority of exactly the kind this system does not have. A program
/// signalling itself — which is what `raise`, `abort` and `pthread_kill` are —
/// works.
pub fn kill(target: u64, signal: u64) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    // Zero is "is this process there", which it is.
    if signal == 0 {
        return 0;
    }
    let Some(bit) = bit(signal) else {
        return error::EINVAL;
    };
    if target != process.id.0 && target != 0 {
        crate::kprintln!("[linux] a signal to another process is not translated; answering EPERM");
        return (-1i64) as u64; // EPERM
    }
    // `SIGKILL` on itself is the program asking to stop, and it is answered by
    // stopping rather than by a handler that cannot exist.
    if signal == number::KILL {
        process.completion.cancel();
        crate::sched::wake_process_threads(process.id);
        return 0;
    }
    let mut state = STATE.lock();
    let signals = state.entry(process.id.0).or_insert_with(Signals::new);
    signals.pending |= bit;
    0
}

/// Whether anything is waiting to be delivered to the calling process.
///
/// Cheap, because it is asked on the way out of every system call a translated
/// program makes. One map lookup, and nothing at all for a process that has
/// never touched a signal.
#[must_use]
pub fn pending(process: u64) -> bool {
    STATE
        .lock()
        .get(&process)
        .is_some_and(|signals| signals.pending & !signals.blocked != 0)
}

/// Deliver whatever is waiting, if the frame can be moved to a handler.
///
/// Called on the way back to ring 3 from a system call, with the frame the
/// caller is about to be restored from. Changing it here is what makes the
/// program resume in the handler instead of after its call.
///
/// Returns true if the frame was changed.
pub fn deliver(frame: &mut Frame) -> bool {
    let Some(process) = crate::sched::current_process() else {
        return false;
    };

    // Which signal, and what to do about it. Decided under the lock and acted
    // on outside it: writing the frame touches the user's stack, which can
    // fault, and faulting with this lock held would stop every other process's
    // signals too.
    let (signal, action, trampoline) = {
        let mut state = STATE.lock();
        let Some(signals) = state.get_mut(&process.id.0) else {
            return false;
        };
        let ready = signals.pending & !signals.blocked;
        if ready == 0 {
            return false;
        }
        // The lowest-numbered one first, which is what Linux does and is what a
        // program expecting `SIGINT` before `SIGTERM` relies on.
        let signal = ready.trailing_zeros() as u64 + 1;
        let Some(bit) = bit(signal) else {
            return false;
        };
        signals.pending &= !bit;
        let action = signals.actions[(signal - 1) as usize];

        match action.handler {
            disposition::IGNORE => return false,
            disposition::DEFAULT => {
                // No handler. What a signal does by default is end the program,
                // for every one this layer can raise -- and doing that here,
                // rather than silently dropping it, is the difference between
                // `abort` stopping a program and `abort` doing nothing.
                drop(state);
                crate::kprintln!(
                    "[linux] process {} \"{}\" was ended by {} ({signal})",
                    process.id,
                    process.name.as_str(),
                    name(signal)
                );
                process.completion.finish(128 + signal);
                crate::arch::interrupts::disable();
                crate::sched::exit();
            }
            _ => {}
        }
        // The mask the handler runs under: what was blocked, plus what the
        // action asked for, plus the signal itself -- so a handler cannot be
        // re-entered by the same signal while it runs.
        signals.blocked |= action.mask | bit;
        if action.flags & flag::RESETHAND != 0 {
            signals.actions[(signal - 1) as usize] = Action::default_action();
        }
        let trampoline = signals.trampoline;
        (signal, action, trampoline)
    };

    // Where the handler returns to. The program's own, if its C library gave
    // one -- every current libc does, and it is the `SA_RESTORER` flag that
    // says so. Otherwise one this layer puts in the program's address space.
    let restorer = if action.flags & flag::RESTORER != 0 && action.restorer != 0 {
        action.restorer
    } else {
        match ensure_trampoline(&process, trampoline) {
            Some(at) => at,
            None => {
                crate::kprintln!("[linux] no return path for a signal handler; ending the process");
                process.completion.finish(128 + signal);
                crate::arch::interrupts::disable();
                crate::sched::exit();
            }
        }
    };

    // The frame the handler will return through, written onto the user's own
    // stack below the red zone.
    let saved = *frame;
    let at = (frame
        .rsp
        .saturating_sub(RED_ZONE + core::mem::size_of::<Frame>() as u64))
        & !15;
    // Room for the return address, which has to be immediately below the frame
    // so that a `ret` at the end of the handler finds it.
    let stack = at.saturating_sub(8);
    if stack < 0x1000 || at >= nexus_abi::layout::USER_SPACE_END {
        crate::kprintln!("[linux] no room on the stack for a signal frame; ending the process");
        process.completion.finish(128 + signal);
        crate::arch::interrupts::disable();
        crate::sched::exit();
    }

    let Some((frame_at, _)) = crate::arch::syscall::user_range(
        at,
        core::mem::size_of::<Frame>() as u64,
        core::mem::size_of::<Frame>() as u64,
    ) else {
        crate::kprintln!("[linux] a signal frame would not fit in the program's stack");
        process.completion.finish(128 + signal);
        crate::arch::interrupts::disable();
        crate::sched::exit();
    };
    let Some((return_at, _)) = crate::arch::syscall::user_range(stack, 8, 8) else {
        process.completion.finish(128 + signal);
        crate::arch::interrupts::disable();
        crate::sched::exit();
    };

    // SAFETY: both ranges were checked to lie inside the user half, which this
    // thread's address space maps. An unmapped stack faults, which is the
    // program's own fault and is what a stack overflow looks like anyway.
    unsafe {
        core::ptr::write_unaligned(frame_at as *mut Frame, saved);
        core::ptr::write_unaligned(return_at as *mut u64, restorer);
    }

    // And the state the handler is entered in. `rdi` is the signal number,
    // because a handler is `void handler(int)`; the other two arguments a
    // `SA_SIGINFO` handler takes are null, which is a thing Linux itself does
    // when it has nothing to put there -- but a handler that *reads* them would
    // dereference null, so it is said in the log rather than left to be found.
    if action.flags & flag::SIGINFO != 0 {
        crate::kprintln!(
            "[linux] {} is entered with no siginfo: this layer has none to give",
            name(signal)
        );
    }
    frame.rip = action.handler;
    frame.rsp = stack;
    frame.rdi = signal;
    frame.rsi = 0;
    frame.rdx = 0;
    // `rax` is scratch at the entry to any function, so nothing is lost by the
    // system-call path writing its own answer over it on the way out -- and the
    // answer the *program* is owed is in the frame that was just saved, which
    // is where `rt_sigreturn` will find it.
    frame.rax = 0;

    STATE
        .lock()
        .entry(process.id.0)
        .or_insert_with(Signals::new)
        .handling
        .push(signal);
    DELIVERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    true
}

/// `rt_sigreturn()`: put the frame back and carry on.
///
/// Takes nothing and returns nothing, because everything it needs is on the
/// stack: the handler's `ret` popped the return address, so `rsp` is pointing
/// exactly at the frame this layer wrote.
pub fn sigreturn(frame: &mut Frame) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let at = frame.rsp;
    let Some((from, _)) = crate::arch::syscall::user_range(
        at,
        core::mem::size_of::<Frame>() as u64,
        core::mem::size_of::<Frame>() as u64,
    ) else {
        crate::kprintln!("[linux] rt_sigreturn with no frame under the stack pointer");
        process.completion.finish(139);
        crate::arch::interrupts::disable();
        crate::sched::exit();
    };
    // SAFETY: the range was checked to lie inside the user half. What is read
    // is whatever the program has at that address -- which is this layer's own
    // frame unless the program moved its stack pointer, and a program that did
    // that is one resuming at an address of its own choosing, which it could
    // have reached by jumping there.
    let saved = unsafe { core::ptr::read_unaligned(from as *const Frame) };

    // Every register but the segment state, which is not the program's to set.
    // `rflags` is taken from the frame with the bits a program may not change
    // forced back: a handler that returned with `IF` clear would be a program
    // that had turned interrupts off.
    *frame = saved;
    frame.rflags = (saved.rflags & USER_FLAGS_MASK) | ALWAYS_SET;

    // And the mask the handler was entered under goes back: the signal's own
    // bit is cleared, and anything the program blocked *inside* the handler
    // stays blocked, because a program that did that meant it.
    let mut state = STATE.lock();
    if let Some(signals) = state.get_mut(&process.id.0) {
        if let Some(signal) = signals.handling.pop() {
            if let Some(bit) = bit(signal) {
                signals.blocked &= !bit;
            }
        }
    }
    // What the program's interrupted system call was going to return. It was
    // written into the frame before the handler was entered, for exactly this.
    frame.rax
}

/// Which `RFLAGS` bits a program may set for itself.
///
/// Carry, parity, adjust, zero, sign, direction and overflow: the arithmetic
/// results and the string direction. Not the interrupt flag, not the I/O
/// privilege level, not the trap flag.
const USER_FLAGS_MASK: u64 = 0x0000_08D5;
/// And the ones the processor requires: bit one is reserved and always set,
/// and `IF` stays on because ring 3 must remain interruptible.
const ALWAYS_SET: u64 = 0x0000_0202;

/// Put a return trampoline in the program's address space, if it has none.
///
/// Four bytes of code: `mov eax, 15; syscall`. A signal handler is an ordinary
/// function and ends with `ret`, so something has to be at the top of its
/// stack, and on Linux that something is supplied by the C library. A program
/// with no C library has none, and this is what it gets instead.
///
/// The page is mapped read-only and executable, which is the one place this
/// layer maps an executable page it wrote itself — and it writes four bytes
/// that make one system call.
fn ensure_trampoline(process: &crate::process::Process, existing: u64) -> Option<u64> {
    if existing != 0 {
        return Some(existing);
    }
    /// `mov eax, 15` -- `rt_sigreturn` -- and `syscall`.
    const CODE: [u8; 7] = [0xB8, 0x0F, 0x00, 0x00, 0x00, 0x0F, 0x05];
    /// Where it goes: above everything `mmap` hands out and below the stack,
    /// at an address nothing else in this layer uses.
    const AT: u64 = 0x0000_1000_0000_0000;

    let frame = crate::memory::allocate_frame()?;
    let kernel = nexus_abi::layout::phys_to_virt(frame) as *mut u8;
    // SAFETY: the frame came from the allocator and nothing else holds it.
    unsafe {
        core::ptr::write_bytes(kernel, 0, 4096);
        core::ptr::copy_nonoverlapping(CODE.as_ptr(), kernel, CODE.len());
    }
    // Readable and executable, and *not* writable: the program cannot change
    // what it returns through.
    let flags = crate::memory::paging::PRESENT | crate::memory::paging::USER;
    // SAFETY: the frame is this mapping's and `AT` is inside the user half.
    if unsafe { process.address_space.map(AT, frame, flags) }.is_err() {
        // SAFETY: never mapped, so nothing refers to it.
        unsafe { crate::memory::free_frame(frame) };
        return None;
    }
    STATE
        .lock()
        .entry(process.id.0)
        .or_insert_with(Signals::new)
        .trampoline = AT;
    Some(AT)
}

/// `sigaltstack`, `rt_sigsuspend` and the rest of the family.
///
/// Refused by name rather than by falling through to the catch-all, so the log
/// says which one a program wanted. Each is real work and none of them is
/// hidden behind a stub that returns success.
pub fn not_translated(what: &str) -> u64 {
    crate::kprintln!("[linux] {what} is not translated; answering ENOSYS");
    error::ENOSYS
}
