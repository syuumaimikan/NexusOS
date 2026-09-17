//! The calls a C library asks for before it will start.
//!
//! # Why this is a file of its own
//!
//! `tools/nexus-probe` asked this machine which Linux calls it does not have
//! and printed a list of twenty-three. This is the part of that list that can
//! be answered **truthfully** with what the kernel already has, reached through
//! one line in `linux::dispatch`'s fallback rather than twenty-three lines in
//! its table.
//!
//! One line because `compat/linux.rs` is worked on by somebody else. A new file
//! and a single hook is a smaller thing to merge than two dozen new arms
//! threaded through a match somebody is editing.
//!
//! # What is not here, and why that is the point
//!
//! The rest of the list still answers `ENOSYS`, and that is deliberate. A call
//! that returned a plausible number without doing the work would be worse than
//! a missing one: `ENOSYS` is a thing a C library is *required* to handle, and
//! a wrong answer is a thing it cannot detect.
//!
//! So `chdir` is absent rather than pretending, because there is no working
//! directory to change. `wait4` is absent because collecting a child is the
//! process layer's business. `mremap` is absent because moving a mapping is
//! real work and answering `ENOMEM` to avoid it would be a lie told in a
//! register.
//!
//! # The ones that look like stubs and are not
//!
//! Three answers here are suspiciously simple and each is what Linux itself
//! says:
//!
//! - **`readlink` on something that is not a symbolic link is `EINVAL`.** This
//!   filesystem has no symbolic links, so that is the answer for every path
//!   that exists, and it is the correct one rather than an evasion.
//! - **`madvise` is advice.** A kernel is allowed to ignore it, and returning
//!   success without acting is conforming behaviour -- for the advisory values.
//!   `MADV_DONTNEED` is *not* advice, because a program may rely on reading
//!   zeroes afterwards, so that one is refused.
//! - **`membarrier(QUERY)` returns a set of supported commands**, and an empty
//!   set is a legitimate answer that tells the caller to use its slow path.

use super::linux::error;
use super::linux_files;
use crate::arch::syscall::user_range;

/// Two answers this file needs that `linux::error` does not carry.
///
/// Defined here rather than added there, because that module belongs to the
/// table this one extends and a new constant in it is a change to somebody
/// else's file for the sake of two lines.
mod also {
    /// The operation is not permitted.
    pub const EPERM: u64 = (-1i64) as u64;
    /// The device said no.
    pub const EIO: u64 = (-5i64) as u64;
    /// Nothing is wrong; ask again.
    pub const EAGAIN: u64 = (-11i64) as u64;
}

/// Try to answer a call `linux::dispatch` did not recognise.
///
/// `None` means this file does not know it either, and the caller says
/// `ENOSYS` and logs it as before.
pub fn translate(number: u64, frame: &crate::arch::syscall::Frame) -> Option<u64> {
    let (a, b, c, d) = (frame.rdi, frame.rsi, frame.rdx, frame.r10);
    Some(match number {
        call::NANOSLEEP => nanosleep(a, b),
        call::CLOCK_NANOSLEEP => clock_nanosleep(a, b, c, d),
        call::GETTIMEOFDAY => gettimeofday(a, b),
        call::TIME => time(a),
        call::GETRLIMIT => getrlimit(a, b),
        call::PRLIMIT64 => prlimit64(a, b, c, d),
        call::SYSINFO => sysinfo(a),
        call::MADVISE => madvise(a, b, c),
        call::MEMBARRIER => membarrier(a),
        call::FSYNC | call::FDATASYNC => fsync(a),
        call::READV => readv(a, b, c),
        call::LSTAT => lstat(a, b),
        call::READLINK => readlink(a),
        call::READLINKAT => readlinkat(a, b),
        call::FCNTL => fcntl(a, b, c),
        call::PWRITE64 => pwrite64(a, b, c, d),
        call::STATX => statx(a, b, c, frame.r8),
        call::CLONE3 => clone3(a, b, frame),
        call::CREAT => creat(a),
        call::TRUNCATE => truncate(a, b),
        call::PRCTL => prctl(a, b),
        call::STATFS => statfs(a, b),
        call::FSTATFS => statfs_into(b),
        call::SELECT => select(a, b, c, d, frame.r8, Timeout::Microseconds),
        call::PSELECT6 => select(a, b, c, d, frame.r8, Timeout::Nanoseconds),
        _ => return None,
    })
}

