//! Linux, translated again, for a program built for i386.
//!
//! A thirty-two bit Linux program is not a sixty-four bit one with smaller
//! numbers. It is a *different system-call interface*: `write` is call number 1
//! on x86-64 and 4 on i386, `exit_group` is 231 and 252, and the numbering is
//! not a translation of one into the other — it is two lists that grew
//! separately. That is why a kernel that supports both has two tables, and why
//! Linux calls the second one `CONFIG_IA32_EMULATION` rather than a flag.
//!
//! It also arrives by a different door. A sixty-four bit program uses the
//! `syscall` instruction; a thirty-two bit one uses `int 0x80`, a software
//! interrupt, which is why there is a gate at vector 0x80 with a privilege
//! level that lets ring 3 take it.
//!
//! And the arguments are in different registers: `ebx`, `ecx`, `edx`, `esi`,
//! `edi`, `ebp`, rather than `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`.
//!
//! # What this is for
//!
//! One thing, named plainly: the Steam bootstrap is an i386 binary, and so is a
//! great deal of older Linux software. A machine that runs only sixty-four bit
//! programs cannot start any of it.
//!
//! # What is translated here, and what is not
//!
//! The calls whose arguments are integers and pointers, which behave the same
//! whatever width the program is. Everything that passes a *structure* is
//! refused by name, because the structures are different: `struct stat64` is
//! not `struct stat`, an `iovec` is eight bytes rather than sixteen, a
//! `timespec` is eight rather than sixteen, and a `sigaction` has its fields in
//! another order. Each of those is a translation of its own and none is hidden
//! behind a stub that returns success.
//!
//! So a thirty-two bit program that reads, writes, maps memory and exits runs;
//! one that asks for the time or installs a signal handler is told, by name,
//! that the call is not translated.

extern crate alloc;

use crate::arch::syscall::Frame;

use super::linux::error;
use super::linux_files as files;
use super::linux_memory as memory;

/// i386's system-call numbers.
///
/// Its own list, which is the whole point of this file. A few of them are worth
/// seeing next to their sixty-four bit counterparts: `write` is 4 here and 1
/// there, `exit_group` is 252 and 231, `mmap2` is 192 and there is no `mmap2`
/// on x86-64 at all.
mod call {
    pub const EXIT: u64 = 1;
    pub const READ: u64 = 3;
    pub const WRITE: u64 = 4;
    pub const OPEN: u64 = 5;
    pub const CLOSE: u64 = 6;
    pub const UNLINK: u64 = 10;
    pub const LSEEK: u64 = 19;
    pub const GETPID: u64 = 20;
    pub const ACCESS: u64 = 33;
    pub const MKDIR: u64 = 39;
    pub const RMDIR: u64 = 40;
    pub const DUP: u64 = 41;
    pub const PIPE: u64 = 42;
    pub const BRK: u64 = 45;
    pub const GETUID: u64 = 24;
    pub const GETGID: u64 = 47;
    pub const GETEUID: u64 = 49;
    pub const GETEGID: u64 = 50;
    pub const IOCTL: u64 = 54;
    pub const DUP2: u64 = 63;
    pub const MUNMAP: u64 = 91;
    pub const UNAME: u64 = 122;
    pub const MPROTECT: u64 = 125;
    pub const GETCWD: u64 = 183;
    /// `mmap2`, whose offset is in *pages* rather than bytes -- which is how a
    /// thirty-two bit program maps a file more than four gigabytes into it.
    pub const MMAP2: u64 = 192;
    pub const EXIT_GROUP: u64 = 252;
    pub const SET_THREAD_AREA: u64 = 243;
    pub const SET_TID_ADDRESS: u64 = 258;
    pub const GETTID: u64 = 224;
    pub const SCHED_YIELD: u64 = 158;
    pub const PIPE2: u64 = 331;
    pub const GETRANDOM: u64 = 355;
    /// The one that arrives instead of `open` from anything recent.
    pub const OPENAT: u64 = 295;
}

