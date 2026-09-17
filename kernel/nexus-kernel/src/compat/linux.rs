//! Linux, translated.
//!
//! A program built for Linux makes its requests with the `syscall` instruction,
//! a call number in `rax` and arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`,
//! `r9`. So does a NexusOS program. The instruction is the same because the
//! processor has one; everything above it is different, and that difference is
//! what this file is.
//!
//! # Why this is a translation and not a personality of the kernel
//!
//! Nothing here reaches into the kernel. Every Linux call is turned into
//! something the Nexus interface already offers to any program: `write` on
//! standard output becomes the same logging operation `nexus_user::log` uses,
//! `exit_group` becomes the same exit, `getpid` reads the same process
//! identifier. There is no operation a translated Linux program can perform
//! that a Nexus program could not, and no code path below this one knows Linux
//! exists.
//!
//! That is the whole rule this system is built on: Linux compatibility is a
//! layer *above* the Nexus interface, never a fork of it. The moment a Linux
//! call needed something the Nexus interface does not have, the answer would be
//! to add it to the Nexus interface — for everybody — and then translate.
//!
//! # How a program is told apart
//!
//! It is not, and it cannot be: a static Linux executable and a NexusOS one are
//! both `ET_EXEC`, `EM_X86_64`, `ELFOSABI_SYSV` images with no interpreter.
//! Nothing in the file says which world it was built for. So the *asker* says:
//! a spawn request beginning `linux:` means the program is to be started under
//! this translation. Guessing would be worse than asking, because the two ways
//! of being wrong are "a Nexus program's first system call is read as Linux's
//! call number one" and "a Linux program's write is read as a Nexus channel
//! send", and both of those corrupt memory rather than fail.
//!
//! # What is here
//!
//! What the programs run so far actually use, and no more. Everything else
//! returns `-ENOSYS`, which is what Linux itself returns for a call a kernel
//! does not implement — a real answer that a real program is required to
//! handle, not a stub pretending to succeed.

use super::linux_display as display;
use super::linux_exec as exec;
use super::linux_files as files;
use super::linux_memory as memory;
use super::linux_poll as poll;
use super::linux_signal as signal;
use super::linux_socket as socket;
use super::linux_threads as threads;
use crate::kprintln;

/// The Linux call numbers this understands.
///
/// x86-64's numbering, which is its own: `write` is 1 here and 4 on i386, and
/// the fact that the number depends on the architecture is exactly why a
/// translation layer is per-architecture work.
mod call {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const CLOSE: u64 = 3;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const WRITEV: u64 = 20;
    pub const GETPID: u64 = 39;
    pub const EXIT: u64 = 60;
    pub const UNAME: u64 = 63;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const ARCH_PRCTL: u64 = 158;
    pub const GETTID: u64 = 186;
    pub const SET_TID_ADDRESS: u64 = 218;

    // Threads, and the lock every C library builds out of one call.
    pub const CLONE: u64 = 56;
    pub const FUTEX: u64 = 202;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const GET_ROBUST_LIST: u64 = 274;
    pub const SCHED_YIELD: u64 = 24;
    pub const SCHED_GETAFFINITY: u64 = 204;
    /// What a C library calls to find out how many processors there are, so it
    /// can size its per-processor caches.
    pub const GETCPU: u64 = 309;
    pub const EXIT_GROUP: u64 = 231;
    pub const CLOCK_GETTIME: u64 = 228;

    // Files. `open` and `stat` are the old forms and `openat` and `newfstatat`
    // the ones every current libc actually emits; both are here because a
    // statically linked binary from a few years ago emits the old ones and
    // there is no reason to make that the difference between running and not.
    pub const OPEN: u64 = 2;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSEEK: u64 = 8;
    pub const ACCESS: u64 = 21;
    pub const GETCWD: u64 = 79;
    pub const MKDIR: u64 = 83;
    pub const UNLINK: u64 = 87;
    pub const RMDIR: u64 = 84;
    pub const GETDENTS64: u64 = 217;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const UNLINKAT: u64 = 263;
    pub const NEWFSTATAT: u64 = 262;
    pub const FACCESSAT: u64 = 269;
    pub const GETRANDOM: u64 = 318;

