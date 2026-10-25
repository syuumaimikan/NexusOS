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
//! Scalar arguments can share the native implementation. Structures require
//! explicit translation: packed stat64, eight-byte iovecs and legacy timespecs
//! are handled here. Fixed-width statx and time64 layouts can be shared.
//! i386 CLONE_SETTLS supplies a user_desc pointer, and the scheduler carries
//! each thread's GDT slots, selectors and GS base. Unimplemented structures,
//! including i386 signal actions/frames, are still refused by name.

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
    pub const CHDIR: u64 = 12;
    pub const TIME: u64 = 13;
    pub const FCHDIR: u64 = 133;
    pub const GETTIMEOFDAY: u64 = 78;
    pub const MKDIRAT: u64 = 296;
    pub const MKNODAT: u64 = 297;
    pub const UNLINKAT: u64 = 301;
    pub const LSEEK: u64 = 19;
    pub const GETPID: u64 = 20;
    pub const KILL: u64 = 37;
    pub const WAIT4: u64 = 114;
    pub const EXECVE: u64 = 11;
    pub const EXECVEAT: u64 = 358;
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
    pub const GET_THREAD_AREA: u64 = 244;
    pub const SET_TID_ADDRESS: u64 = 258;
    pub const GETTID: u64 = 224;
    pub const SCHED_YIELD: u64 = 158;
    pub const PIPE2: u64 = 331;
    pub const GETRANDOM: u64 = 355;
    /// The one that arrives instead of `open` from anything recent.
    pub const OPENAT: u64 = 295;
    pub const STAT64: u64 = 195;
    pub const LSTAT64: u64 = 196;
    pub const FSTAT64: u64 = 197;
    pub const FSTATAT64: u64 = 300;
    pub const PREAD64: u64 = 180;
    pub const LLSEEK: u64 = 140;
    pub const READV: u64 = 145;
    pub const WRITEV: u64 = 146;
    pub const FTRUNCATE: u64 = 93;
    pub const FTRUNCATE64: u64 = 194;
    pub const CLOCK_GETTIME: u64 = 265;
    pub const CLOCK_GETTIME64: u64 = 403;
    pub const NANOSLEEP: u64 = 162;
    pub const CLOCK_NANOSLEEP: u64 = 267;
    pub const CLOCK_NANOSLEEP64: u64 = 407;
    pub const UGETRLIMIT: u64 = 191;
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

    if matches!(number, 359..=364 | 366 | 369 | 371) {
        TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    match number {
        323 | 328 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_eventfd::create(argument0, if number == 323 { 0 } else { argument1 })
        }
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
            let answer = files::lseek(argument0, argument1 as u32 as i32 as i64 as u64, argument2);
            if answer as i64 > i64::from(i32::MAX) {
                (-75i64) as u64
            } else {
                answer
            }
        }
        call::LLSEEK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let Some((out, _)) = crate::arch::syscall::user_range(argument3, 8, 8) else {
                return error::EFAULT;
            };
            let answer = files::lseek(argument0, (argument1 << 32) | argument2, argument4);
            if (answer as i64) < 0 {
                answer
            } else {
                unsafe {
                    (out as *mut u64).write_unaligned(answer);
                }
                0
            }
        }
        call::READV | call::WRITEV => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            vectors32(argument0, argument1, argument2, number == call::WRITEV)
        }
        call::PREAD64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::pread64(
                argument0,
                argument1,
                argument2,
                argument3 | (argument4 << 32),
            )
        }
        call::FTRUNCATE | call::FTRUNCATE64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let length = argument1
                | if number == call::FTRUNCATE64 {
                    argument2 << 32
                } else {
                    0
                };
            files::ftruncate(argument0, length)
        }
        call::STAT64 | call::LSTAT64 | call::FSTAT64 | call::FSTATAT64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            match number {
                call::FSTATAT64 => {
                    files::stat64_i386(argument0, Some(argument1), argument2, argument3)
                }
                call::FSTAT64 => files::stat64_i386(argument0, None, argument1, 0),
                _ => {
                    files::stat64_i386(files::AT_FDCWD as u32 as u64, Some(argument0), argument1, 0)
                }
            }
        }
        call::CLOCK_GETTIME | call::CLOCK_GETTIME64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let result = if number == call::CLOCK_GETTIME64 {
                super::linux::clock_gettime(argument0, argument1)
            } else {
                clock32(argument0, argument1)
            };
            result
        }
        call::TIME => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            time32(argument0)
        }
        call::GETTIMEOFDAY => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            gettimeofday32(argument0)
        }
        call::NANOSLEEP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            sleep32(argument0, argument1)
        }
        call::CLOCK_NANOSLEEP | call::CLOCK_NANOSLEEP64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if number == call::CLOCK_NANOSLEEP64 {
                let translated = Frame {
                    rdi: argument0,
                    rsi: argument1,
                    rdx: argument2,
                    r10: argument3,
                    ..*frame
                };
                super::linux_more::translate(230, &translated).unwrap_or(error::ENOSYS)
            } else if argument0 > 1 || argument1 & !1 != 0 {
                error::EINVAL
            } else if argument1 == 0 {
                sleep32(argument2, argument3)
            } else {
                let Some((input, _)) = crate::arch::syscall::user_range(argument2, 8, 8) else {
                    return error::EFAULT;
                };
                let [seconds, nanos] = unsafe { (input as *const [i32; 2]).read_unaligned() };
                if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
                    return error::EINVAL;
                }
                let uptime = crate::arch::time::uptime_ms();
                let now = if argument0 == 0 {
                    crate::drivers::rtc::now()
                        .unwrap_or(uptime / 1000)
                        .saturating_mul(1000)
                } else {
                    uptime
                };
                let deadline = seconds as u64 * 1000 + (nanos as u64).div_ceil(1_000_000);
                if deadline > now {
                    crate::sched::sleep_ms(deadline - now);
                }
                0
            }
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
            files::unlinkat(
                files::AT_FDCWD as u32 as u64,
                argument0,
                if number == call::RMDIR { 0x200 } else { 0 },
            )
        }
        call::CHDIR | call::FCHDIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if number == call::CHDIR {
                super::linux_paths::chdir(argument0)
            } else {
                super::linux_paths::fchdir(argument0)
            }
        }
        83 => super::linux_paths::symlinkat(argument0, files::AT_FDCWD as u32 as u64, argument1),
        304 => super::linux_paths::symlinkat(argument0, argument1, argument2),
        102 => socketcall(argument0, argument1),
        // The newer i386 socket ABI assigns one syscall to each operation.
        // Steam's bootstrap uses this interface rather than legacy socketcall.
        359 => super::linux_socket::socket(argument0, argument1, argument2),
        360 => super::linux_socket::socketpair(argument0, argument1, argument2, argument3),
        361 => super::linux_socket::bind(argument0, argument1, argument2),
        362 => super::linux_socket::connect(argument0, argument1, argument2),
        363 => super::linux_socket::listen(argument0, argument1),
        364 => super::linux_socket::accept(argument0, argument1, argument2, argument3),
        366 => super::linux_socket::setsockopt(argument0, argument1, argument2),
        369 => super::linux_socket::send_to(
            argument0, argument1, argument2, argument3, argument4, argument5,
        ),
        371 => super::linux_socket::receive_from(
            argument0, argument1, argument2, argument3, argument4, argument5,
        ),
        call::MKDIRAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::mkdirat(argument0, argument1)
        }
        call::UNLINKAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::unlinkat(argument0, argument1, argument2)
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
        call::KILL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let result = super::linux_signal::kill(argument0, argument1);
            crate::kprintln!(
                "[linux32] kill pid={} signal={} -> {}",
                argument0,
                argument1,
                result as i64
            );
            result
        }
        call::EXECVE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_exec::execve32(argument0, argument1, argument2, frame)
        }
        call::EXECVEAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_exec::execveat32(
                argument0, argument1, argument2, argument3, argument4, frame,
            )
        }
        call::SET_TID_ADDRESS => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_threads::set_tid_address(argument0)
        }
        call::GETTID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::arch::percpu::current_thread()
        }
        call::GETUID | call::GETGID | call::GETEUID | call::GETEGID | 199..=202 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            0
        }
        call::UGETRLIMIT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_more::getrlimit32(argument0, argument1)
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
        call::SET_THREAD_AREA | call::GET_THREAD_AREA => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            thread_area(argument0, number == call::GET_THREAD_AREA)
        }
        186 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_signal::sigaltstack(argument0, argument1, true)
        }
        116 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_more::sysinfo32(argument0)
        }
        311 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_threads::set_robust_list32(argument0, argument1)
        }
        312 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_threads::get_robust_list32(argument0, argument1, argument2)
        }
        270 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_signal::tgkill(argument0, argument1, argument2)
        }
        call::MKNODAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::mknodat(argument0, argument1, argument2)
        }
        120 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let result = super::linux_threads::clone(
                argument0, argument1, argument2, argument4, argument3, frame,
            );
            crate::kprintln!(
                "[linux32] clone flags={:#x} stack={:#x} parent_tid={:#x} tls={:#x} child_tid={:#x} -> {}",
                argument0,
                argument1,
                argument2,
                argument3,
                argument4,
                result as i64
            );
            result
        }
        143 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_more::flock(argument0, argument1)
        }
        15 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_paths::chmodat(
                super::linux_files::AT_FDCWD as u32 as u64,
                argument0,
                argument1,
                0,
            )
        }
        94 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_paths::chmod_descriptor(argument0)
        }
        306 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_paths::chmodat(argument0, argument1, argument2, 0)
        }
        439 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_paths::chmodat(argument0, argument1, argument2, argument3)
        }
        268 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let result = super::linux_more::statfs64_path(argument0, argument1, argument2);
            crate::kprintln!(
                "[linux32] statfs64 path={:#x} size={} output={:#x} -> {}",
                argument0,
                argument1,
                argument2,
                result as i64
            );
            result
        }
        269 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let result = super::linux_more::statfs64_descriptor(argument0, argument1, argument2);
            crate::kprintln!(
                "[linux32] fstatfs64 fd={} size={} output={:#x} -> {}",
                argument0,
                argument1,
                argument2,
                result as i64
            );
            result
        }
        435 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            clone3(argument0, argument1, frame)
        }
        call::WAIT4 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_fork::wait4(argument0, argument1, argument2, argument3, true)
        }
        240 | 422 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if number == 240 {
                super::linux_threads::futex32(argument0, argument1, argument2, argument3, argument5)
            } else {
                super::linux_threads::futex(argument0, argument1, argument2, argument3, argument5)
            }
        }
        174 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_signal::sigaction32(argument0, argument1, argument2, argument3)
        }
        175 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_signal::sigprocmask32(argument0, argument1, argument2, argument3)
        }
        126 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_signal::sigprocmask_legacy32(argument0, argument1, argument2)
        }
        168 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_poll::poll(argument0, argument1, argument2 as u32 as i32 as i64)
        }
        220 => {
            // linux_dirent64 has the same fixed-width wire layout at both
            // widths; libc readdir uses this on i386 as well as x86-64.
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getdents64(argument0, argument1, argument2)
        }
        356 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::memfd_create(argument0, argument1)
        }
        254 | 329 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if number == 254 && (argument0 as u32 as i32) <= 0 {
                error::EINVAL
            } else {
                super::linux_poll::create(if number == 254 { 0 } else { argument0 })
            }
        }
        255 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_poll::control(argument0, argument1, argument2, argument3)
        }
        256 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_poll::wait(
                argument0,
                argument1,
                argument2,
                argument3 as u32 as i32 as i64,
            )
        }
        117 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_semaphore::ipc32(
                argument0, argument1, argument2, argument3, argument4, argument5,
            )
        }
        393 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_semaphore::get(argument0, argument1, argument2)
        }
        394 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_semaphore::control(
                argument0,
                argument1,
                argument2,
                argument3,
                super::linux_semaphore::Layout::I386,
            )
        }
        420 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::linux_semaphore::operate(argument0, argument1, argument2, argument3, false)
        }
        call::EXIT | call::EXIT_GROUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if number == call::EXIT {
                super::linux::exit_thread(argument0)
            } else {
                super::linux::exit_now(argument0)
            }
        }
        _ => {
            // These calls pass only scalar values/byte strings, or explicitly
            // fixed-width structs (statx/prlimit64), at both ABI widths.
            let mapped = match number {
                38 => Some(82),
                302 => Some(264),
                353 => Some(316),
                55 | 221 => Some(72),
                85 => Some(89),
                305 => Some(267),
                219 => Some(28),
                340 => Some(302),
                383 => Some(332),
                118 => Some(74),
                148 => Some(75),
                172 => Some(157),
                _ => None,
            };
            if let Some(mapped) = mapped {
                let translated = Frame {
                    rdi: argument0,
                    rsi: argument1,
                    rdx: argument2,
                    r10: argument3,
                    r8: argument4,
                    r9: argument5,
                    ..*frame
                };
                if let Some(answer) = super::linux_more::translate(mapped, &translated) {
                    TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return answer;
                }
            }
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

/// i386's `clone3` uses the same 64-byte Linux structure as x86-64, while the
/// call itself arrives with two arguments in 32-bit registers. Keep the child
/// frame untouched so both return paths resume through `int 0x80`.
fn clone3(arguments: u64, size: u64, frame: &Frame) -> u64 {
    const SMALLEST: u64 = 64;
    if size < SMALLEST {
        return error::EINVAL;
    }
    let Some((at, _)) = crate::arch::syscall::user_range(arguments, SMALLEST, SMALLEST) else {
        return error::EFAULT;
    };
    let field = |offset: usize| unsafe {
        core::ptr::read_unaligned((at as *const u8).add(offset) as *const u64)
    };
    let flags = field(0);
    let child_tid = field(16);
    let parent_tid = field(24);
    let exit_signal = field(32) & 0xFF;
    let stack = field(40);
    let stack_size = field(48);
    let tls = field(56);
    let top = if stack == 0 {
        0
    } else {
        match stack.checked_add(stack_size) {
            Some(top) if top < nexus_abi::layout::USER_SPACE_END => top,
            _ => return error::EINVAL,
        }
    };
    let result =
        super::linux_threads::clone(flags | exit_signal, top, parent_tid, child_tid, tls, frame);
    crate::kprintln!(
        "[linux32] clone3 flags={:#x} stack={top:#x} parent_tid={parent_tid:#x} child_tid={child_tid:#x} tls={tls:#x} -> {}",
        flags | exit_signal,
        result as i64
    );
    result
}

fn clock32(which: u64, address: u64) -> u64 {
    const REALTIME: u64 = 0;
    const MONOTONIC: u64 = 1;
    const MONOTONIC_RAW: u64 = 4;
    const REALTIME_COARSE: u64 = 5;
    const MONOTONIC_COARSE: u64 = 6;
    const BOOTTIME: u64 = 7;
    const REALTIME_ALARM: u64 = 8;
    const BOOTTIME_ALARM: u64 = 9;
    let realtime = matches!(which, REALTIME | REALTIME_COARSE | REALTIME_ALARM);
    let monotonic = matches!(
        which,
        MONOTONIC | MONOTONIC_RAW | MONOTONIC_COARSE | BOOTTIME | BOOTTIME_ALARM
    );
    if !realtime && !monotonic {
        return error::EINVAL;
    }
    let Some((out, _)) = crate::arch::syscall::user_range(address, 8, 8) else {
        return error::EFAULT;
    };
    let milliseconds = crate::arch::time::uptime_ms();
    let seconds = if realtime {
        crate::drivers::rtc::now().unwrap_or(milliseconds / 1000)
    } else {
        milliseconds / 1000
    };
    if seconds > i32::MAX as u64 {
        return (-75i64) as u64;
    }
    let nanos = (milliseconds % 1000) * 1_000_000;
    unsafe {
        (out as *mut [i32; 2]).write_unaligned([seconds as i32, nanos as i32]);
    }
    0
}

/// i386 `time(time_t *)`, with Linux's signed 32-bit time ABI.
fn time32(address: u64) -> u64 {
    let seconds = crate::drivers::rtc::now().unwrap_or(crate::arch::time::uptime_ms() / 1000);
    if seconds > i32::MAX as u64 {
        return (-75i64) as u64; // EOVERFLOW
    }
    if address != 0 {
        let Some((out, _)) = crate::arch::syscall::user_range(address, 4, 4) else {
            return error::EFAULT;
        };
        // SAFETY: the caller's four-byte i386 time_t was checked above.
        unsafe { (out as *mut i32).write_unaligned(seconds as i32) };
    }
    seconds as u32 as u64
}

/// i386 `gettimeofday(timeval32 *, timezone *)`; the obsolete timezone is
/// ignored, as it is on Linux. The fractional component follows the kernel's
/// monotonic timer rather than introducing a second wall-clock source.
fn gettimeofday32(address: u64) -> u64 {
    if address == 0 {
        return 0;
    }
    let Some((out, _)) = crate::arch::syscall::user_range(address, 8, 8) else {
        return error::EFAULT;
    };
    let seconds = crate::drivers::rtc::now().unwrap_or(crate::arch::time::uptime_ms() / 1000);
    if seconds > i32::MAX as u64 {
        return (-75i64) as u64; // EOVERFLOW
    }
    let microseconds = ((crate::arch::time::uptime_ms() % 1000) * 1000) as i32;
    // SAFETY: `user_range` checked the eight-byte timeval32 output.
    unsafe {
        (out as *mut [i32; 2]).write_unaligned([seconds as i32, microseconds]);
    }
    0
}

fn vectors32(descriptor: u64, address: u64, count: u64, writing: bool) -> u64 {
    if count > 16 {
        return error::EINVAL;
    }
    if count == 0 {
        return 0;
    }
    let Some((pointer, _)) = crate::arch::syscall::user_range(address, count * 8, 16 * 8) else {
        return error::EFAULT;
    };
    let mut done = 0u64;
    for index in 0..count as usize {
        let [buffer, length] = unsafe { (pointer as *const [u32; 2]).add(index).read_unaligned() };
        let answer = if writing {
            files::write(descriptor, u64::from(buffer), u64::from(length))
        } else {
            files::read(descriptor, u64::from(buffer), u64::from(length))
        };
        if (answer as i64) < 0 {
            return if done == 0 { answer } else { done };
        }
        done += answer;
        if answer < u64::from(length) {
            break;
        }
    }
    done
}

fn sleep32(request: u64, remaining: u64) -> u64 {
    let Some((input, _)) = crate::arch::syscall::user_range(request, 8, 8) else {
        return error::EFAULT;
    };
    let [seconds, nanos] = unsafe { (input as *const [i32; 2]).read_unaligned() };
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return error::EINVAL;
    }
    let output = if remaining == 0 {
        None
    } else {
        let Some((pointer, _)) = crate::arch::syscall::user_range(remaining, 8, 8) else {
            return error::EFAULT;
        };
        Some(pointer)
    };
    let milliseconds = seconds as u64 * 1000 + (nanos as u64).div_ceil(1_000_000);
    if milliseconds != 0 {
        crate::sched::sleep_ms(milliseconds);
    }
    if let Some(output) = output {
        unsafe {
            (output as *mut u64).write_unaligned(0);
        }
    }
    0
}

