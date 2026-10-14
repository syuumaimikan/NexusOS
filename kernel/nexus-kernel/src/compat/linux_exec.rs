//! Replacing a program with another one.
//!
//! `execve` is the second half of how Unix starts programs, and on Linux it is
//! usually the *only* half that matters to the program being started: a
//! launcher calls `fork` to get a second process and `execve` to make it
//! something else. There is no `fork` here — see `compat::linux_threads` for
//! why — so `execve` on its own is what is left, and what it does is exactly
//! what it says: this process stops being this program and starts being that
//! one.
//!
//! That is not a lesser thing. A bootstrapper is a program that works out which
//! real program to run and then becomes it, and that is the shape of nearly
//! every installer and launcher there is, Steam's included.
//!
//! # What survives, and what does not
//!
//! The process survives: same identifier, same handles, same threads — of which
//! there must be exactly one; see below. Its *memory* does not. The address
//! space is emptied and the new program is loaded into the same one, which is
//! what makes the identifier meaningful: everything that pointed at this
//! process still does.
//!
//! Descriptors survive, which is what makes a launcher able to hand the program
//! it starts an already-open connection. `O_CLOEXEC` is meant to say otherwise
//! and is not honoured here: nothing records it yet, so a descriptor marked
//! close-on-exec stays open. That is a real difference and it is the safe
//! direction only for programs that were not relying on it — which is written
//! down rather than discovered.
//!
//! Signal handlers do not survive, and must not: the handler was a function in
//! a program that no longer exists, and entering it would be a jump into
//! whatever the new program has at that address. Linux resets them for the same
//! reason.
//!
//! # Why more than one thread is refused
//!
//! Linux kills every other thread first. Doing that here means asking them to
//! stop and *waiting* for each to notice, with the calling thread holding no
//! locks and the address space still intact — and then tearing the space down
//! underneath whatever has not noticed. The failure mode of getting it slightly
//! wrong is a thread running in a freed address space, which is the worst
//! failure this system has.
//!
//! So a threaded program is refused, by name, with the error Linux uses when it
//! cannot start something. That is a limit worth having written down rather
//! than a race worth having.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::arch::syscall::Frame;

use super::linux::error;

/// The most arguments or environment variables one `execve` will carry.
///
/// The arrays come from ring 3 and are terminated by a null the caller supplies,
/// so there has to be a bound: without one a program could hand over a list that
/// does not end.
const MAX_STRINGS: usize = 256;
/// And the most bytes in one of them.
const MAX_STRING: usize = 4096;

/// Too many, or too long.
const E2BIG: u64 = (-7i64) as u64;
/// Not permitted: more than one thread.
const EAGAIN: u64 = (-11i64) as u64;

