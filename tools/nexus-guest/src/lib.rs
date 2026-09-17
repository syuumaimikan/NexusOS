//! Linux, from the other side.
//!
//! Everything in this crate is a program built for Linux. Not for NexusOS: the
//! call numbers here are Linux's, the register convention is Linux's, and the
//! only thing these programs know about the machine they end up on is that it
//! has a `syscall` instruction. They are compiled for
//! `x86_64-unknown-linux-gnu` and would run unchanged on a Linux kernel.
//!
//! # Why there is a compiler involved at all
//!
//! The fixtures in `nexus-linux-example` are hand-assembled, byte by byte, with
//! the field each byte belongs to named beside it. That was the only way to
//! have any Linux binary at all: building one needs a Linux toolchain, and the
//! machine this repository is developed on does not have one.
//!
//! It turns out it does not need one. `rustc` can target
//! `x86_64-unknown-linux-gnu` with `core` rebuilt from source, and `rust-lld`
//! can link a static `ET_EXEC` with no C runtime — which is a real Linux
//! executable produced without leaving the toolchain the rest of this
//! repository already uses. See `.cargo/config.toml` for the flags and why each
//! one is there.
//!
//! The hand-assembled fixtures stay. They are the ones that prove the *loader*
//! works on an image nobody could have shaped to suit, and one of them is a
//! dynamic linker, which is not a thing a compiler will emit for you. These are
//! for the programs that got too big to write by hand: a thread pool, a socket
//! client, a signal handler.
//!
//! # No standard library
//!
//! `#![no_std]`, and no libc. A program here makes its own system calls and
//! brings its own entry point, because linking against glibc would mean having
//! glibc — which is the thing this whole exercise does not have yet.
//!
//! That has one consequence worth naming: there is no `main`, no argument
//! parsing and no `errno`. A call returns what Linux put in `rax`, negative for
//! an error, and the programs here read it that way.

#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::arch::asm;

pub mod call;
pub mod fmt;
pub mod wayland;