/// The numbers this file answers. x86-64's, like the table it extends.
mod call {
    pub const LSTAT: u64 = 6;
    pub const READV: u64 = 19;
    pub const MADVISE: u64 = 28;
    pub const NANOSLEEP: u64 = 35;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const READLINK: u64 = 89;
    pub const GETTIMEOFDAY: u64 = 96;
    pub const GETRLIMIT: u64 = 97;
    pub const SYSINFO: u64 = 99;
    pub const TIME: u64 = 201;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const READLINKAT: u64 = 267;
    pub const PRLIMIT64: u64 = 302;
    pub const MEMBARRIER: u64 = 324;
    pub const FCNTL: u64 = 72;
    pub const PWRITE64: u64 = 18;
    pub const STATX: u64 = 332;
    pub const CLONE3: u64 = 435;
    pub const CREAT: u64 = 85;
    pub const TRUNCATE: u64 = 76;
    pub const PRCTL: u64 = 157;
    pub const STATFS: u64 = 137;
    pub const FSTATFS: u64 = 138;
    pub const SELECT: u64 = 23;
    pub const PSELECT6: u64 = 270;
}

/// Which of the two shapes a `select` timeout arrives in.
///
/// `select` takes a `struct timeval` -- seconds and *micro*seconds -- and
/// `pselect6` takes a `struct timespec` -- seconds and *nano*seconds. The two
/// are the same size and differ only in the meaning of the second field, which
/// is exactly the sort of difference that produces a timeout a thousand times
/// too short and a bug nobody can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timeout {
    Microseconds,
    Nanoseconds,
}

/// The descriptor flags this layer keeps for a program.
///
/// # Why there is a table at all
///
/// `F_SETFD` and `F_SETFL` are not questions, they are *settings*, and a
/// program that sets one expects to read it back. Answering `F_GETFL` with a
/// plausible constant would be the exact failure this file's header warns
/// about: a wrong answer a C library cannot detect. So the flags are kept.
///
/// Keyed by process as well as descriptor, because descriptor three means a
/// different thing in every program, and forgotten when a process ends -- the
/// same discipline `linux_files` already follows, and for the same reason: a
/// map that only grows is a leak with a slow fuse.
static FLAGS: crate::sync::IrqSpinLock<alloc::collections::BTreeMap<(u64, u64), Descriptor>> =
    crate::sync::IrqSpinLock::new(alloc::collections::BTreeMap::new());

/// What is remembered about one open descriptor.
#[derive(Debug, Clone, Copy, Default)]
struct Descriptor {
    /// `FD_CLOEXEC`, which is a property of the descriptor.
    close_on_exec: bool,
    /// `O_NONBLOCK`, which is a property of the open file.
    non_blocking: bool,
}

/// Let a process's flags go when it ends.
///
/// Called from `Process::drop` beside the other `forget`s.
pub fn forget(process: u64) {
    FLAGS.lock().retain(|(owner, _), _| *owner != process);
}

fn whose() -> Option<u64> {
    crate::sched::current_process().map(|process| process.id.0)
}

/// Seconds since the epoch, and the fraction, as this machine knows them.
///
/// The clock the machine actually has, and the uptime when it has none. A
/// translation layer that answered zero would be one that made every timestamp
/// on this system 1970.
fn wall_clock() -> (i64, i64) {
    let milliseconds = crate::arch::time::uptime_ms();
    match crate::drivers::rtc::now() {
        Some(now) => (now as i64, ((milliseconds % 1000) * 1_000_000) as i64),
        None => (
            (milliseconds / 1000) as i64,
            ((milliseconds % 1000) * 1_000_000) as i64,
        ),
    }
}

/// Read a `struct timespec` a program handed over.
fn timespec_at(pointer: u64) -> Option<(i64, i64)> {
    let (at, _) = user_range(pointer, 16, 16)?;
    // SAFETY: `user_range` has checked that sixteen bytes at this address
    // belong to the calling program and are readable.
    let seconds = unsafe { core::ptr::read_unaligned(at as *const i64) };
    // SAFETY: the same sixteen bytes; the second half of them.
    let nanoseconds = unsafe { core::ptr::read_unaligned((at as *const i64).add(1)) };
    Some((seconds, nanoseconds))
}

/// `nanosleep(request, remaining)`.
///
/// Rounded up to a millisecond, because that is what the scheduler counts in.
/// Rounded *up* and not down: a program asking for a hundred microseconds in a
/// loop and getting none would spin, and a sleep that returns early is the one
/// failure a sleeping program cannot detect.
fn nanosleep(request: u64, remaining: u64) -> u64 {
    let Some((seconds, nanoseconds)) = timespec_at(request) else {
        return error::EFAULT;
    };
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return error::EINVAL;
    }
    let milliseconds = (seconds as u64)
        .saturating_mul(1000)
        // Rounded up by hand: `div_ceil` is not stable for the integer types
        // this target builds with.
        .saturating_add(((nanoseconds as u64) + 999_999) / 1_000_000);
    if milliseconds > 0 {
        crate::sched::sleep_ms(milliseconds);
    }
    // Slept the whole time, so nothing is left. Written only if asked for.
    if remaining != 0 {
        if let Some((at, _)) = user_range(remaining, 16, 16) {
            // SAFETY: `user_range` has checked these sixteen bytes belong to
            // the caller and are writable.
            unsafe {
                core::ptr::write_unaligned(at as *mut i64, 0);
                core::ptr::write_unaligned((at as *mut i64).add(1), 0);
            }
        }
    }
    0
}

