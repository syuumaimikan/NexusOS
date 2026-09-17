//! A program that signals itself and carries on.
//!
//! A signal is the one thing in the Unix interface that runs a program's code
//! at a moment the program did not choose. What makes this a test rather than a
//! demonstration is that the program has to still be *correct* afterwards: the
//! handler runs in the middle of the program's own work, on the program's own
//! stack, and when it returns everything has to be exactly as it was.
//!
//! So the program fills an array, raises a signal, and checks the array. A
//! kernel that entered the handler on the wrong stack, or restored the wrong
//! registers, or wrote its frame over the red zone, corrupts something here --
//! and a check of a local that was live across the signal is the only thing
//! that notices.
//!
//! | 140 | `rt_sigaction` |
//! | 141 | the handler did not run |
//! | 142 | it ran with the wrong signal number |
//! | 143 | a value that was live across the signal was changed |
//! | 144 | the call the signal interrupted returned the wrong thing |
//! | 145 | `rt_sigprocmask` |
//! | 146 | a blocked signal was delivered anyway |
//! | 147 | it was not delivered after being unblocked |
//! | 148 | an ignored signal ran a handler |
//! | 149 | the handler ran twice for one signal |

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nexus_guest::{call, exit_group, expect, say, syscall1, syscall2, syscall4};

nexus_guest::guest_main!(run);

/// How many times the handler has run, and what it was told.
static RAN: AtomicU32 = AtomicU32::new(0);
static SAW: AtomicU64 = AtomicU64::new(0);

/// `SIGUSR1` and `SIGUSR2`: the two the system reserves for a program to mean
/// whatever it likes by, which is exactly what a test needs.
const SIGUSR1: u64 = 10;
const SIGUSR2: u64 = 12;

/// `SA_RESTORER`, and where it points.
///
/// Every C library sets this and supplies its own four bytes of code for a
/// handler to return through. This program has no C library, so it does not set
/// it -- and the kernel has to supply one instead, which is the path this
/// exercises. A program built against glibc would take the other path.
const SA_RESTORER: u64 = 0x0400_0000;

/// `SIG_IGN`.
const SIG_IGN: u64 = 1;

/// `struct sigaction`, in the order the *kernel* takes it -- which is not the
/// order the manual page lists.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigAction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

/// The handler.
///
/// `extern "C"` because that is what a signal handler is: the kernel enters it
/// with the signal number in the first argument register, exactly as a call
/// would.
extern "C" fn handle(signal: i32) {
    RAN.fetch_add(1, Ordering::SeqCst);
    SAW.store(signal as u64, Ordering::SeqCst);
}

/// Install `action` for `signal`.
fn install(signal: u64, handler: u64, flags: u64) -> i64 {
    let action = SigAction {
        handler,
        flags,
        restorer: 0,
        mask: 0,
    };
    syscall4(
        call::RT_SIGACTION,
        signal,
        core::ptr::addr_of!(action) as u64,
        0,
        8,
    )
}

fn run() -> ! {
    // A handler for `SIGUSR1`, with no restorer of its own: the kernel has to
    // provide the few bytes a handler returns through.
    expect(install(SIGUSR1, handle as *const () as u64, 0) == 0, 140);

    // Something live across the signal. Filled before, checked after, and
    // deliberately on the stack rather than in a static: the stack is where a
    // signal frame written in the wrong place does its damage, and a static
    // would not notice.
    let mut live = [0u64; 32];
    let mut index = 0;
    while index < live.len() {
        live[index] = 0x5A5A_0000 + index as u64;
        index += 1;
    }

    // `kill` with this program's own identifier, which is what `raise` is.
    let me = syscall1(call::GETPID, 0);
    expect(me > 0, 140);
    let sent = syscall2(call::KILL, me as u64, SIGUSR1);

    // The signal is delivered on the way out of a system call, so by the time
    // `kill` has returned the handler has already run and returned. That is not
    // true on Linux for a signal sent to another process, and it is true for
    // one a program sends to itself -- which is the case this is.
    expect(RAN.load(Ordering::SeqCst) == 1, 141);
    expect(SAW.load(Ordering::SeqCst) == SIGUSR1, 142);

    // What `kill` itself returned. It has to be the answer to the call, not
    // whatever the handler left behind: a kernel that wrote the signal frame
    // before putting the call's result into it would give the program the
    // number of the call it made.
    expect(sent == 0, 144);

    // And the array. Every element, because a frame written over the red zone
    // damages a few words rather than all of them.
    let mut index = 0;
    while index < live.len() {
        expect(live[index] == 0x5A5A_0000 + index as u64, 143);
        index += 1;
    }

    // ---- blocking ---------------------------------------------------------
    //
    // A blocked signal stays pending. This is what every critical section in
    // every threaded program relies on: the handler does not run *here*, it
    // runs when the program says it may.
    RAN.store(0, Ordering::SeqCst);
    expect(install(SIGUSR2, handle as *const () as u64, 0) == 0, 140);

    let blocked: u64 = 1 << (SIGUSR2 - 1);
    let mut previous: u64 = 0;
    expect(
        syscall4(
            call::RT_SIGPROCMASK,
            0, // SIG_BLOCK
            core::ptr::addr_of!(blocked) as u64,
            core::ptr::addr_of_mut!(previous) as u64,
            8,
        ) == 0,
        145,
    );
    expect(syscall2(call::KILL, me as u64, SIGUSR2) == 0, 145);
    // A few calls, any of which would have delivered it had it not been blocked.
    let _ = syscall1(call::GETPID, 0);
    let _ = syscall1(call::GETPID, 0);
    expect(RAN.load(Ordering::SeqCst) == 0, 146);

    // Unblocked, and now it arrives.
    expect(
        syscall4(
            call::RT_SIGPROCMASK,
            1, // SIG_UNBLOCK
            core::ptr::addr_of!(blocked) as u64,
            0,
            8,
        ) == 0,
        145,
    );
    expect(RAN.load(Ordering::SeqCst) == 1, 147);
    expect(SAW.load(Ordering::SeqCst) == SIGUSR2, 147);
    // Exactly once: a pending signal is one signal, however many times it was
    // raised while blocked.
    let _ = syscall1(call::GETPID, 0);
    expect(RAN.load(Ordering::SeqCst) == 1, 149);

    // ---- ignoring ---------------------------------------------------------
    RAN.store(0, Ordering::SeqCst);
    expect(install(SIGUSR1, SIG_IGN, 0) == 0, 140);
    expect(syscall2(call::KILL, me as u64, SIGUSR1) == 0, 148);
    let _ = syscall1(call::GETPID, 0);
    expect(RAN.load(Ordering::SeqCst) == 0, 148);

    // `SA_RESTORER` is named so that this program says what it is *not* doing:
    // it never sets the flag, so the kernel supplies the return path. A program
    // built against a C library sets it and supplies its own.
    let _ = SA_RESTORER;

    say("guest: a signal was raised, handled, blocked, unblocked and ignored");
    exit_group(0)
}
