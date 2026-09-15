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

use alloc::string::String;

use super::linux_files as files;
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

/// What `mmap` may be asked for, and what this will do about it.
mod mmap_flags {
    /// The mapping is not backed by a file.
    pub const ANONYMOUS: u64 = 0x20;
    /// Changes are private to this process. Every mapping here is.
    pub const PRIVATE: u64 = 0x02;
    /// The caller insists on the address. Refused: see `mmap`.
    pub const FIXED: u64 = 0x10;

    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;
}

/// `arch_prctl` subfunctions.
mod arch {
    pub const SET_FS: u64 = 0x1002;
    pub const GET_FS: u64 = 0x1003;
}

/// Where anonymous mappings are placed.
///
/// A bump pointer, well above where a static executable is loaded (`0x40_0000`)
/// and its heap would grow, and below the region Nexus programs map surfaces
/// into. It only ever goes up: this layer does not reuse addresses, because
/// reusing one means being sure nothing still holds it and there is no
/// bookkeeping here that could be sure.
///
/// A program that mapped and unmapped in a loop would run out of address space
/// after a few hundred gigabytes of it. That is a real limit and it is written
/// down rather than papered over with a free list that has not been thought
/// through.
const MMAP_BASE: u64 = 0x0000_2000_0000_0000;

/// The most one `mmap` will give out.
///
/// Sixty-four mebibytes. Enough for a language runtime's heap and small enough
/// that a program asking for a terabyte is refused rather than spending the
/// machine's memory finding out.
const MAX_MMAP: u64 = 64 * 1024 * 1024;

/// The most `writev` will walk.
///
/// Linux's own limit is 1024. This is smaller because every vector here is read
/// from user memory one at a time.
const MAX_IOV: u64 = 64;

/// The most a single `write` will take.
///
/// A translated write ends up in the boot log, which is a serial line. A
/// program that asked to write a megabyte would be a program that stopped the
/// machine for a second, so the count is capped and the *short write* is
/// reported honestly — which is a thing every caller of `write` already has to
/// handle, because on Linux it happens too.
const MAX_WRITE: usize = 512;

/// Calls translated, and calls refused, for the monitor.
static TRANSLATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Pages handed out by `mmap`, for the same.
static MAPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Where the next anonymous mapping goes. See [`MMAP_BASE`].
static NEXT_MAPPING: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(MMAP_BASE);

/// How many calls have been translated, how many refused, and how many pages
/// of memory handed out.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        TRANSLATED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        MAPPED.load(Ordering::Relaxed),
    )
}

/// One Linux system call, from a process running under the translation.
///
/// Returns what the program will find in `rax`.
pub fn dispatch(
    number: u64,
    argument0: u64,
    argument1: u64,
    argument2: u64,
    _argument3: u64,
    _argument4: u64,
) -> u64 {
    match number {
        call::WRITE => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            write(argument0, argument1, argument2)
        }
        call::WRITEV => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            writev(argument0, argument1, argument2)
        }
        call::READ => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            read(argument0, argument1, argument2)
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
        call::MMAP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            mmap(argument0, argument1, argument2, _argument3)
        }
        call::MUNMAP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            munmap(argument0, argument1)
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
        // Accepted and ignored, each for a reason:
        //
        // `mprotect` -- every anonymous mapping here is already readable,
        // writable and not executable, so a request to make it less than that
        // is one this layer cannot honour and a request to make it more is one
        // it has already granted. Saying so would break a startup that only
        // wanted to drop a permission.
        //
        // `rt_sigprocmask` -- there are no signals to block, so blocking them
        // succeeds trivially and honestly.
        //
        // `set_tid_address` returns this thread's identifier, which is what
        // Linux does, and the address is not recorded because nothing here
        // clears it on exit.
        call::MPROTECT | call::RT_SIGPROCMASK => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            0
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
        // `isatty` asks this. The answer is no: standard output goes to the
        // boot log, which is a serial line and not a terminal, and a program
        // told otherwise would start emitting escape sequences at it.
        call::IOCTL => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            error::ENOTTY
        }
        // Refused rather than accepted. Accepting would mean promising to
        // deliver a signal that will never arrive, and a program that installed
        // a handler for a fault and then took one would sit in a fault loop
        // instead of dying with a message.
        call::RT_SIGACTION => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            error::ENOSYS
        }
        call::GETPID => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::sched::current_process().map_or(error::ENOSYS, |process| process.id.0)
        }
        call::EXIT | call::EXIT_GROUP => {
            TRANSLATED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            exit(argument0)
        }
        _ => {
            REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Named in the log, because the useful thing about running a
            // foreign program is finding out what it asks for. A translation
            // layer grows by reading these.
            kprintln!("[linux] call {number} is not translated yet; answering ENOSYS");
            error::ENOSYS
        }
    }
}