    // Descriptors, pipes, and waiting for one to be ready. Underneath almost
    // everything: a shell redirects with `dup2`, a library talks to a
    // subprocess through a `pipe`, and every event loop blocks in one of the
    // last three.
    pub const POLL: u64 = 7;
    pub const PIPE: u64 = 22;
    pub const DUP: u64 = 32;
    pub const DUP2: u64 = 33;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const PPOLL: u64 = 271;
    pub const EPOLL_CREATE: u64 = 213;
    pub const EPOLL_CREATE1: u64 = 291;
    pub const EPOLL_CTL: u64 = 233;
    pub const EPOLL_WAIT: u64 = 232;
    pub const EPOLL_PWAIT: u64 = 281;

    // Sockets. One family, `AF_UNIX`: a connection between two programs on one
    // machine, reached by a name in the filesystem. How X11 and Wayland are
    // reached, and how nearly every desktop service on Linux is.
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const SENDTO: u64 = 44;
    pub const RECVFROM: u64 = 45;
    pub const SENDMSG: u64 = 46;
    pub const RECVMSG: u64 = 47;
    pub const SHUTDOWN: u64 = 48;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const GETPEERNAME: u64 = 52;
    pub const SOCKETPAIR: u64 = 53;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    pub const ACCEPT4: u64 = 288;

    // Signals: the one thing in this interface that runs a program's code at a
    // moment the program did not choose.
    pub const RT_SIGRETURN: u64 = 15;
    pub const KILL: u64 = 62;
    pub const TGKILL: u64 = 234;
    pub const SIGALTSTACK: u64 = 131;
    pub const RT_SIGSUSPEND: u64 = 130;
    pub const RT_SIGPENDING: u64 = 127;
    pub const RT_SIGTIMEDWAIT: u64 = 128;

    /// Become a different program. See `compat::linux_exec`.
    pub const EXECVE: u64 = 59;

    /// Memory with a descriptor on it, and the call that gives it a size.
    pub const MEMFD_CREATE: u64 = 319;
    pub const FTRUNCATE: u64 = 77;
}

/// Errors, as Linux returns them: negative, in the return register.
///
/// Not a separate error channel and not a flag — the value itself is the
/// answer, and a caller tells the two apart by whether it is in the top page of
/// the address space. That convention is why a Linux `write` can return a byte
/// count and an error in the same register.
pub(super) mod error {
    /// Function not implemented.
    pub const ENOSYS: u64 = (-38i64) as u64;
    /// Bad file descriptor.
    pub const EBADF: u64 = (-9i64) as u64;
    /// Bad address.
    pub const EFAULT: u64 = (-14i64) as u64;
    /// Out of memory.
    pub const ENOMEM: u64 = (-12i64) as u64;
    /// Not a terminal.
    pub const ENOTTY: u64 = (-25i64) as u64;
    /// An argument this call will not accept.
    pub const EINVAL: u64 = (-22i64) as u64;
}

/// `arch_prctl` subfunctions.
mod arch {
    pub const SET_FS: u64 = 0x1002;
    pub const GET_FS: u64 = 0x1003;
}

/// The most `writev` will walk.
///
/// Linux's own limit is 1024. This is smaller because every vector here is read
/// from user memory one at a time.
const MAX_IOV: u64 = 64;

/// Calls translated, and calls refused, for the monitor.
static TRANSLATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many calls have been translated, how many refused, and how many pages
/// of memory handed out.
///
/// The page count comes from [`super::linux_memory`], which is the thing that
/// hands them out, rather than from a second counter here that would have to be
/// kept in step with it.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    let (mapped, _, _) = super::linux_memory::statistics();
    (
        TRANSLATED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        mapped,
    )
}

