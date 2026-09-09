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

use crate::kprintln;

/// The Linux call numbers this understands.
///
/// x86-64's numbering, which is its own: `write` is 1 here and 4 on i386, and
/// the fact that the number depends on the architecture is exactly why a
/// translation layer is per-architecture work.
mod call {
    pub const WRITE: u64 = 1;
    pub const GETPID: u64 = 39;
    pub const EXIT: u64 = 60;
    pub const EXIT_GROUP: u64 = 231;
}

/// Errors, as Linux returns them: negative, in the return register.
///
/// Not a separate error channel and not a flag — the value itself is the
/// answer, and a caller tells the two apart by whether it is in the top page of
/// the address space. That convention is why a Linux `write` can return a byte
/// count and an error in the same register.
mod error {
    /// Function not implemented.
    pub const ENOSYS: u64 = (-38i64) as u64;
    /// Bad file descriptor.
    pub const EBADF: u64 = (-9i64) as u64;
    /// Bad address.
    pub const EFAULT: u64 = (-14i64) as u64;
}

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

/// How many calls have been translated, and how many refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        TRANSLATED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
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
    if descriptor != 1 && descriptor != 2 {
        return error::EBADF;
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
    let name = crate::sched::current_process().map_or_else(
        || String::from("?"),
        |process| String::from(process.name.as_str()),
    );
    kprintln!("[linux] {name} wrote: {text}");

    // What was actually taken, not what was asked for.
    wanted as u64
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