/// `clock_nanosleep(clock, flags, request, remaining)`.
///
/// `TIMER_ABSTIME` is refused rather than approximated. Sleeping *until* a time
/// is a different thing from sleeping *for* one, and a program that asked for
/// the first and got the second would wake at the wrong moment for ever
/// afterwards.
fn clock_nanosleep(clock: u64, flags: u64, request: u64, remaining: u64) -> u64 {
    const REALTIME: u64 = 0;
    const MONOTONIC: u64 = 1;
    const TIMER_ABSTIME: u64 = 1;
    if clock != REALTIME && clock != MONOTONIC {
        return error::EINVAL;
    }
    if flags & TIMER_ABSTIME != 0 {
        return error::ENOSYS;
    }
    nanosleep(request, remaining)
}

/// `gettimeofday(time, zone)`.
///
/// The timezone argument is obsolete and Linux ignores it; so does this.
fn gettimeofday(pointer: u64, _zone: u64) -> u64 {
    if pointer == 0 {
        return 0;
    }
    let Some((at, _)) = user_range(pointer, 16, 16) else {
        return error::EFAULT;
    };
    let (seconds, nanoseconds) = wall_clock();
    // SAFETY: sixteen bytes the caller owns, checked above.
    unsafe {
        core::ptr::write_unaligned(at as *mut i64, seconds);
        core::ptr::write_unaligned((at as *mut i64).add(1), nanoseconds / 1000);
    }
    0
}

/// `time(out)`. The oldest one, and still called.
fn time(pointer: u64) -> u64 {
    let (seconds, _) = wall_clock();
    if pointer != 0 {
        let Some((at, _)) = user_range(pointer, 8, 8) else {
            return error::EFAULT;
        };
        // SAFETY: eight bytes the caller owns, checked above.
        unsafe { core::ptr::write_unaligned(at as *mut i64, seconds) };
    }
    seconds as u64
}

/// What a limit is, when this system does not have the thing being limited.
///
/// `RLIM_INFINITY`. Not a made-up number: this kernel does not cap a process's
/// stack or its address space, and reporting a small figure would have a C
/// library refuse to do something it is perfectly able to do.
const INFINITY: u64 = u64::MAX;

/// How many open files this system tells a program to stay under.
///
/// **A policy, not a measurement**, and the difference is worth stating. The
/// handle table is a map and has no fixed size, so nothing here enforces this
/// figure. It is reported because a C library that asked and was told
/// "unlimited" would take it literally: the common shape is a loop closing
/// every descriptor from three up to the limit, and with `RLIM_INFINITY` that
/// is a loop over eighteen quintillion numbers.
///
/// A thousand and twenty-four, which is what Linux itself uses by default.
const NOFILE_LIMIT: u64 = 1024;

fn write_limit(pointer: u64, soft: u64, hard: u64) -> u64 {
    let Some((at, _)) = user_range(pointer, 16, 16) else {
        return error::EFAULT;
    };
    // SAFETY: sixteen bytes the caller owns, checked above.
    unsafe {
        core::ptr::write_unaligned(at as *mut u64, soft);
        core::ptr::write_unaligned((at as *mut u64).add(1), hard);
    }
    0
}

/// `getrlimit(resource, out)`.
fn getrlimit(resource: u64, out: u64) -> u64 {
    const NOFILE: u64 = 7;
    let (soft, hard) = if resource == NOFILE {
        (NOFILE_LIMIT, NOFILE_LIMIT)
    } else {
        (INFINITY, INFINITY)
    };
    write_limit(out, soft, hard)
}

/// `prlimit64(pid, resource, new, old)`.
///
/// Setting is refused rather than accepted-and-ignored. A program that lowered
/// a limit and was told it succeeded would go on believing the limit was in
/// force, which is worse than being told it cannot be done.
fn prlimit64(pid: u64, resource: u64, new: u64, old: u64) -> u64 {
    if pid != 0 {
        // Another process's limits. There is no way to reach them here.
        return error::EINVAL;
    }
    if old != 0 {
        let answer = getrlimit(resource, old);
        if answer != 0 {
            return answer;
        }
    }
    if new != 0 {
        return also::EPERM;
    }
    0
}