/// One Linux system call, from a process running under the translation.
///
/// Returns what the program will find in `rax`.
pub fn dispatch(number: u64, frame: &mut crate::arch::syscall::Frame) -> u64 {
    // The arguments, read out of the frame rather than taken as six more
    // parameters. They were six parameters until `clone` needed the frame as
    // well, and a function taking both was taking the same six values twice --
    // once by name and once inside a structure that already had them.
    //
    // `r10` and not `rcx` in the fourth place, because the `syscall`
    // instruction takes `rcx` for the return address and Linux's convention
    // moves the fourth argument out of its way. That is the one place this
    // differs from an ordinary C call and it is the same on both sides of the
    // boundary.
    let argument0 = frame.rdi;
    let argument1 = frame.rsi;
    let argument2 = frame.rdx;
    let _argument3 = frame.r10;
    let _argument4 = frame.r8;
    let _argument5 = frame.r9;

    match number {
        call::WRITE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::write(argument0, argument1, argument2)
        }
        call::WRITEV => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            writev(argument0, argument1, argument2)
        }
        call::READ => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::read(argument0, argument1, argument2)
        }
        call::PREAD64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::pread64(argument0, argument1, argument2, _argument3)
        }

        // Files. Every one of these is a walk through the machine's own store,
        // reached by the same operations a Nexus program uses: see
        // `compat::linux_files`.
        call::OPENAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::openat(argument0, argument1, argument2)
        }
        // `open(path, flags)` is `openat(AT_FDCWD, path, flags)`. Written as
        // that rather than as a second implementation, because two of them
        // would be two things to keep in step.
        call::OPEN => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::openat(files::AT_FDCWD as u32 as u64, argument0, argument1)
        }
        call::LSEEK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::lseek(argument0, argument1, argument2)
        }
        call::FSTAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::fstat(argument0, argument1)
        }
        call::NEWFSTATAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::newfstatat(argument0, argument1, argument2)
        }
        call::STAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::newfstatat(files::AT_FDCWD as u32 as u64, argument0, argument1)
        }
        call::GETDENTS64 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getdents64(argument0, argument1, argument2)
        }
        call::FACCESSAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::faccessat(argument0, argument1)
        }
        call::ACCESS => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::faccessat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::GETCWD => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getcwd(argument0, argument1)
        }
        call::MKDIRAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::mkdirat(argument0, argument1)
        }
        call::MKDIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::mkdirat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::UNLINKAT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::unlinkat(argument0, argument1)
        }
        // `unlink` and `rmdir` are the same operation on a store that knows
        // which of the two a name is.
        call::UNLINK | call::RMDIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::unlinkat(files::AT_FDCWD as u32 as u64, argument0)
        }
        call::GETRANDOM => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::getrandom(argument0, argument1)
        }
        call::CLOSE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::close(argument0)
        }

        // Descriptors. A duplicate is a second handle to the same object, and
        // `dup2` is the same with the caller choosing the number -- which is
        // the whole reason a shell can redirect.
        call::DUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::dup(argument0)
        }
        call::DUP2 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::dup2(argument0, argument1)
        }
        // `dup3` is `dup2` that refuses to duplicate a descriptor onto itself
        // and takes `O_CLOEXEC`. The flag has nothing to change here yet, and
        // the refusal is the caller's own check, so the two are one call.
        call::DUP3 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if argument0 == argument1 {
                error::EINVAL
            } else {
                files::dup2(argument0, argument1)
            }
        }
        // `pipe` is `pipe2` with no flags, written as that rather than as a
        // second implementation.
        call::PIPE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::pipe2(argument0, 0)
        }
        call::PIPE2 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::pipe2(argument0, argument1)
        }

        // Sockets.
        call::SOCKET => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::socket(argument0, argument1, argument2)
        }
        call::SOCKETPAIR => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::socketpair(argument0, argument1, argument2, _argument3)
        }
        call::BIND => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::bind(argument0, argument1, argument2)
        }
        call::LISTEN => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::listen(argument0, argument1)
        }
        call::CONNECT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::connect(argument0, argument1, argument2)
        }
        // `accept` is `accept4` with no flags, written as that rather than as
        // a second implementation.
        call::ACCEPT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::accept(argument0, argument1, argument2, 0)
        }
        call::ACCEPT4 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::accept(argument0, argument1, argument2, _argument3)
        }
        // A connected socket has no address to send to, so `sendto`'s last two
        // arguments are ignored -- which is what Linux does with them.
        call::SENDTO => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::send(argument0, argument1, argument2, _argument3)
        }
        call::RECVFROM => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::receive(argument0, argument1, argument2, _argument3)
        }
        call::SENDMSG => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::sendmsg(argument0, argument1, argument2)
        }
        call::RECVMSG => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::recvmsg(argument0, argument1, argument2)
        }
        call::SHUTDOWN => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::shutdown(argument0, argument1)
        }
        call::SETSOCKOPT | call::GETSOCKOPT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            socket::setsockopt(argument0, argument1, argument2)
        }
        // Which address a socket is at, and which its peer is. A connection
        // made through a name has no address of its own here, so both write
        // the family and nothing else -- which is what Linux writes for an
        // unnamed socket.
        call::GETSOCKNAME | call::GETPEERNAME => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            unnamed_address(argument1, argument2)
        }

        // Waiting for a descriptor. `ppoll` differs by taking a `timespec`
        // instead of milliseconds and a signal mask this system has nothing to
        // do with; the mask is ignored, which is what a kernel with no signals
        // can honestly do with one.
        call::POLL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll::poll(argument0, argument1, argument2 as i64)
        }
        call::PPOLL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let timeout = timespec_ms(argument2);
            poll::poll(argument0, argument1, timeout)
        }
        // `epoll_create` took a size hint that Linux stopped using in 2.6.8.
        // Accepted and ignored, which is what Linux does with it.
        call::EPOLL_CREATE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll::create(0)
        }
        call::EPOLL_CREATE1 => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll::create(argument0)
        }
        call::EPOLL_CTL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll::control(argument0, argument1, argument2, _argument3)
        }
        // `epoll_pwait` is `epoll_wait` with a signal mask, as `ppoll` is to
        // `poll`, and the mask is ignored for the same reason.
        call::EPOLL_WAIT | call::EPOLL_PWAIT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            poll::wait(argument0, argument1, argument2, _argument3 as i64)
        }
        // Memory. Six arguments, and the sixth is the file offset -- a
        // translation that could only see five would map every library from
        // the start of its file.
        call::MMAP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            memory::mmap(
                argument0, argument1, argument2, _argument3, _argument4, _argument5,
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

        // Threads. `clone` is the one call here that needs the caller's whole
        // register state, because a new thread begins with a copy of it: see
        // `crate::user::resume`.
        //
        // The argument order is x86-64's and not the manual page's: `tls` is
        // the fifth register and `child_tid` the fourth. Swapping them gives a
        // thread whose thread pointer is an address the library meant as
        // somewhere to write a number.
        call::CLONE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            threads::clone(
                argument0, argument1, argument2, _argument3, _argument4, frame,
            )
        }
        call::FUTEX => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            threads::futex(argument0, argument1, argument2, _argument3)
        }
        call::SET_ROBUST_LIST => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            threads::set_robust_list(argument0, argument1)
        }
        call::GET_ROBUST_LIST => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            threads::get_robust_list(argument1, argument2)
        }
        // Giving up the rest of a slice is the same operation in both worlds.
        call::SCHED_YIELD => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::sched::yield_now();
            0
        }
        // How many processors a program may run on. A bitmask, and every
        // processor this machine has is in it: the scheduler places threads
        // itself and this layer has no way to pin one, so a program told
        // otherwise would be told something untrue.
        call::SCHED_GETAFFINITY => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            sched_getaffinity(argument1, argument2)
        }
        // Which processor a thread is on. Answered, because a C library uses it
        // to pick a per-processor cache and any answer is a correct one --
        // the thread may move between the call and the use, which is true on
        // Linux too and is why the answer is a hint there as well.
        call::GETCPU => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            getcpu(argument0, argument1)
        }
        call::ARCH_PRCTL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            arch_prctl(argument0, argument1)
        }
        call::UNAME => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            uname(argument0)
        }
        call::CLOCK_GETTIME => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            clock_gettime(argument0, argument1)
        }
        // `brk` is refused, deliberately and with the error Linux uses when it
        // cannot grow the break. Every libc worth running falls back to `mmap`
        // for its heap when this fails -- musl does not use `brk` at all -- and
        // a second allocator here would be a second thing to get wrong for no
        // program that needs it.
        call::BRK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            error::ENOMEM
        }
        // Memory with a descriptor on it: a file that is on no filesystem,
        // made to be passed to another program over a socket. How every
        // graphics protocol on Linux moves a frame.
        call::MEMFD_CREATE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::memfd_create(argument0, argument1)
        }
        call::FTRUNCATE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            files::ftruncate(argument0, argument1)
        }

        // Become a different program. Does not return on success: the frame is
        // rewritten so the way out of this call restores the *new* program's
        // registers, which is the same mechanism a signal handler is entered
        // through.
        call::EXECVE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            exec::execve(argument0, argument1, argument2, frame)
        }

        // Signals. `rt_sigaction` used to be refused, and that was right while
        // there was no delivery: accepting it would have promised a handler
        // that never runs. It can be kept now -- see `compat::linux_signal`.
        call::RT_SIGACTION => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::sigaction(argument0, argument1, argument2, _argument3)
        }
        call::RT_SIGPROCMASK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::sigprocmask(argument0, argument1, argument2, _argument3)
        }
        // Not a call that returns: it puts the program back where it was before
        // the handler, which means replacing the frame this call is about to
        // return through.
        call::RT_SIGRETURN => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // The one call that writes the frame rather than reading it. The
            // stub restores the registers from it on the way out, which is
            // exactly what makes putting the program back where it was work.
            signal::sigreturn(frame)
        }
        call::KILL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::kill(argument0, argument1)
        }
        // `tgkill(tgid, tid, signal)`: the thread group is the process here,
        // and a signal to one thread of it is a signal to it.
        call::TGKILL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::kill(argument0, argument2)
        }
        // The rest of the family, refused by name so the log says which one a
        // program wanted rather than a number.
        call::SIGALTSTACK => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::not_translated("sigaltstack")
        }
        call::RT_SIGSUSPEND => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::not_translated("rt_sigsuspend")
        }
        call::RT_SIGPENDING => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::not_translated("rt_sigpending")
        }
        call::RT_SIGTIMEDWAIT => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            signal::not_translated("rt_sigtimedwait")
        }
        call::SET_TID_ADDRESS | call::GETTID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::arch::percpu::current_thread()
        }
        // One machine, one user, and that user is the one who turned it on.
        // Zero is root's identifier and this machine has no others -- said here
        // rather than pretended: there is no user model underneath.
        call::GETUID | call::GETGID | call::GETEUID | call::GETEGID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            0
        }
        // A device, or a question about a terminal.
        //
        // The one device this layer has is the window, and a descriptor that
        // names one is answered by it. Everything else gets `ENOTTY`, which is
        // what `isatty` is asking and what the honest answer is: standard
        // output goes to the boot log, which is a serial line and not a
        // terminal, and a program told otherwise would start emitting escape
        // sequences at it.
        call::IOCTL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let window = crate::sched::current_process()
                .is_some_and(|process| display::is_window(process.id.0, argument0));
            if window {
                display::ioctl(argument0, argument1, argument2)
            } else {
                error::ENOTTY
            }
        }
        call::GETPID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::sched::current_process().map_or(error::ENOSYS, |process| process.id.0)
        }
        // Two calls that were one thing while a process had one thread, and
        // are not one thing any more.
        //
        // `exit` ends the calling *thread*. A C library calls it at the end of
        // every thread it made, and a program whose second thread finished by
        // taking the whole process down with it would be a program that runs
        // once.
        //
        // `exit_group` ends the program. Every other thread is asked to stop
        // and the caller records the status -- asked rather than stopped,
        // because a thread is the only thing that knows what it is holding and
        // so is the only thing that can safely put it down. That is the same
        // rule `kill` follows for a Nexus process.
        call::EXIT => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            exit_thread(argument0)
        }
        call::EXIT_GROUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            exit(argument0)
        }
        // Before giving up: `linux_more` holds the calls a C library asks for
        // before it will start. They are in a file of their own rather than in
        // this table because there are two dozen of them and this file is
        // worked on by more than one person.
        _ => match super::linux_more::translate(number, frame) {
            Some(answer) => {
                TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                answer
            }
            None => {
                REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                // Named in the log, because the useful thing about running a
                // foreign program is finding out what it asks for. A
                // translation layer grows by reading these.
                kprintln!("[linux] call {number} is not translated yet; answering ENOSYS");
                error::ENOSYS
            }
        },
    }
}