/// Calls translated, and calls refused.
static TRANSLATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many thirty-two bit calls have been translated and refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        TRANSLATED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// One `int 0x80` from a thirty-two bit program.
///
/// The arguments come out of the frame in i386's registers rather than
/// x86-64's, and every one of them is narrowed to thirty-two bits first. That
/// narrowing is not decoration: a thirty-two bit program sets `ebx`, and the
/// top half of `rbx` is whatever the register happened to hold. A pointer read
/// as sixty-four bits would be a pointer with rubbish above bit thirty-one, and
/// the range check would refuse it for the wrong reason.
pub fn dispatch(frame: &mut Frame) -> u64 {
    let number = frame.rax & 0xFFFF_FFFF;
    // `ebx`, `ecx`, `edx`, `esi`, `edi`, `ebp` -- i386's six, in i386's order.
    //
    // `ecx` arrives in the frame's `r10` slot. A `Frame` has no `rcx` of its
    // own: at the sixty-four bit boundary that register holds the return
    // address the `syscall` instruction put there, so the slot next to it holds
    // the fourth argument instead. The `int 0x80` stub puts the caller's `rcx`
    // where the fourth argument would be, which is the one place the two
    // entries differ -- and is written down here and at the stub.
    let argument0 = frame.rbx & 0xFFFF_FFFF;
    let argument1 = frame.r10 & 0xFFFF_FFFF;
    let argument2 = frame.rdx & 0xFFFF_FFFF;
    let argument3 = frame.rsi & 0xFFFF_FFFF;
    let argument4 = frame.rdi & 0xFFFF_FFFF;
    let argument5 = frame.rbp & 0xFFFF_FFFF;

    match number {
        // Output and input, which behave the same at either width: a buffer and
        // a count are a buffer and a count.
        call::WRITE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::write(argument0, argument1, argument2)
        }
        call::READ => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::read(argument0, argument1, argument2)
        }
        call::CLOSE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::close(argument0)
        }
        call::OPEN => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::openat(files::AT_FDCWD as u32 as u64, argument0, argument1)
        }
        call::OPENAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::openat(argument0, argument1, argument2)
        }
        call::LSEEK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::lseek(argument0, argument1, argument2)
        }
        call::ACCESS => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::faccessat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::MKDIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::mkdirat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::UNLINK | call::RMDIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::unlinkat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::GETCWD => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getcwd(argument0, argument1)
        }
        call::GETRANDOM => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getrandom(argument0, argument1)
        }
        call::DUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::dup(argument0)
        }
        call::DUP2 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::dup2(argument0, argument1)
        }
        call::PIPE | call::PIPE2 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // A thirty-two bit pipe writes two `int`s, which are the same two
            // `int`s at either width.
            files::pipe2(argument0, if number == call::PIPE { 0 } else { argument1 })
        }

        // Memory. `mmap2`'s offset is in pages, which is the whole reason it
        // exists: a thirty-two bit program cannot name a byte offset past four
        // gigabytes in a register.
        call::MMAP2 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            memory::mmap(
                argument0,
                argument1,
                argument2,
                argument3,
                argument4,
                argument5.saturating_mul(4096),
            )
        }
        call::MUNMAP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            memory::munmap(argument0, argument1)
        }
        call::MPROTECT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            memory::mprotect(argument0, argument1, argument2)
        }
        // Refused for the same reason as at sixty-four bits: every libc worth
        // running falls back to `mmap`.
        call::BRK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            error::ENOMEM
        }

        call::UNAME => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // `struct utsname` is six fixed sixty-five byte fields at either
            // width: the one structure in this list that does not change.
            super::linux::uname_into(argument0)
        }
        call::GETPID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::sched::current_process().map_or(error::ENOSYS, |process| process.id.0)
        }
        call::GETTID | call::SET_TID_ADDRESS => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::arch::percpu::current_thread()
        }
        call::GETUID | call::GETGID | call::GETEUID | call::GETEGID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            0
        }
        call::SCHED_YIELD => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::sched::yield_now();
            0
        }
        call::IOCTL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            error::ENOTTY
        }
        // Thread-local storage on i386 is a *segment*, set up by installing a
        // descriptor in the local descriptor table -- not a base register. That
        // is a different mechanism from `arch_prctl`, needs an LDT this system
        // does not have, and is refused rather than approximated.
        call::SET_THREAD_AREA => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::kprintln!(
                "[linux32] set_thread_area needs a local descriptor table; answering ENOSYS"
            );
            error::ENOSYS
        }
        call::EXIT | call::EXIT_GROUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux::exit_now(argument0)
        }
        _ => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Named with its i386 number, which is not the number the same call
            // has at sixty-four bits -- so a reader looking it up must look it
            // up in the right table.
            crate::kprintln!(
                "[linux32] i386 call {number} is not translated yet; answering ENOSYS"
            );
            error::ENOSYS
        }
    }
}
