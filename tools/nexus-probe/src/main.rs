//! A Linux program that asks the machine which system calls it does not have.
//!
//! # Why this exists
//!
//! The plan is a custom kernel with a Linux userspace ABI, and software built
//! elsewhere -- Mesa, and a C library under it -- running on top. That turns
//! "implement Linux" into a finite list, but only if somebody writes the list
//! down, and a list written from memory is a list of what the author happens to
//! remember.
//!
//! So this asks the machine. Each call below is made with arguments chosen to
//! be harmless, and the only thing looked at is whether the answer is
//! `-ENOSYS`. Linux returns that for a call a kernel does not implement and
//! nothing else does, so it separates "missing" from "present and refused the
//! arguments" exactly.
//!
//! # Why the arguments are what they are
//!
//! **Nothing here is allowed to change anything.** A probe that wrote a file to
//! find out whether `write` existed would be a probe you could only run once.
//!
//! The trick is a file descriptor of `-1`. Every call that takes one answers
//! `EBADF` when it is implemented, whatever else it would have done, so the
//! call is made and nothing is touched. Where there is no descriptor, a null
//! pointer earns `EFAULT` or `EINVAL` for the same reason.
//!
//! # The control
//!
//! `getpid` is on the list and is certainly implemented. If it ever comes back
//! missing, the probe is broken rather than the kernel -- which is the failure
//! this kind of program is most likely to have and least likely to notice.

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

mod say;

/// What Linux answers for a call it does not have.
const ENOSYS: i64 = -38;