/// `writev(fd, iov, count)`.
///
/// The call a real libc's `printf` makes. musl and glibc both build the format
/// result as a list of pieces and hand the list over rather than copying it
/// flat first, so a translation layer with `write` and not this one runs a
/// program that prints nothing and reports success.
fn writev(descriptor: u64, vectors: u64, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    if count > MAX_IOV {
        return error::EINVAL;
    }

    // `struct iovec { void *base; size_t len; }` -- two words, and the layout
    // is part of the ABI rather than something this chooses.
    const IOVEC: u64 = 16;
    let Some((base, length)) = crate::arch::syscall::user_range(
        vectors,
        count.saturating_mul(IOVEC),
        MAX_IOV.saturating_mul(IOVEC),
    ) else {
        return error::EFAULT;
    };

    // SAFETY: the range was checked to lie wholly inside the user half, which
    // this thread's address space maps, and is exactly `count` iovecs long.
    let raw = unsafe { core::slice::from_raw_parts(base as *const u8, length) };

    // Each piece written in turn, through the same `write` a single one goes
    // through -- so a `writev` to a pipe reaches the pipe, and one to the log
    // reaches the log. It used to assume the descriptor was the log, which was
    // true for exactly as long as there was nothing else a descriptor could be.
    let mut written = 0u64;
    for index in 0..count as usize {
        let at = index * IOVEC as usize;
        let pointer = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap_or_default());
        let wanted = u64::from_le_bytes(raw[at + 8..at + 16].try_into().unwrap_or_default());
        if wanted == 0 {
            continue;
        }
        let taken = files::write(descriptor, pointer, wanted);
        // A negative answer is an error. Reported only if nothing has been
        // written yet: once some of it has gone, the count is what the caller
        // needs, and a program that was told an error after a partial write
        // would send those bytes twice.
        if (taken as i64) < 0 {
            return if written == 0 { taken } else { written };
        }
        written += taken;
        if taken < wanted {
            // A short write ends the gather: the pieces are one stream, and
            // skipping the rest of this piece to start the next would reorder
            // the caller's bytes.
            break;
        }
    }
    written
}