/// Linux's 16-byte user_desc; descriptor slots are private to each thread.
fn thread_area(address: u64, get: bool) -> u64 {
    let Some((pointer, _)) = crate::arch::syscall::user_range(address, 16, 16) else {
        return error::EFAULT;
    };
    let fields = unsafe { (pointer as *const [u32; 4]).read_unaligned() };
    if get {
        let Some(descriptor) = crate::sched::compat_tls_get(fields[0]) else {
            return error::EINVAL;
        };
        let base = ((descriptor >> 16) & 0xffff)
            | ((descriptor >> 32) & 0xff) << 16
            | (descriptor >> 56) << 24;
        let limit = (descriptor & 0xffff) | ((descriptor >> 48) & 15) << 16;
        let flags = if descriptor == 0 {
            0x28
        } else {
            1 | (((descriptor >> 42) & 1) << 1)
                | (((!descriptor >> 41) & 1) << 3)
                | (((descriptor >> 55) & 1) << 4)
                | (((descriptor >> 52) & 1) << 6)
        };
        unsafe {
            (pointer as *mut [u32; 4]).write_unaligned([
                fields[0],
                base as u32,
                limit as u32,
                flags as u32,
            ]);
        }
        return 0;
    }
    let entry = fields[0];
    let descriptor = match tls_descriptor(fields) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let Some(selected) = crate::sched::compat_tls_set(entry, descriptor) else {
        return if entry == u32::MAX {
            (-3i64) as u64
        } else {
            error::EINVAL
        };
    };
    unsafe {
        (pointer as *mut u32).write_unaligned(selected);
    }
    0
}