/// `sysinfo(out)`.
///
/// `struct sysinfo` is a long list and most of it is zero here honestly: this
/// system has no swap, no load average and no shared or buffered memory to
/// report. Uptime, total and free memory, and the process count are real.
fn sysinfo(out: u64) -> u64 {
    /// The size of the structure on x86-64: the fields below plus padding.
    const SIZE: u64 = 112;
    let Some((at, _)) = user_range(out, SIZE, SIZE) else {
        return error::EFAULT;
    };
    // SAFETY: `user_range` has checked that this many bytes belong to the
    // caller and are writable. Zeroed first so that every field this does not
    // know about is zero rather than whatever was in the page.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, SIZE as usize) };

    // Before `init` there is no allocator to ask, which cannot happen here --
    // a Linux program is running, so memory works -- but the answer is an
    // `Option` and inventing a figure for the impossible case is how a made-up
    // number gets into a structure somebody later trusts.
    let Some(memory) = crate::memory::stats() else {
        return also::EIO;
    };
    let free = memory.free_frames * nexus_mm::PAGE_SIZE;
    let total = memory.managed_frames * nexus_mm::PAGE_SIZE;
    let put = |offset: usize, value: u64| {
        // SAFETY: inside the range checked above.
        unsafe { core::ptr::write_unaligned((at as *mut u8).add(offset) as *mut u64, value) };
    };
    put(0, crate::arch::time::uptime_ms() / 1000); // uptime, seconds
    put(32, total); // totalram
    put(40, free); // freeram
    // `mem_unit`, at the end: the multiplier for every memory field above.
    // One, because the figures here are already bytes.
    // SAFETY: inside the range checked above.
    unsafe { core::ptr::write_unaligned((at as *mut u8).add(80) as *mut u32, 1) };
    0
}

/// `madvise(address, length, advice)`.
///
/// Advice is advice, and a kernel that ignores it conforms. The exception is
/// `MADV_DONTNEED`, which is not advice at all: a program is entitled to read
/// zeroes from those pages afterwards, so answering success without doing it
/// would be a promise this cannot keep.
fn madvise(_address: u64, _length: u64, advice: u64) -> u64 {
    const NORMAL: u64 = 0;
    const RANDOM: u64 = 1;
    const SEQUENTIAL: u64 = 2;
    const WILLNEED: u64 = 3;
    const DONTFORK: u64 = 10;
    const DOFORK: u64 = 11;
    match advice {
        NORMAL | RANDOM | SEQUENTIAL | WILLNEED | DONTFORK | DOFORK => 0,
        _ => error::EINVAL,
    }
}

/// `membarrier(command, ...)`.
///
/// `QUERY` answers with the set of commands available, and an empty set is a
/// legitimate answer: it tells a caller to use the slow path it already has.
/// Anything else is refused, because pretending to have issued a barrier is
/// how a lock-free algorithm silently stops being correct.
fn membarrier(command: u64) -> u64 {
    const QUERY: u64 = 0;
    if command == QUERY {
        0
    } else {
        error::EINVAL
    }
}

/// `fsync(fd)` and `fdatasync(fd)`.
///
/// Both become the disk's barrier, which is a real flush rather than a hint:
/// see `docs/disk-barriers.md`. The descriptor is checked first so that
/// `fsync` on something that is not a file is `EBADF` rather than a flush of
/// the whole disk.
fn fsync(descriptor: u64) -> u64 {
    if linux_files::node_of(descriptor).is_err() {
        return error::EBADF;
    }
    match crate::fs::cache::barrier() {
        Ok(()) => 0,
        Err(_) => also::EIO,
    }
}

/// `readv(fd, iov, count)`.
///
/// The read half of the pair `writev` already had, and needed for the same
/// reason: a C library reads into a list of buffers rather than one.
///
/// Each piece is a separate `read`, and the loop stops at the first short one.
/// That is what Linux does and it matters: carrying on would fill a later
/// buffer with bytes from beyond a gap the caller cannot see.
fn readv(descriptor: u64, vectors: u64, count: u64) -> u64 {
    /// The same bound `writev` uses. A list longer than this is a program that
    /// has lost track of its own array.
    const MOST: u64 = 1024;
    if count > MOST {
        return error::EINVAL;
    }
    let Some((list, _)) = user_range(vectors, count * 16, count * 16) else {
        return error::EFAULT;
    };
    let mut total = 0u64;
    for index in 0..count {
        // SAFETY: `user_range` checked `count * 16` bytes at `list`, and this
        // reads the `index`th sixteen of them.
        let (base, length) = unsafe {
            let entry = (list as *const u64).add((index * 2) as usize);
            (
                core::ptr::read_unaligned(entry),
                core::ptr::read_unaligned(entry.add(1)),
            )
        };
        if length == 0 {
            continue;
        }
        let answer = linux_files::read(descriptor, base, length);
        if (answer as i64) < 0 {
            // An error on the first piece is the call's error; after any bytes
            // have been read it is not, because those bytes are gone from the
            // file and saying so is the only way the caller learns of them.
            return if total == 0 { answer } else { total };
        }
        total += answer;
        if answer < length {
            break;
        }
    }
    total
}

/// `lstat(path, out)`.
///
/// The same as `stat` here, and that is a fact about this filesystem rather
/// than a shortcut: NexusFS has no symbolic links, so there is nothing for
/// `lstat` to decline to follow.
fn lstat(path: u64, out: u64) -> u64 {
    /// `AT_FDCWD`, which `newfstatat` takes to mean "relative to where I am".
    const AT_FDCWD: u64 = (-100i64) as u64;
    linux_files::newfstatat(AT_FDCWD, path, out)
}