/// `arch_prctl(code, address)`.
///
/// `ARCH_SET_FS` is what makes thread-local storage work, and thread-local
/// storage is what a libc sets up before it runs `main`. Without this a static
/// binary faults on its first `errno`.
fn arch_prctl(code: u64, address: u64) -> u64 {
    /// `IA32_FS_BASE`.
    const FS_BASE: u32 = 0xC000_0100;

    match code {
        arch::SET_FS => {
            if address >= nexus_abi::layout::USER_SPACE_END {
                return error::EFAULT;
            }
            // SAFETY: writing this processor's `FS_BASE`, which is per-thread
            // state that the scheduler saves and restores with the thread. The
            // value was checked to be a user address, so a program cannot point
            // its own `fs` at the kernel.
            unsafe { crate::arch::syscall::write_msr(FS_BASE, address) };
            0
        }
        arch::GET_FS => {
            // SAFETY: reading a model-specific register.
            let value = unsafe { crate::arch::syscall::read_msr(FS_BASE) };
            let Some((pointer, _)) = crate::arch::syscall::user_range(address, 8, 8) else {
                return error::EFAULT;
            };
            // SAFETY: the range is eight bytes inside the user half.
            unsafe { core::ptr::write_unaligned(pointer as *mut u64, value) };
            0
        }
        _ => error::EINVAL,
    }
}