/// Make a system call.
///
/// # Safety
///
/// The caller is promising that this number and these arguments are safe to
/// make. Every call in this file is chosen to fail harmlessly; a caller passing
/// something else is on its own.
unsafe fn call(number: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> i64 {
    let out: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => out,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            in("r8") e,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    out
}

fn write(text: &[u8]) {
    // SAFETY: standard output, a real pointer and its real length.
    unsafe {
        call(1, 1, text.as_ptr() as u64, text.len() as u64, 0, 0);
    }
}

fn exit(code: i32) -> ! {
    // SAFETY: exit_group takes a status and does not return.
    unsafe {
        call(231, code as u64, 0, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    write(b"probe: PANIC\n");
    exit(101)
}

/// A number, written out, because there is no formatter here.
fn number(mut value: u64, into: &mut [u8; 20]) -> &str {
    if value == 0 {
        into[0] = b'0';
        // SAFETY: one ASCII digit is valid UTF-8.
        return unsafe { core::str::from_utf8_unchecked(&into[..1]) };
    }
    let mut digits = [0u8; 20];
    let mut count = 0;
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    for index in 0..count {
        into[index] = digits[count - 1 - index];
    }
    // SAFETY: ASCII digits are valid UTF-8.
    unsafe { core::str::from_utf8_unchecked(&into[..count]) }
}

fn say(parts: &[&str]) {
    say::line(parts, write);
}

/// What to ask about, and how to ask harmlessly.
///
/// `(number, name, a, b, c, d, e)`. The arguments are the harmless ones
/// described at the top: `-1` where a descriptor goes, zero everywhere else.
const BAD_FD: u64 = -1i64 as u64;

struct Ask {
    number: u64,
    name: &'static str,
    args: [u64; 5],
    /// Why somebody building a C library or Mesa on this machine would care.
    wanted_for: &'static str,
}

/// The list.
///
/// Chosen from what a C library needs to start and what Mesa's software
/// rasteriser needs to run -- not from the whole of Linux, which would be a
/// list nobody could act on. `docs/linux-abi-graphics.md` says where each one
/// comes from.
const ASKS: &[Ask] = &[
    // The control. Certainly implemented; a "missing" here means this program
    // is wrong, not the kernel.
    Ask { number: 39, name: "getpid", args: [0, 0, 0, 0, 0], wanted_for: "the control" },

    // A C library's start-up and its allocator.
    Ask { number: 25, name: "mremap", args: [0, 0, 0, 0, 0], wanted_for: "realloc of a mapping" },
    Ask { number: 28, name: "madvise", args: [0, 0, 0, 0, 0], wanted_for: "malloc returning memory" },
    Ask { number: 97, name: "getrlimit", args: [7, 0, 0, 0, 0], wanted_for: "how many files may be open" },
    Ask { number: 302, name: "prlimit64", args: [0, 7, 0, 0, 0], wanted_for: "the same, as musl asks it" },
    Ask { number: 157, name: "prctl", args: [0, 0, 0, 0, 0], wanted_for: "thread names, no-new-privs" },
    Ask { number: 99, name: "sysinfo", args: [0, 0, 0, 0, 0], wanted_for: "how much memory there is" },
    Ask { number: 324, name: "membarrier", args: [0, 0, 0, 0, 0], wanted_for: "a lock-free fast path" },

    // Files, which Mesa uses for its shader cache and for /proc and /sys.
    Ask { number: 72, name: "fcntl", args: [BAD_FD, 3, 0, 0, 0], wanted_for: "close-on-exec, non-blocking" },
    Ask { number: 74, name: "fsync", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "a cache that survives" },
    Ask { number: 19, name: "readv", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "scattered reads" },
    Ask { number: 18, name: "pwrite64", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "writing at an offset" },
    Ask { number: 89, name: "readlink", args: [0, 0, 0, 0, 0], wanted_for: "/proc/self/exe" },
    Ask { number: 267, name: "readlinkat", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "the same, as a C library asks it" },
    Ask { number: 80, name: "chdir", args: [0, 0, 0, 0, 0], wanted_for: "a working directory" },
    Ask { number: 137, name: "statfs", args: [0, 0, 0, 0, 0], wanted_for: "how much room is left" },
    Ask { number: 332, name: "statx", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "what a modern C library stats with" },
    Ask { number: 6, name: "lstat", args: [0, 0, 0, 0, 0], wanted_for: "telling a link from a file" },

    // Time and waiting.
    Ask { number: 35, name: "nanosleep", args: [0, 0, 0, 0, 0], wanted_for: "sleeping at all" },
    Ask { number: 230, name: "clock_nanosleep", args: [0, 0, 0, 0, 0], wanted_for: "sleeping until a time" },
    Ask { number: 96, name: "gettimeofday", args: [0, 0, 0, 0, 0], wanted_for: "the time, the old way" },
    Ask { number: 201, name: "time", args: [0, 0, 0, 0, 0], wanted_for: "the time, the oldest way" },

    // Processes.
    Ask { number: 61, name: "wait4", args: [BAD_FD, 0, 1, 0, 0], wanted_for: "collecting a child" },

    // Waiting on several things, which a windowing client does constantly.
    Ask { number: 23, name: "select", args: [BAD_FD, 0, 0, 0, 0], wanted_for: "the oldest way to wait on many" },
];

/// The entry point, which cannot be an ordinary function.
///
/// At process entry the stack pointer is sixteen-byte aligned and there is no
/// return address on it. A compiler given `extern "C" fn _start` assumes the
/// opposite -- that it was reached by a `call`, so `rsp % 16 == 8` -- and lays
/// out its stack accordingly. The first sixteen-byte SSE store into that frame
/// is then misaligned and the processor raises a general protection fault.
///
/// Which is exactly what happened: the probe printed its first line and died at
/// `0x4024a0` with `EXCEPTION 13`. Aligning the stack and entering through a
/// `call` gives the compiler the shape it already believed it had.
#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "and rsp, -16",
        "call {main}",
        "ud2",
        main = sym probe,
    )
}

extern "C" fn probe() -> ! {
    say(&["probe: asking this machine which calls it does not have"]);

    let mut missing = 0u64;
    let mut present = 0u64;
    let mut control_wrong = false;

    for ask in ASKS {
        // SAFETY: every argument set above is chosen so that an implemented
        // call refuses it -- a descriptor of -1, or a null pointer -- and
        // changes nothing. The only thing read from the answer is whether it
        // is ENOSYS.
        let answer = unsafe {
            call(
                ask.number,
                ask.args[0],
                ask.args[1],
                ask.args[2],
                ask.args[3],
                ask.args[4],
            )
        };
        let mut digits = [0u8; 20];
        let shown = number(ask.number, &mut digits);
        if answer == ENOSYS {
            missing += 1;
            if ask.number == 39 {
                control_wrong = true;
            }
            say(&["probe: MISSING ", ask.name, " (", shown, ") -- ", ask.wanted_for]);
        } else {
            present += 1;
            say(&["probe: have ", ask.name, " (", shown, ")"]);
        }
    }

    let mut a = [0u8; 20];
    let mut b = [0u8; 20];
    let mut c = [0u8; 20];
    say(&[
        "probe: ",
        number(present, &mut a),
        " of ",
        number(ASKS.len() as u64, &mut b),
        " present, ",
        number(missing, &mut c),
        " missing",
    ]);

    if control_wrong {
        // getpid came back missing, which cannot be true: the machine started
        // this program. Something about the way this asks is wrong, and every
        // other answer in the list is worth nothing.
        say(&["probe: FAILED: the control call came back missing, so this probe is wrong"]);
        exit(2);
    }
    exit(0)
}