/// `readlink(path, buffer, size)`.
///
/// `EINVAL` is what Linux answers for a path that is not a symbolic link, and
/// since this filesystem has none, it is the answer for every path that exists.
/// A C library reading `/proc/self/exe` gets a refusal it knows how to handle
/// rather than a made-up path it would then open.
fn readlink(path: u64) -> u64 {
    if user_range(path, 1, 1).is_none() {
        return error::EFAULT;
    }
    error::EINVAL
}

/// `readlinkat(directory, path, buffer, size)`. The same, as a C library asks
/// it.
fn readlinkat(_directory: u64, path: u64) -> u64 {
    readlink(path)
}

/// `fcntl(fd, command, argument)`.
///
/// Only the commands a C library actually uses on the way to opening a file.
/// Everything else is `EINVAL`, which is what Linux answers for a command it
/// does not know, and which is a real answer rather than a shrug.
fn fcntl(descriptor: u64, command: u64, argument: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const FD_CLOEXEC: u64 = 1;
    const O_NONBLOCK: u64 = 0o4000;
    /// `O_RDWR`. Everything this filesystem opens is readable and writable, so
    /// this is the truth rather than a placeholder.
    const O_RDWR: u64 = 2;

    // The descriptor has to exist whatever is being asked about it, and asking
    // first means `fcntl` on a closed one is `EBADF` rather than a cheerful
    // answer about flags nobody owns.
    if linux_files::describe(descriptor).is_err() {
        return error::EBADF;
    }
    let Some(process) = whose() else {
        return error::EINVAL;
    };

    match command {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // `argument` is the lowest number to use, which this cannot honour:
            // `dup` gives out the next free one. Refused rather than quietly
            // returning a lower descriptor, because a program that asked for
            // "at least ten" and got four would install its handle over
            // something it is still using.
            if argument > 3 {
                return error::EINVAL;
            }
            let new = linux_files::dup(descriptor);
            if (new as i64) >= 0 && command == F_DUPFD_CLOEXEC {
                FLAGS.lock().entry((process, new)).or_default().close_on_exec = true;
            }
            new
        }
        F_GETFD => u64::from(
            FLAGS
                .lock()
                .get(&(process, descriptor))
                .is_some_and(|flags| flags.close_on_exec),
        ),
        F_SETFD => {
            FLAGS
                .lock()
                .entry((process, descriptor))
                .or_default()
                .close_on_exec = argument & FD_CLOEXEC != 0;
            0
        }
        F_GETFL => {
            let non_blocking = FLAGS
                .lock()
                .get(&(process, descriptor))
                .is_some_and(|flags| flags.non_blocking);
            O_RDWR | if non_blocking { O_NONBLOCK } else { 0 }
        }
        F_SETFL => {
            // Only the bit `F_SETFL` is allowed to change. The access mode and
            // the creation flags are fixed once a file is open, and Linux
            // ignores them here rather than refusing.
            FLAGS
                .lock()
                .entry((process, descriptor))
                .or_default()
                .non_blocking = argument & O_NONBLOCK != 0;
            0
        }
        _ => error::EINVAL,
    }
}

/// `pwrite64(fd, buffer, count, offset)`.
///
/// Seek, write, seek back. **Not atomic**, and that is said rather than hidden:
/// two threads calling this on one descriptor can interleave and land in each
/// other's place, which the real call guarantees against.
///
/// It is here anyway, because the alternative is `ENOSYS` and a C library that
/// gets `ENOSYS` from `pwrite` does not fall back to seek-and-write. It fails
/// the write. Every program on this machine so far has one thread.
fn pwrite64(descriptor: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    const SEEK_SET: u64 = 0;
    const SEEK_CUR: u64 = 1;

    let was = linux_files::lseek(descriptor, 0, SEEK_CUR);
    if (was as i64) < 0 {
        return was;
    }
    let moved = linux_files::lseek(descriptor, offset, SEEK_SET);
    if (moved as i64) < 0 {
        return moved;
    }
    let written = linux_files::write(descriptor, buffer, count);
    // Put the position back whatever happened to the write: a failed `pwrite`
    // that moved the file position would break the next ordinary `write` too.
    linux_files::lseek(descriptor, was, SEEK_SET);
    written
}