/// `uname(buffer)`.
///
/// `struct utsname` is six fixed 65-byte fields. A program reads the release
/// string to decide what the kernel can do, so what goes in it matters: this
/// says `NexusOS`, not a Linux version number. A program that refuses to run
/// because it does not recognise the system is a program refusing honestly, and
/// is much better than one that runs believing it is on Linux 6.
fn uname(buffer: u64) -> u64 {
    uname_into(buffer)
}

/// The same, reachable from the thirty-two bit table.
///
/// `struct utsname` is six fixed sixty-five byte fields whatever the width of
/// the program, which makes it the one structure both tables can share.
pub(super) fn uname_into(buffer: u64) -> u64 {
    const FIELD: usize = 65;
    const FIELDS: usize = 6;
    let Some((pointer, length)) =
        crate::arch::syscall::user_range(buffer, (FIELD * FIELDS) as u64, (FIELD * FIELDS) as u64)
    else {
        return error::EFAULT;
    };

    let mut out = [0u8; FIELD * FIELDS];
    for (index, value) in [
        "NexusOS",
        "nexus",
        // Deliberately not a Linux version. See above.
        "NexusOS-compat",
        env!("CARGO_PKG_VERSION"),
        "x86_64",
        "(none)",
    ]
    .iter()
    .enumerate()
    {
        let bytes = value.as_bytes();
        let take = bytes.len().min(FIELD - 1);
        out[index * FIELD..index * FIELD + take].copy_from_slice(&bytes[..take]);
    }

    // SAFETY: the range was checked to lie inside the user half and is exactly
    // as long as what is written into it.
    unsafe {
        core::ptr::copy_nonoverlapping(out.as_ptr(), pointer as *mut u8, length);
    }
    0
}