/// Programs replaced.
static REPLACED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many times a program has replaced itself.
#[must_use]
pub fn statistics() -> u64 {
    REPLACED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Read a null-terminated array of strings out of the caller's memory.
///
/// Two levels of pointer, both from ring 3, both checked: the array itself and
/// every string in it. A program that handed over an array with no terminator
/// gets `E2BIG` rather than a kernel walking its address space.
fn strings_at(array: u64, pointer_bytes: u64) -> Result<Vec<String>, u64> {
    if !matches!(pointer_bytes, 4 | 8) {
        return Err(error::EINVAL);
    }
    let mut out = Vec::new();
    if array == 0 {
        return Ok(out);
    }
    for index in 0..MAX_STRINGS {
        let offset = (index as u64) * pointer_bytes;
        let Some((at, _)) =
            crate::arch::syscall::user_range(array + offset, pointer_bytes, pointer_bytes)
        else {
            return Err(error::EFAULT);
        };
        // SAFETY: the ABI's four/eight-byte pointer is in the checked range.
        let pointer = unsafe {
            if pointer_bytes == 4 {
                core::ptr::read_unaligned(at as *const u32) as u64
            } else {
                core::ptr::read_unaligned(at as *const u64)
            }
        };
        if pointer == 0 {
            return Ok(out);
        }
        out.push(string_at(pointer)?);
    }
    Err(E2BIG)
}

/// And one string.
fn string_at(pointer: u64) -> Result<String, u64> {
    let mut bytes: Vec<u8> = Vec::new();
    for step in 0..MAX_STRING {
        let Some((at, _)) = crate::arch::syscall::user_range(pointer + step as u64, 1, 1) else {
            return Err(error::EFAULT);
        };
        // SAFETY: one byte inside the user half.
        let byte = unsafe { core::ptr::read_volatile(at as *const u8) };
        if byte == 0 {
            return String::from_utf8(bytes).map_err(|_| error::EINVAL);
        }
        bytes.push(byte);
    }
    Err(E2BIG)
}

/// `execve(path, argv, envp)`.
///
/// Does not return on success: the frame it was given is rewritten so that the
/// system-call path restores the *new* program's registers instead of this
/// one's, which is the same mechanism a signal handler is entered through.
pub fn execve(path: u64, argv: u64, envp: u64, frame: &mut Frame) -> u64 {
    // Everything is read out of the old address space before any of it is taken
    // away. After the teardown there is no `path` to read and no `argv` to walk:
    // they were in the memory that has just gone.
    let path = match string_at(path) {
        Ok(path) => path,
        Err(reason) => return reason,
    };
    let arguments = match strings_at(argv, 8) {
        Ok(arguments) => arguments,
        Err(reason) => return reason,
    };
    let environment = match strings_at(envp, 8) {
        Ok(environment) => environment,
        Err(reason) => return reason,
    };
    execve_values(
        super::linux_files::AT_FDCWD as u32 as u64,
        path,
        arguments,
        environment,
        frame,
    )
}

/// The i386 form uses 32-bit pointers in both arrays.
pub fn execve32(path: u64, argv: u64, envp: u64, frame: &mut Frame) -> u64 {
    let path = match string_at(path) {
        Ok(path) => path,
        Err(reason) => {
            crate::kprintln!("[linux32] execve path pointer failed: {reason}");
            return reason;
        }
    };
    crate::kprintln!("[linux32] execve {path}");
    let arguments = match strings_at(argv, 4) {
        Ok(arguments) => arguments,
        Err(reason) => return reason,
    };
    let environment = match strings_at(envp, 4) {
        Ok(environment) => environment,
        Err(reason) => return reason,
    };
    execve_values(
        super::linux_files::AT_FDCWD as u32 as u64,
        path,
        arguments,
        environment,
        frame,
    )
}

/// i386 `execveat(dirfd, path, argv, envp, flags)`.
pub fn execveat32(
    dirfd: u64,
    path: u64,
    argv: u64,
    envp: u64,
    flags: u64,
    frame: &mut Frame,
) -> u64 {
    // AT_EMPTY_PATH needs execution directly from an open descriptor. That
    // path is not implemented yet; reject all flags instead of silently
    // executing a different object.
    if flags != 0 {
        return error::EINVAL;
    }
    let path = match string_at(path) {
        Ok(path) => path,
        Err(reason) => {
            crate::kprintln!("[linux32] execveat path pointer failed: {reason}");
            return reason;
        }
    };
    crate::kprintln!("[linux32] execveat {path} dirfd={dirfd}");
    let arguments = match strings_at(argv, 4) {
        Ok(arguments) => arguments,
        Err(reason) => return reason,
    };
    let environment = match strings_at(envp, 4) {
        Ok(environment) => environment,
        Err(reason) => return reason,
    };
    execve_values(dirfd as u32 as u64, path, arguments, environment, frame)
}

fn execve_values(
    dirfd: u64,
    path: String,
    arguments: Vec<String>,
    environment: Vec<String>,
    frame: &mut Frame,
) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    let resolved = match super::linux_paths::resolve(dirfd, &path) {
        Ok(resolved) => resolved,
        Err(reason) => {
            crate::kprintln!("[linux] exec of {path} could not resolve: {reason}");
            return reason;
        }
    };
    let executable = match resolved.node() {
        Ok(node) => node,
        Err(reason) => {
            crate::kprintln!("[linux] exec of {path} could not open node: {reason}");
            return reason;
        }
    };
    if executable.is_directory() {
        return (-21i64) as u64;
    }
    let executable_path = super::linux_proc::link(&resolved.path)
        .and_then(Result::ok)
        .unwrap_or(resolved.path);
    let image = match crate::fs::store::read_node(&executable).map_err(super::linux_files::errno) {
        Ok(image) => image,
        Err(reason) => {
            crate::kprintln!("[linux] exec of {path} could not read image: {reason}");
            return reason;
        }
    };

    // One thread, for the reason at the top of the file.
    let siblings = super::linux_threads::siblings(process.id.0);
    if !siblings.is_empty() {
        crate::kprintln!(
            "[linux] {} called execve with {} other thread(s) running; answering EAGAIN",
            process.name.as_str(),
            siblings.len()
        );
        return EAGAIN;
    }

    // Past this point there is no going back: the program that called this is
    // about to stop existing. An error from here on cannot be returned to it,
    // because there is nothing left to return to -- so everything that can fail
    // has failed already, above.
    //
    // SAFETY: the calling thread is in the kernel and is this process's only
    // thread, so nothing is running in the user half.
    unsafe { process.address_space.clear_user_half() };

    // What the old program had arranged, which the new one has not. Handlers
    // first, because a handler is a function in a program that no longer
    // exists and entering it would be a jump into whatever is at that address
    // now.
    super::linux_signal::forget(process.id.0);
    super::linux_memory::forget(process.id.0);
    super::linux_display::forget(process.id.0);

    // SAFETY: the space was just emptied and nothing is running in it.
    let started = unsafe {
        if process.personality == crate::process::Personality::Linux32 {
            crate::user::load_program_into32(
                &process.address_space,
                &path,
                &image,
                &arguments,
                &environment,
            )
        } else {
            crate::user::load_program_into(
                &process.address_space,
                &path,
                &image,
                &arguments,
                &environment,
            )
        }
    };
    let (entry, stack) = match started {
        Ok(started) => started,
        Err(reason) => {
            // The old program is gone and the new one will not load. There is
            // nothing to return to, so the process ends -- which is what Linux
            // does when an `execve` fails after the point of no return, and is
            // why it tries very hard to fail before it.
            crate::kprintln!(
                "[linux] {} could not become {path}: {reason}; the process ends",
                process.name.as_str()
            );
            process.completion.finish(127);
            crate::arch::interrupts::disable();
            crate::sched::exit();
        }
    };
    stack.record_image(&process, &executable_path, Some(executable));

    // The new program's starting state. Every register zero but the two that
    // matter, which is what a program is entitled to: the System V ABI says
    // only `rsp` is defined at entry, and that `rdx` is either a function to
    // register with `atexit` or zero. Leaving the old program's values in place
    // would be handing the new one somebody else's pointers.
    *frame = Frame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r9: 0,
        r8: 0,
        r10: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        rax: 0,
        // Interrupts on, and nothing else: a program does not inherit the
        // arithmetic flags of the one it replaced.
        rflags: 0x202,
        rip: entry,
        rsp: stack.pointer,
    };

    // And the thread pointer, which belonged to the old program's thread-local
    // storage. A new program sets its own with `arch_prctl`, and one that read
    // `fs` before doing so would be reading the old program's memory -- which
    // is no longer mapped, so it would fault rather than mislead.
    //
    // SAFETY: writing this processor's `IA32_FS_BASE`, which is per-thread
    // state the scheduler saves and restores with the thread.
    unsafe { crate::arch::syscall::write_msr(0xC000_0100, 0) };

    REPLACED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::kprintln!(
        "[linux] process {} \"{}\" became {path}: entry {entry:#x}, {} argument(s)",
        process.id,
        process.name.as_str(),
        arguments.len()
    );
    0
}