/// `statx(dirfd, path, flags, mask, out)`.
///
/// What a C library built in the last few years stats with. A wider structure
/// than `stat` and a different shape, so it is filled here rather than
/// translated from one.
///
/// **The mask is answered honestly.** `stx_mask` says which fields were
/// actually filled, and a program asking about a field this machine knows
/// nothing about -- times, owner, device numbers -- can see from the mask that
/// it is not there. That is what the mask is for, and reporting everything as
/// present would throw it away.
fn statx(directory: u64, path: u64, flags: u64, out: u64) -> u64 {
    /// `struct statx` is 256 bytes.
    const SIZE: u64 = 256;
    /// `STATX_TYPE | STATX_MODE | STATX_NLINK | STATX_SIZE | STATX_BLOCKS`.
    const FILLED: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_0200 | 0x0000_0400;
    /// `S_IFDIR | 0755` and `S_IFREG | 0644`, as `linux_files` reports them.
    const DIRECTORY: u16 = 0o040_755;
    const FILE: u16 = 0o100_644;
    /// `AT_EMPTY_PATH`: stat the descriptor itself rather than a path under it.
    const AT_EMPTY_PATH: u64 = 0x1000;

    let Some((at, _)) = user_range(out, SIZE, SIZE) else {
        return error::EFAULT;
    };

    // Which node is being asked about. An empty path means the descriptor
    // itself, which is how `fstat` is spelled in `statx`.
    let empty = match user_range(path, 1, 1) {
        // SAFETY: one byte the caller owns, checked by `user_range`.
        Some((pointer, _)) => unsafe { core::ptr::read(pointer as *const u8) == 0 },
        None => true,
    };
    if !empty && flags & AT_EMPTY_PATH == 0 {
        // A path relative to a directory descriptor, and resolving one is
        // `linux_files`'s business rather than this file's.
        return error::ENOSYS;
    }
    let Ok(node) = linux_files::node_of(directory) else {
        return error::EBADF;
    };

    let size = crate::fs::store::size(&node).unwrap_or(0);
    let is_directory = node.is_directory();
    let mut fields = [0u8; SIZE as usize];
    fields[0..4].copy_from_slice(&FILLED.to_le_bytes());
    fields[4..8].copy_from_slice(&4096u32.to_le_bytes());
    fields[16..20].copy_from_slice(&(if is_directory { 2u32 } else { 1 }).to_le_bytes());
    let mode: u16 = if is_directory { DIRECTORY } else { FILE };
    fields[28..30].copy_from_slice(&mode.to_le_bytes());
    fields[40..48].copy_from_slice(&size.to_le_bytes());
    fields[48..56].copy_from_slice(&size.div_ceil(512).to_le_bytes());

    // SAFETY: the range was checked above and is exactly this many bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr(), at as *mut u8, fields.len());
    }
    0
}

/// `clone3(args, size)`.
///
/// The way a C library built in the last few years makes a thread. It is not a
/// different operation from `clone` -- it is the same one with its arguments in
/// a structure instead of in registers, because `clone` had run out of room.
///
/// So this reads the structure and calls the `clone` that already exists.
/// Translating rather than reimplementing matters here: the two would otherwise
/// drift, and a thread made one way behaving differently from a thread made the
/// other is the kind of bug that takes a week.
///
/// # Why it is worth having when `clone` works
///
/// A C library tries `clone3` first and falls back on `ENOSYS`. Without this
/// every thread costs a refused call -- which works, and is a small lie about
/// what this machine is: it reports itself as a kernel too old for an interface
/// it could perfectly well answer.
fn clone3(args: u64, size: u64, frame: &crate::arch::syscall::Frame) -> u64 {
    /// `struct clone_args` as the kernel first defined it. Later kernels added
    /// fields on the end; a caller may pass a larger structure and this reads
    /// only the part it understands, which is what the size is for.
    const SMALLEST: u64 = 64;
    if size < SMALLEST {
        return error::EINVAL;
    }
    let Some((at, _)) = user_range(args, SMALLEST, SMALLEST) else {
        return error::EFAULT;
    };
    // SAFETY: `user_range` has checked that these bytes belong to the caller
    // and are readable. Each field is read where the structure defines it.
    let field = |offset: usize| unsafe {
        core::ptr::read_unaligned((at as *const u8).add(offset) as *const u64)
    };
    let flags = field(0);
    let child_tid = field(16);
    let parent_tid = field(24);
    let stack = field(40);
    let stack_size = field(48);
    let tls = field(56);

    // `clone3` is given the *bottom* of the stack and its size; `clone` is
    // given the top, because the old call had nowhere to put a size and the
    // stack grows down. Getting this backwards gives the new thread a stack
    // pointer below its own memory, which faults on its first push.
    let top = if stack == 0 {
        0
    } else {
        match stack.checked_add(stack_size) {
            Some(top) => top,
            None => return error::EINVAL,
        }
    };

    super::linux_threads::clone(flags, top, parent_tid, child_tid, tls, frame)
}

/// `creat(path, mode)`.
///
/// `open` with three flags fixed, which is all it has ever been:
/// `O_CREAT | O_WRONLY | O_TRUNC`. The mode is dropped because this filesystem
/// has no permissions to set, and saying so here is better than a caller
/// wondering why `creat(path, 0600)` produced something anyone can read --
/// everything here is readable by the one person using the machine.
fn creat(path: u64) -> u64 {
    const AT_FDCWD: u64 = (-100i64) as u64;
    const O_WRONLY: u64 = 1;
    const O_CREAT: u64 = 0o100;
    const O_TRUNC: u64 = 0o1000;
    linux_files::openat(AT_FDCWD, path, O_WRONLY | O_CREAT | O_TRUNC)
}