/// A system call with no arguments.
#[must_use]
pub fn syscall0(number: u64) -> i64 {
    let out: i64;
    // SAFETY: `syscall` with a number this crate chose. The kernel destroys
    // `rcx` and `r11`, which are declared clobbered.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out,
             lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// One argument.
#[must_use]
pub fn syscall1(number: u64, a: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a,
             lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Two.
#[must_use]
pub fn syscall2(number: u64, a: u64, b: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a, in("rsi") b,
             lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Three.
#[must_use]
pub fn syscall3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a, in("rsi") b,
             in("rdx") c, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Four. The fourth argument goes in `r10`, not `rcx`: the instruction takes
/// `rcx` for the return address, and Linux's convention moves it out of the way.
#[must_use]
pub fn syscall4(number: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Five.
#[must_use]
pub fn syscall5(number: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, in("r8") e,
             lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Six, which is as many as the instruction carries.
#[must_use]
pub fn syscall6(number: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let out: i64;
    // SAFETY: as above.
    unsafe {
        asm!("syscall", inlateout("rax") number as i64 => out, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
             lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    out
}

/// Write to a descriptor.
pub fn write(descriptor: u64, bytes: &[u8]) -> i64 {
    syscall3(
        call::WRITE,
        descriptor,
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    )
}

/// Write a line to standard output.
///
/// One write, including the newline. Standard output here is a channel to a log
/// that puts a prefix on every write it is given, so a line and its newline
/// sent separately arrive as a line and then an empty one.
pub fn say(text: &str) {
    fmt::Line::new().text(text).say();
}

/// Stop the whole program.
pub fn exit_group(status: u64) -> ! {
    let _ = syscall1(call::EXIT_GROUP, status);
    // A kernel that returned from `exit_group` is one this program cannot
    // reason about any further, so it stops here rather than carrying on into
    // whatever follows in memory.
    loop {
        core::hint::spin_loop();
    }
}

/// Stop, saying which step failed.
///
/// The status *is* the message: a number a test script maps back to a step. A
/// program that printed an explanation and exited zero would be a program whose
/// failure a script has to parse English to notice.
pub fn fail(step: u64) -> ! {
    exit_group(step)
}

/// Stop unless the condition holds.
pub fn expect(condition: bool, step: u64) {
    if !condition {
        fail(step);
    }
}

/// Anonymous memory, or `None`.
pub fn map_anonymous(bytes: usize) -> Option<*mut u8> {
    /// `PROT_READ | PROT_WRITE`, `MAP_PRIVATE | MAP_ANONYMOUS`.
    const PROT: u64 = 3;
    const FLAGS: u64 = 0x22;
    let at = syscall6(call::MMAP, 0, bytes as u64, PROT, FLAGS, u64::MAX, 0);
    if at < 0 {
        None
    } else {
        Some(at as *mut u8)
    }
}

/// Where the initial stack pointer was left, for reading `argv` and the
/// auxiliary vector.
///
/// Recorded by the entry stub, because by the time any Rust code runs the
/// register that held it has been used for something else.
static mut STACK: *const u64 = core::ptr::null();

/// The stack as the kernel left it: `argc`, the arguments, the environment and
/// the auxiliary vector, in that order.
#[must_use]
pub fn stack() -> *const u64 {
    // SAFETY: written once by the entry stub before anything else runs, and
    // only read afterwards. These programs are single-threaded until they call
    // `clone` themselves, and none of them does before reading this.
    unsafe { STACK }
}

/// Not called from Rust: the entry stub below jumps here.
///
/// # Safety
///
/// Called once, by `_start`, with the kernel's stack pointer.
#[doc(hidden)]
pub unsafe fn __guest_started(stack: *const u64) {
    // SAFETY: as above.
    unsafe { STACK = stack };
}

/// The entry point every program here shares.
///
/// # Why this is assembly and not a function
///
/// `_start` is not called — it is *jumped to*, by the kernel, and the stack
/// pointer it is entered with is sixteen-byte aligned. A function reached by
/// `call` is entered with the return address already pushed, so its stack
/// pointer is eight past an alignment boundary, and a compiler lays out every
/// frame it emits on that assumption.
///
/// Writing `_start` as an `extern "C" fn` therefore produces a function whose
/// every stack slot is eight bytes out. Nothing notices until the first
/// sixteen-byte aligned access, which for a compiler zeroing a local is the
/// very common `movaps %xmm0, n(%rsp)` — and that instruction *faults* on a
/// misaligned address. What it looked like was a general protection fault
/// three calls into the first compiled program to run here, with nothing in
/// the program's own source anywhere near it.
///
/// So the entry point is four instructions of assembly, which is what the
/// `_start` in every real C library is and is why it is written in assembly
/// there too:
///
/// * `xor ebp, ebp` ends the frame-pointer chain, so a debugger walking it
///   stops here rather than following whatever the kernel left.
/// * `mov rdi, rsp` keeps the stack the kernel left, which is where `argc`,
///   the arguments, the environment and the auxiliary vector are. It has to be
///   taken before the next instruction moves it.
/// * `and rsp, -16` is the alignment the compiled code below is expecting.
/// * `call` rather than `jmp`, so that the callee is entered the way it was
///   compiled to be.
#[macro_export]
macro_rules! guest_main {
    ($body:path) => {
        core::arch::global_asm!(
            ".globl _start",
            ".type _start, @function",
            "_start:",
            "xor ebp, ebp",
            "mov rdi, rsp",
            "and rsp, -16",
            "call __guest_main",
            // Unreachable: the program below never returns. An instruction that
            // faults is better here than whatever bytes follow in the image.
            "ud2",
        );

        /// The first Rust code in the program.
        ///
        /// # Safety
        ///
        /// Called once, by the stub above.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn __guest_main(stack: *const u64) -> ! {
            // SAFETY: called once, before anything else in this program runs.
            unsafe { $crate::__guest_started(stack) };
            let run: fn() -> ! = $body;
            run()
        }

        #[panic_handler]
        fn panic(_info: &core::panic::PanicInfo) -> ! {
            // A panic is a failure with no number of its own, so it gets one.
            // Silent would be worse: a program that stopped without saying
            // anything is indistinguishable from a kernel that lost it.
            $crate::say("guest: panicked");
            $crate::exit_group(99)
        }
    };
}