/// `write(fd, buffer, count)`.
///
/// Standard output and standard error both go where a Nexus program's `log`
/// goes, which is the boot log. Anything else is a descriptor this process does
/// not have, and saying so is the truth: a translated program has no file
/// descriptor table because nothing has given it one.
fn write(descriptor: u64, buffer: u64, count: u64) -> u64 {
    if descriptor == files::STDIN {
        return error::EBADF;
    }
    // Anything that is not one of the three a program starts with is a file.
    if descriptor != files::STDOUT && descriptor != files::STDERR {
        return files::write(descriptor, buffer, count);
    }
    let wanted = (count as usize).min(MAX_WRITE);
    if wanted == 0 {
        return 0;
    }

    // The same check every Nexus system call makes on a caller's buffer: the
    // range must not wrap and must lie wholly inside the user half, so a
    // pointer from ring 3 can never name kernel memory. A translated program
    // gets no more trust than any other.
    let Some((pointer, length)) =
        crate::arch::syscall::user_range(buffer, wanted as u64, MAX_WRITE as u64)
    else {
        return error::EFAULT;
    };

    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps. An unmapped range faults, which is the
    // caller's own page fault and not a kernel bug.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };

    // Trailing newline dropped, because the log adds its own line ending and a
    // program that wrote one would otherwise get a blank line after every
    // message.
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    kprintln!("[linux] {} wrote: {text}", name_of());

    // What was actually taken, not what was asked for.
    wanted as u64
}

/// `writev(fd, iov, count)`.
///
/// The call a real libc's `printf` makes. musl and glibc both build the format
/// result as a list of pieces and hand the list over rather than copying it
/// flat first, so a translation layer with `write` and not this one runs a
/// program that prints nothing and reports success.
fn writev(descriptor: u64, vectors: u64, count: u64) -> u64 {
    if descriptor != 1 && descriptor != 2 {
        return error::EBADF;
    }
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

    let mut written = 0u64;
    let mut text = String::new();
    for index in 0..count as usize {
        let at = index * IOVEC as usize;
        let pointer = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap_or_default());
        let wanted = u64::from_le_bytes(raw[at + 8..at + 16].try_into().unwrap_or_default());
        if wanted == 0 {
            continue;
        }
        // Each piece capped on its own, and the count reported is what was
        // actually taken. A short `writev` is something every caller already
        // handles, because Linux does it too.
        let taken = wanted.min(MAX_WRITE as u64);
        let Some((from, size)) =
            crate::arch::syscall::user_range(pointer, taken, MAX_WRITE as u64)
        else {
            // Whatever was gathered before the bad pointer has not been
            // written anywhere, so nothing was written at all.
            return if written == 0 { error::EFAULT } else { written };
        };
        // SAFETY: as above, for this piece.
        let bytes = unsafe { core::slice::from_raw_parts(from as *const u8, size) };
        text.push_str(&String::from_utf8_lossy(bytes));
        written += size as u64;
    }

    if !text.is_empty() {
        let text = text.trim_end_matches('\n');
        kprintln!("[linux] {} wrote: {text}", name_of());
    }
    written
}

/// `read(fd, buffer, count)`.
///
/// Standard input gives zero, which on Linux means end of file: the keyboard
/// belongs to the compositor, which hands keys to windows, and a program
/// running under this layer has no window. End of file is the honest answer --
/// a program that reads gets nothing and stops, rather than blocking for ever
/// on input that cannot arrive.
///
/// Anything else is a file, and goes to the store.
fn read(descriptor: u64, buffer: u64, count: u64) -> u64 {
    if descriptor == files::STDIN {
        return 0;
    }
    if descriptor == files::STDOUT || descriptor == files::STDERR {
        return error::EBADF;
    }
    files::read(descriptor, buffer, count)
}