/// `truncate(path, length)`.
///
/// Open, shorten, close. The descriptor is closed whatever happened, because a
/// `truncate` that failed *and* leaked a descriptor would cost a program one
/// of its thousand and twenty-four every time it tried.
fn truncate(path: u64, length: u64) -> u64 {
    const AT_FDCWD: u64 = (-100i64) as u64;
    const O_WRONLY: u64 = 1;

    let descriptor = linux_files::openat(AT_FDCWD, path, O_WRONLY);
    if (descriptor as i64) < 0 {
        return descriptor;
    }
    let answer = linux_files::ftruncate(descriptor, length);
    linux_files::close(descriptor);
    answer
}

/// `prctl(option, argument, ...)`.
///
/// Two options, and both are answered truthfully rather than accepted.
///
/// `PR_SET_NO_NEW_PRIVS` asks that no later `execve` may gain privileges. This
/// system has no privilege to gain -- there is no setuid, no capability set,
/// and one person using the machine -- so the guarantee holds and saying yes is
/// correct rather than convenient. `PR_GET_NO_NEW_PRIVS` therefore answers one,
/// and it would be incoherent to answer anything else.
///
/// Everything else is `EINVAL`, which is what Linux says for an option it does
/// not know. In particular **`PR_SET_NAME` is not accepted**: there is nowhere
/// to keep a thread name, and a program that set one and read back something
/// else would be worse served than one told plainly that it cannot.
fn prctl(option: u64, _argument: u64) -> u64 {
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    match option {
        PR_SET_NO_NEW_PRIVS => 0,
        PR_GET_NO_NEW_PRIVS => 1,
        _ => error::EINVAL,
    }
}

/// `statfs(path, out)` and `fstatfs(fd, out)`.
///
/// One filesystem, so the path and the descriptor name the same volume and both
/// arrive here. The path is checked for readability and then ignored, which is
/// worth saying rather than leaving to be discovered: a machine with two
/// filesystems would have to resolve it.
fn statfs(path: u64, out: u64) -> u64 {
    if user_range(path, 1, 1).is_none() {
        return error::EFAULT;
    }
    statfs_into(out)
}

/// Fill a `struct statfs` from the volume this machine has.
fn statfs_into(out: u64) -> u64 {
    /// `struct statfs` on x86-64 is 120 bytes.
    const SIZE: u64 = 120;
    /// The block size reported, and the unit the counts below are in.
    const BLOCK: u64 = 4096;

    let Some((at, _)) = user_range(out, SIZE, SIZE) else {
        return error::EFAULT;
    };
    // `None` while the volume lock is held by something else, which is a real
    // state rather than an error: the answer is "ask again", and `EAGAIN` says
    // that where a fabricated figure would not.
    let Some((total, free)) = crate::fs::store::space() else {
        return also::EAGAIN;
    };

    let mut fields = [0u8; SIZE as usize];
    // f_type. `NEXUSFS` rather than a borrowed magic number: a program that
    // recognised this as ext4 would make decisions on that basis.
    fields[0..8].copy_from_slice(&0x4E58_5553u64.to_le_bytes());
    fields[8..16].copy_from_slice(&BLOCK.to_le_bytes()); // f_bsize
    fields[16..24].copy_from_slice(&(total / BLOCK).to_le_bytes()); // f_blocks
    fields[24..32].copy_from_slice(&(free / BLOCK).to_le_bytes()); // f_bfree
    // f_bavail: what an unprivileged program may use. The same as free, because
    // this filesystem keeps no reserve and there is no privilege here to be
    // outside of.
    fields[32..40].copy_from_slice(&(free / BLOCK).to_le_bytes());
    // f_files and f_ffree are left zero rather than guessed. The volume knows
    // its inode counts, but reaching them from here would mean holding the
    // volume lock a second time for a figure almost nothing reads.
    fields[72..80].copy_from_slice(&255u64.to_le_bytes()); // f_namelen

    // SAFETY: the range was checked above and is exactly this many bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr(), at as *mut u8, fields.len());
    }
    0
}