/// CLONE_SETTLS supplies a user_desc pointer on i386, not an FS base.
pub(super) fn clone_tls(address: u64) -> Result<(u32, u64), u64> {
    let (pointer, _) = crate::arch::syscall::user_range(address, 16, 16).ok_or(error::EFAULT)?;
    let fields = unsafe { (pointer as *const [u32; 4]).read_unaligned() };
    let entry = fields[0];
    if !(crate::arch::gdt::TLS_FIRST..crate::arch::gdt::TLS_FIRST + 3).contains(&entry) {
        return Err(error::EINVAL);
    }
    let descriptor = tls_descriptor(fields)?;
    // A clone cannot reload a selector referring to an absent descriptor.
    if descriptor == 0 {
        return Err(error::EINVAL);
    }
    Ok((entry, descriptor))
}

fn tls_descriptor(fields: [u32; 4]) -> Result<u64, u64> {
    let [_, base, limit, flags] = fields;
    let empty = base == 0 && limit == 0 && flags & 0x7f == 0x28;
    if !empty && (flags & 1 == 0 || (flags >> 1) & 3 > 1 || flags & 0x20 != 0 || limit > 0xfffff) {
        return Err(error::EINVAL);
    }
    let descriptor = if empty {
        0
    } else {
        u64::from(limit & 0xffff)
            | (u64::from(base & 0xffff) << 16)
            | (u64::from((base >> 16) & 255) << 32)
            | ((0xf0u64 | if flags & 8 == 0 { 2 } else { 0 } | (u64::from((flags >> 1) & 1) << 2))
                << 40)
            | (u64::from((limit >> 16) & 15) << 48)
            | (1u64 << 54)
            | (u64::from((flags >> 4) & 1) << 55)
            | (u64::from((flags >> 6) & 1) << 52)
            | (u64::from(base >> 24) << 56)
    };
    Ok(descriptor)
}