/// `clock_gettime(which, buffer)`.
///
/// `struct timespec { i64 seconds; i64 nanoseconds; }`.
///
/// The realtime clock comes from the machine's own, and the monotonic one from
/// uptime -- which is what each of them means. A machine with no clock gets
/// uptime for both, because a wall clock that starts at 1970 every boot is
/// worse than one a program can tell is not a wall clock.
fn clock_gettime(which: u64, buffer: u64) -> u64 {
    /// `CLOCK_REALTIME` and `CLOCK_MONOTONIC`.
    const REALTIME: u64 = 0;
    const MONOTONIC: u64 = 1;

    let seconds: i64 = match which {
        REALTIME => match crate::drivers::rtc::now() {
            Some(now) => now as i64,
            None => (crate::arch::time::uptime_ms() / 1000) as i64,
        },
        MONOTONIC => (crate::arch::time::uptime_ms() / 1000) as i64,
        _ => return error::EINVAL,
    };
    let nanoseconds: i64 = if which == MONOTONIC {
        ((crate::arch::time::uptime_ms() % 1000) * 1_000_000) as i64
    } else {
        0
    };

    let Some((pointer, _)) = crate::arch::syscall::user_range(buffer, 16, 16) else {
        return error::EFAULT;
    };
    // SAFETY: sixteen bytes inside the user half, written as the two words a
    // `timespec` is.
    unsafe {
        core::ptr::write_unaligned(pointer as *mut i64, seconds);
        core::ptr::write_unaligned((pointer + 8) as *mut i64, nanoseconds);
    }
    0
}

/// Write the address of a socket that has not got one.
///
/// Two bytes of family and a length of two. What Linux writes for an unnamed
/// socket, and what every connection accepted through a name is here.
fn unnamed_address(address: u64, length: u64) -> u64 {
    /// `AF_UNIX`.
    const UNIX: u16 = 1;
    if address == 0 || length == 0 {
        return error::EINVAL;
    }
    let Some((at, _)) = crate::arch::syscall::user_range(length, 4, 4) else {
        return error::EFAULT;
    };
    // SAFETY: four bytes inside the user half, read as the `socklen_t` it is.
    let room = unsafe { core::ptr::read_unaligned(at as *const u32) };
    if room >= 2 {
        let Some((into, _)) = crate::arch::syscall::user_range(address, 2, 110) else {
            return error::EFAULT;
        };
        // SAFETY: two bytes inside the user half.
        unsafe { core::ptr::write_unaligned(into as *mut u16, UNIX) };
    }
    // SAFETY: as above.
    unsafe { core::ptr::write_unaligned(at as *mut u32, 2) };
    0
}

/// A `struct timespec` read as a count of milliseconds, for `ppoll`.
///
/// A null pointer means "wait for ever", which `poll` spells as a negative
/// timeout -- so the two are not the same value and the translation is here
/// rather than at the call site.
fn timespec_ms(timespec: u64) -> i64 {
    if timespec == 0 {
        return -1;
    }
    let Some((pointer, _)) = crate::arch::syscall::user_range(timespec, 16, 16) else {
        // A bad pointer cannot be reported from here without changing what
        // this returns, so it becomes "do not wait" -- and the call that
        // follows returns whatever is ready now, which is the safe reading.
        return 0;
    };
    // SAFETY: sixteen bytes inside the user half, read as the two words a
    // `timespec` is.
    let (seconds, nanoseconds) = unsafe {
        (
            core::ptr::read_unaligned(pointer as *const i64),
            core::ptr::read_unaligned((pointer + 8) as *const i64),
        )
    };
    seconds
        .saturating_mul(1000)
        .saturating_add(nanoseconds / 1_000_000)
}

/// The same exit, reachable from the thirty-two bit table.
pub(super) fn exit_now(status: u64) -> ! {
    exit(status)
}