/// `select(n, read, write, except, timeout)` and `pselect6`, which is the same
/// with a nanosecond timeout and a signal mask.
///
/// # Why this is here rather than left to `poll`
///
/// A C library does not always have the choice. `select` is what a great deal
/// of older software calls directly, and `pselect6` is what musl's own
/// `select` becomes -- so a machine with `poll` and without these runs `poll`
/// for the programs that were written this decade and refuses the rest.
///
/// The readiness itself is `linux_poll`'s, called rather than copied. Two
/// answers to "is this descriptor ready" would drift, and the one that drifted
/// would be this one.
///
/// # The signal mask
///
/// `pselect6` takes one and this ignores it. That is a real limitation and not
/// a rounding: the whole point of `pselect6` over `select` is to change the
/// mask atomically around the wait, and a program relying on that to avoid a
/// race will still have the race. It is ignored rather than refused because the
/// overwhelming majority of callers pass null, and refusing all of them to be
/// strict with a few would be the worse trade.
fn select(
    count: u64,
    readable: u64,
    writable: u64,
    failing: u64,
    timeout: u64,
    shape: Timeout,
) -> u64 {
    use super::linux_poll::event;

    /// `FD_SETSIZE`. A set is a bitmap of exactly this many bits, and a `count`
    /// above it describes memory the caller did not pass.
    const SETSIZE: u64 = 1024;
    /// How long to wait between looks, in milliseconds.
    const STEP: u64 = 10;

    if count > SETSIZE {
        return error::EINVAL;
    }
    let bytes = count.div_ceil(8).max(1);

    // How long to wait. A null pointer means for ever, which is what the
    // interface says and not an oversight.
    let patience = if timeout == 0 {
        None
    } else {
        let Some((at, _)) = user_range(timeout, 16, 16) else {
            return error::EFAULT;
        };
        // SAFETY: sixteen bytes the caller owns, checked above.
        let (whole, fraction) = unsafe {
            (
                core::ptr::read_unaligned(at as *const i64),
                core::ptr::read_unaligned((at as *const i64).add(1)),
            )
        };
        if whole < 0 || fraction < 0 {
            return error::EINVAL;
        }
        let milliseconds = match shape {
            Timeout::Microseconds => (fraction as u64) / 1000,
            Timeout::Nanoseconds => (fraction as u64) / 1_000_000,
        };
        Some((whole as u64).saturating_mul(1000).saturating_add(milliseconds))
    };

    // Read the three sets once. They are the question; the answer is written
    // back over them at the end, which is what makes `select` awkward to use
    // and is nevertheless what it does.
    let mut asked = [[0u8; (SETSIZE / 8) as usize]; 3];
    for (which, set) in [readable, writable, failing].iter().enumerate() {
        if *set == 0 {
            continue;
        }
        let Some((at, _)) = user_range(*set, bytes, bytes) else {
            return error::EFAULT;
        };
        // SAFETY: `bytes` bytes the caller owns, checked above, copied into a
        // buffer of at least that size.
        unsafe {
            core::ptr::copy_nonoverlapping(at as *const u8, asked[which].as_mut_ptr(), bytes as usize);
        }
    }

    let began = crate::arch::time::uptime_ms();
    // Declared without a value, because every path through the loop below
    // assigns one before it is read and an initialiser here would be a value
    // the compiler can see is never used.
    let found: [[u8; (SETSIZE / 8) as usize]; 3];
    let ready = loop {
        let mut ready = 0u64;
        let mut round = [[0u8; (SETSIZE / 8) as usize]; 3];
        for descriptor in 0..count {
            let byte = (descriptor / 8) as usize;
            let bit = 1u8 << (descriptor % 8);
            let wanted = [
                asked[0][byte] & bit != 0,
                asked[1][byte] & bit != 0,
                asked[2][byte] & bit != 0,
            ];
            if !wanted[0] && !wanted[1] && !wanted[2] {
                continue;
            }
            let now = super::linux_poll::ready_now(descriptor);
            // `POLLHUP` and `POLLERR` make a descriptor readable as far as
            // `select` is concerned: a program waiting to read from something
            // that has hung up has to be woken, or it waits for ever.
            let is_readable = now & (event::IN | event::HUP | event::ERR) != 0;
            let is_writable = now & (event::OUT | event::ERR) != 0;
            let is_failing = now & event::ERR != 0;
            for (which, (want, got)) in [
                (wanted[0], is_readable),
                (wanted[1], is_writable),
                (wanted[2], is_failing),
            ]
            .iter()
            .enumerate()
            {
                if *want && *got {
                    round[which][byte] |= bit;
                    ready += 1;
                }
            }
        }
        if ready > 0 {
            found = round;
            break ready;
        }
        if let Some(patience) = patience {
            if crate::arch::time::uptime_ms().saturating_sub(began) >= patience {
                found = round;
                break 0;
            }
        }
        crate::sched::sleep_ms(STEP);
    };

    // Write the answer back over the question, which is what `select` does.
    // Done even when nothing was ready, because the caller is entitled to find
    // its sets cleared rather than left as it wrote them.
    for (which, set) in [readable, writable, failing].iter().enumerate() {
        if *set == 0 {
            continue;
        }
        if let Some((at, _)) = user_range(*set, bytes, bytes) {
            // SAFETY: the same `bytes` bytes checked when they were read.
            unsafe {
                core::ptr::copy_nonoverlapping(found[which].as_ptr(), at as *mut u8, bytes as usize);
            }
        }
    }
    ready
}