/// Legacy i386 socketcall carries an array of 32-bit words, not a native
/// msghdr or a 64-bit register array. Translate only proven scalar/byte layouts.
fn socketcall(operation: u64, arguments: u64) -> u64 {
    use super::linux_socket as socket;
    let count = match operation {
        1 | 2 | 3 | 5 => 3,
        4 => 2,
        8 | 9 | 10 | 18 => 4,
        11 | 12 => 6,
        14 | 15 => 5,
        _ => {
            crate::kprintln!("[linux32] socketcall operation {operation} is not implemented");
            return error::ENOSYS;
        }
    };
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let Some((pointer, _)) =
        crate::arch::syscall::user_range_for(&process, arguments, count * 4, 24, false)
    else {
        return error::EFAULT;
    };
    let mut values = [0u64; 6];
    for index in 0..count as usize {
        values[index] = unsafe { (pointer as *const u32).add(index).read_unaligned() } as u64;
    }
    let [a, b, c, d, e, f] = values;
    // The native stream endpoint has no descriptor-level nonblocking mode yet.
    // MSG_DONTWAIT receives below are implemented, but silently ignoring a
    // requested SOCK_NONBLOCK would create a blocking descriptor instead.
    if matches!(operation, 1 | 8) && b & 0o4000 != 0 {
        return error::ENOSYS;
    }
    TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    match operation {
        1 => {
            crate::kprintln!("[linux32] socketcall socket family={a} type={b} protocol={c}");
            let descriptor = socket::socket(a, b, c);
            if descriptor as i64 >= 0 && b & 0o2000000 != 0 {
                super::linux_more::mark_cloexec(process.id.0, descriptor);
            }
            descriptor
        }
        2 => socket::bind(a, b, c),
        3 => socket::connect(a, b, c),
        4 => socket::listen(a, b),
        5 => socket::accept(a, b, c, 0),
        8 => {
            let result = socket::socketpair(a, b, c, d);
            if result == 0 && b & 0o2000000 != 0 {
                // socketpair already checked the output. Snapshot its fixed
                // two-word layout; exec currently records but does not enforce
                // these flags, consistently with the existing descriptor ABI.
                let descriptors = unsafe { (d as *const [u32; 2]).read_unaligned() };
                for descriptor in descriptors {
                    super::linux_more::mark_cloexec(process.id.0, descriptor as u64);
                }
            }
            result
        }
        9 if d & !0x4000 == 0 => socket::send(a, b, c, d),
        10 if d & !0x40 == 0 => socket::receive(a, b, c, d),
        11 if d & !0x4000 == 0 => socket::send_to(a, b, c, d, e, f),
        12 if d & !0x40 == 0 => socket::receive_from(a, b, c, d, e, f),
        18 if d == 0 => socket::accept(a, b, c, 0),
        14 => socket::setsockopt(a, b, c),
        15 => {
            crate::kprintln!(
                "[linux32] socketcall option op={operation} fd={a} level={b} option={c} length_arg={e}; not implemented"
            );
            error::ENOSYS
        }
        _ => error::ENOSYS,
    }
}
