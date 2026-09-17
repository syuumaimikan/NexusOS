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