/// `exit_group(status)`: the whole program stops.
///
/// Every other thread is asked to stop, and the caller records the status and
/// goes. Asked rather than stopped: a thread is the only thing that knows what
/// it is holding, so it is the only thing that can put it down -- which is why
/// `stop_if_asked` exists and is consulted at every point a thread can block or
/// return to ring 3.
fn exit(status: u64) -> ! {
    if let Some(process) = crate::sched::current_process() {
        // The flag first, then the wakes: a thread woken before the flag is set
        // looks at it, finds nothing, and goes back to sleep.
        process.completion.cancel();
        let others = threads::siblings(process.id.0).len();
        if others > 0 {
            crate::sched::wake_process_threads(process.id);
        }
        process.completion.finish(status);
        kprintln!(
            "[linux] process {} \"{}\" exited with status {status} through the Linux boundary\
             {}",
            process.id,
            process.name.as_str(),
            if others == 0 {
                alloc::string::String::new()
            } else {
                alloc::format!(", asking {others} other thread(s) to stop")
            }
        );
    }
    crate::arch::interrupts::disable();
    crate::sched::exit()
}

/// `exit(status)`: this thread stops, and the program carries on.
///
/// The last thread out is the program ending, and is the case that used to be
/// the only one. Every other thread is one a C library made and is ending
/// normally, and what it owes the rest of the program is the word
/// `CLONE_CHILD_CLEARTID` named -- cleared, and anything waiting on it woken.
/// That is what makes a `pthread_join` return, and without it a program that
/// joins a thread does not continue.
fn exit_thread(status: u64) -> ! {
    let here = crate::arch::percpu::current_thread();
    let last = crate::sched::current_process().is_none_or(|process| {
        // The word the library is waiting on, cleared while this thread can
        // still reach its own address space.
        threads::ending(here);
        threads::forget_thread(process.id.0, here);
        let remaining = threads::siblings(process.id.0).len();
        if remaining == 0 {
            true
        } else {
            kprintln!(
                "[linux] a thread of process {} \"{}\" exited with status {status}; \
                 {remaining} left",
                process.id,
                process.name.as_str()
            );
            false
        }
    });

    if last {
        // The only thread there was, so this is the program ending -- which is
        // what `exit` meant before there could be more than one.
        exit(status)
    } else {
        crate::arch::interrupts::disable();
        crate::sched::exit()
    }
}

/// `sched_getaffinity(pid, size, mask)`.
///
/// A bitmask with one bit per processor, of which every one this machine has is
/// set. The scheduler places threads itself and this layer cannot pin one, so a
/// program told it may only run on some would be told something untrue -- and
/// what a program does with that is size its thread pool to a number that is
/// not this machine's.
fn sched_getaffinity(size: u64, mask: u64) -> u64 {
    let processors = crate::arch::smp::processor_count().clamp(1, 64);
    // Linux wants at least enough room for the mask it would write, and
    // refuses rather than truncating. Eight bytes is sixty-four processors,
    // which is more than this kernel starts.
    if size < 8 {
        return error::EINVAL;
    }
    let Some((pointer, _)) = crate::arch::syscall::user_range(mask, 8, 8) else {
        return error::EFAULT;
    };
    let bits: u64 = if processors >= 64 {
        u64::MAX
    } else {
        (1u64 << processors) - 1
    };
    // SAFETY: eight bytes inside the user half, checked above.
    unsafe { core::ptr::write_unaligned(pointer as *mut u64, bits) };
    // The number of bytes written, which is what Linux returns.
    8
}

/// `getcpu(cpu, node)`.
///
/// Which processor this thread is on at this instant. A hint, here and on
/// Linux: the thread may be moved between this call and whatever it does with
/// the answer, which is why every correct use of it is one where being wrong
/// costs a cache miss rather than a wrong result.
fn getcpu(processor: u64, node: u64) -> u64 {
    let here = crate::arch::percpu::cpu_index();
    if processor != 0 {
        let Some((pointer, _)) = crate::arch::syscall::user_range(processor, 4, 4) else {
            return error::EFAULT;
        };
        // SAFETY: four bytes inside the user half.
        unsafe { core::ptr::write_volatile(pointer as *mut u32, here) };
    }
    if node != 0 {
        let Some((pointer, _)) = crate::arch::syscall::user_range(node, 4, 4) else {
            return error::EFAULT;
        };
        // One node: this kernel does not model memory affinity, and saying so
        // is better than inventing a topology a program would then optimise
        // for.
        // SAFETY: four bytes inside the user half.
        unsafe { core::ptr::write_volatile(pointer as *mut u32, 0) };
    }
    0
}