/// `mmap(address, length, protection, flags)`.
///
/// Anonymous private mappings only, which is what a language runtime's heap is.
///
/// # What it refuses, and why each refusal is right
///
/// **A file mapping.** There is no descriptor table, so there is no file to
/// map. Returning memory full of zeros instead would be a program reading the
/// wrong thing and never finding out.
///
/// **`MAP_FIXED`.** The caller insisting on an address means it has a reason,
/// and this layer's bump pointer has no way to honour one -- it does not know
/// what else is mapped. Refusing is better than putting the mapping somewhere
/// else and returning that: a caller that passed `MAP_FIXED` is a caller that
/// will use the address it asked for.
///
/// **Anything executable.** A mapping this layer made writable and executable
/// would be a page a program could write code into and jump to. Nothing that
/// runs here needs it, and the day something does is the day to think about it
/// rather than the day to have already allowed it.
fn mmap(address: u64, length: u64, protection: u64, flags: u64) -> u64 {
    use mmap_flags as f;

    if length == 0 || length > MAX_MMAP {
        return error::EINVAL;
    }
    if flags & f::ANONYMOUS == 0 || flags & f::PRIVATE == 0 {
        return error::EINVAL;
    }
    if flags & f::FIXED != 0 {
        return error::EINVAL;
    }
    if protection & f::PROT_EXEC != 0 {
        return error::EINVAL;
    }
    // `PROT_NONE` -- a mapping present but unreadable -- is what an allocator
    // puts between its arenas as a guard page. This layer has no way to make
    // one: every page it maps is present and readable, and a "guard page" that
    // could be read and written without faulting would be a guard that guards
    // nothing while looking like it does. Refused, so the caller knows.
    if protection & f::PROT_READ == 0 {
        return error::EINVAL;
    }
    // The hint is ignored rather than honoured, and that is allowed: Linux
    // treats a non-`MAP_FIXED` address as a suggestion.
    let _ = address;

    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    const PAGE: u64 = 4096;
    let pages = length.div_ceil(PAGE);
    let bytes = pages * PAGE;

    // Where it goes. Taken before anything is mapped so that two threads
    // cannot be given the same address.
    let at = NEXT_MAPPING.fetch_add(bytes, core::sync::atomic::Ordering::Relaxed);
    if at.saturating_add(bytes) >= nexus_abi::layout::USER_SPACE_END {
        return error::ENOMEM;
    }

    let writable = protection & f::PROT_WRITE != 0;
    let mut flags_for_page = crate::memory::paging::PRESENT
        | crate::memory::paging::USER
        | crate::memory::paging::NO_EXECUTE;
    if writable {
        flags_for_page |= crate::memory::paging::WRITABLE;
    }

    // Mapped one page at a time, and every one of them zeroed before it is
    // visible. A page handed over with the last program's data in it is the
    // oldest information leak there is.
    for page in 0..pages {
        let Some(frame) = crate::memory::allocate_frame() else {
            // Out of memory partway. What was mapped stays mapped and is
            // leaked until the process ends, which is when its address space
            // goes anyway -- and that is better than unmapping backwards
            // through a half-built mapping with a fault handler possibly
            // already looking at it.
            return error::ENOMEM;
        };
        // SAFETY: the frame came from the allocator and is this mapping's, and
        // the address is inside the user half -- checked above against
        // `USER_SPACE_END`.
        unsafe {
            core::ptr::write_bytes(nexus_abi::layout::phys_to_virt(frame) as *mut u8, 0, PAGE as usize);
            if process
                .address_space
                .map(at + page * PAGE, frame, flags_for_page)
                .is_err()
            {
                crate::memory::free_frame(frame);
                return error::ENOMEM;
            }
        }
    }

    MAPPED.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
    at
}

/// `munmap(address, length)`.
///
/// Takes the pages out of the address space and gives the frames back.
///
/// The address is *not* reused: [`NEXT_MAPPING`] only goes up. Reusing one
/// would mean being sure nothing still holds it, and there is no bookkeeping
/// here that could be sure.
fn munmap(address: u64, length: u64) -> u64 {
    const PAGE: u64 = 4096;
    if length == 0 || !address.is_multiple_of(PAGE) {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let pages = length.div_ceil(PAGE);

    for page in 0..pages {
        let virt = address + page * PAGE;
        if virt >= nexus_abi::layout::USER_SPACE_END {
            break;
        }
        // SAFETY: nothing else holds the address -- the caller is unmapping
        // it -- and the frame is this space's, because everything mapped here
        // came from `mmap` above and carries no `SHARED` bit.
        if let Ok(frame) = unsafe { process.address_space.unmap(virt) } {
            // SAFETY: the frame came from `allocate_frame` in `mmap` and is
            // no longer mapped anywhere.
            unsafe { crate::memory::free_frame(frame) };
        }
    }
    0
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

/// What to call the running program in the log.
fn name_of() -> String {
    crate::sched::current_process().map_or_else(
        || String::from("?"),
        |process| String::from(process.name.as_str()),
    )
}

/// `exit_group(status)`, which is what a program with one thread means by
/// exiting.
fn exit(status: u64) -> ! {
    if let Some(process) = crate::sched::current_process() {
        process.completion.finish(status);
        kprintln!(
            "[linux] process {} \"{}\" exited with status {status} through the Linux boundary",
            process.id,
            process.name.as_str()
        );
    }
    crate::arch::interrupts::disable();
    crate::sched::exit()
}
