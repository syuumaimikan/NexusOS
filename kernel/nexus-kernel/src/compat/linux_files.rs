//! Files, for a program that was built for Linux.
//!
//! Until this existed a translated program could write to its standard output,
//! ask for memory, and nothing else. That is enough to prove the boundary is
//! real and it is not enough to run anything: the first thing a real program
//! does is open a file -- its configuration, its data, its own `/proc/self/exe`
//! -- and a layer that answers `ENOSYS` to `openat` runs nothing that was not
//! written for it.
//!
//! # A descriptor is a handle
//!
//! Not a parallel table. When a Linux program opens a file, the node goes into
//! the process's ordinary Nexus handle table and the *handle number* is what the
//! program gets back as its file descriptor. `read(fd)` looks the handle up the
//! same way a Nexus program's `read(handle)` does, through the same rights
//! check, and a descriptor closed here is a handle closed there.
//!
//! That is the rule this whole directory is built on, applied to files: there is
//! nothing a translated program can reach that a Nexus program could not, and
//! nothing below this layer knows Linux exists.
//!
//! It has one visible consequence, and it is why `Process::with_personality`
//! starts a Linux process's handles at three. Handle numbers begin at one, and
//! one is standard output. A Linux process whose first `open` returned a handle
//! numbered 1 would have opened a file that every `printf` then wrote into.
//!
//! # Where `/` is
//!
//! `linux/` in the machine's own store, made the first time a program asks for
//! it. A translated program's `/etc/hostname` is this machine's
//! `linux/etc/hostname`, and there is no path it can name that reaches outside
//! that subtree -- `..` is refused rather than walked, which is the same rule
//! `node_open` enforces for every Nexus program.
//!
//! The working directory is `/` and does not move. `chdir` is not translated,
//! so `getcwd` answering `/` is the truth rather than a placeholder.
//!
//! # What a position is
//!
//! A Nexus node has no read position: `read_node_at` takes the offset as an
//! argument, which is the honest shape, because two readers of one file do not
//! share a cursor. Linux descriptors do have one, so this keeps it -- one
//! number per open descriptor, in [`POSITIONS`], dropped when the descriptor is
//! closed or the process ends. For a directory the same number is the index of
//! the next entry `getdents64` will return.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fs::store::{self, Node, StoreError};
use crate::sync::IrqSpinLock;

use super::linux::error;

/// Open flags, as x86-64 Linux numbers them.
mod flag {
    pub const WRONLY: u64 = 0o1;
    pub const RDWR: u64 = 0o2;
    pub const CREAT: u64 = 0o100;
    pub const EXCL: u64 = 0o200;
    pub const TRUNC: u64 = 0o1000;
    pub const APPEND: u64 = 0o2000;
    pub const DIRECTORY: u64 = 0o200000;
}

/// `whence` for `lseek`.
mod seek {
    pub const SET: u64 = 0;
    pub const CUR: u64 = 1;
    pub const END: u64 = 2;
}

/// What `openat` and friends accept in place of a directory descriptor to mean
/// "relative to the working directory".
pub const AT_FDCWD: i32 = -100;

/// Whether a `dirfd` argument is [`AT_FDCWD`].
///
/// The narrowing to thirty-two bits is the whole of this function, and it is not
/// a detail. `dirfd` is an `int` in the ABI, so a compiler loads -100 with a
/// thirty-two bit `mov` -- which zero-extends, leaving `0x00000000ffffff9c` in
/// the register rather than the sign-extended `0xffffffffffffff9c`. Comparing
/// the full register against -100 as a long therefore rejects every `openat` a
/// real program makes. It cost an afternoon: the test failed at step eleven, and
/// eleven is "openat refused", with no log line from inside `openat` at all
/// because it had returned before reaching one.
#[must_use]
pub fn is_cwd(directory: u64) -> bool {
    directory as u32 as i32 == AT_FDCWD
}

/// The three descriptors a program starts with, which are not handles.
pub const STDIN: u64 = 0;
pub const STDOUT: u64 = 1;
pub const STDERR: u64 = 2;
/// The first descriptor that is a handle. See the note at the top of the file.
pub const FIRST_HANDLE: u32 = 3;

/// The most a single read or write moves.
///
/// A translated program asking for more gets a short count, which is a thing
/// every caller of `read` already has to handle because on Linux it happens
/// too. The cap exists so that one call cannot ask the kernel to hold a
/// megabyte on its own stack.
const MAX_TRANSFER: u64 = 64 * 1024;

/// Longest path this will walk, and longest single component.
const MAX_PATH: u64 = 4096;
const MAX_COMPONENT: usize = 255;

/// Where each open descriptor has got to, keyed by process and handle.
///
/// Keyed by both because a handle number is only unique within a process, and
/// process identifiers are never reused -- so a stale entry can name nothing
/// rather than somebody else's file.
static POSITIONS: IrqSpinLock<BTreeMap<(u64, u32), u64>> = IrqSpinLock::new(BTreeMap::new());

/// Turn a store failure into the Linux error that means the same thing.
///
/// Not a single catch-all: a program that cannot tell "no such file" from "the
/// disk is broken" retries the wrong one.
fn errno(error: StoreError) -> u64 {
    use crate::fs::nexusfs::FsError;
    match error {
        StoreError::Fs(FsError::NotFound) => ENOENT,
        StoreError::Fs(FsError::Exists) => EEXIST,
        StoreError::Fs(FsError::WrongKind) => ENOTDIR,
        StoreError::Fs(FsError::BadName) => ENAMETOOLONG,
        StoreError::Fs(FsError::NotEmpty) => (-39i64) as u64, // ENOTEMPTY
        StoreError::Fs(FsError::NoSpace | FsError::NoInodes | FsError::TooLarge) => {
            (-28i64) as u64 // ENOSPC
        }
        // A machine with no disk, an unreadable partition table, a filesystem
        // that will not mount, a corrupt inode: all of them are the medium
        // failing, which is what EIO means and is not the same answer as "there
        // is no such file". A program told ENOENT for a broken disk goes on to
        // create the file.
        _ => (-5i64) as u64, // EIO
    }
}

/// The handful of Linux errors this file names more than once.
const ENOENT: u64 = (-2i64) as u64;
const EEXIST: u64 = (-17i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const EISDIR: u64 = (-21i64) as u64;
/// Illegal seek: what a positional read of a pipe gets.
const ESPIPE: u64 = (-29i64) as u64;
/// Broken pipe: nothing is reading the other end.
const EPIPE: u64 = (-32i64) as u64;
/// Interrupted: the thread was asked to stop while it waited.
const EINTR: u64 = (-4i64) as u64;

/// The most one write to the log will take.
///
/// A translated write ends up in the boot log, which is a serial line. A
/// program that asked to write a megabyte would be a program that stopped the
/// machine for a second, so the count is capped and the *short write* is
/// reported honestly -- which every caller of `write` already handles, because
/// on Linux it happens too.
const MAX_CONSOLE: u64 = 512;
const ENAMETOOLONG: u64 = (-36i64) as u64;
const EACCES: u64 = (-13i64) as u64;

/// The directory a translated program sees as `/`.
///
/// Made if it is not there. A machine that has never run a Linux program has no
/// reason to carry the directory, and making it on demand means the first
/// program to ask is the thing that creates it rather than a step in the build
/// that everyone has to remember.
fn root() -> Result<Arc<Node>, u64> {
    let top = store::root().map_err(errno)?;
    match store::open_child(&top, "linux") {
        Ok(node) if node.is_directory() => Ok(node),
        // A *file* called `linux` is not a root, and silently using the store's
        // own root instead would put a foreign program's files among this
        // machine's.
        Ok(_) => Err(ENOTDIR),
        Err(StoreError::Fs(crate::fs::nexusfs::FsError::NotFound)) => {
            store::create_child(&top, "linux", true).map_err(errno)
        }
        Err(error) => Err(errno(error)),
    }
}

/// Read a path out of the caller's memory.
fn path_of(pointer: u64) -> Result<String, u64> {
    // The length is not given, so it has to be found -- and looked for only
    // inside the user half, one byte at a time, stopping at the cap. A path
    // with no terminator must end as a refusal and not as a walk off the end of
    // the address space.
    let mut bytes: Vec<u8> = Vec::new();
    for step in 0..MAX_PATH {
        let Some((at, _)) = crate::arch::syscall::user_range(pointer + step, 1, 1) else {
            return Err(error::EFAULT);
        };
        // SAFETY: the byte was checked to lie inside the user half, which this
        // thread's address space maps. An unmapped byte faults, which is the
        // caller's own page fault.
        let byte = unsafe { core::ptr::read_volatile(at as *const u8) };
        if byte == 0 {
            return String::from_utf8(bytes).map_err(|_| error::EINVAL);
        }
        bytes.push(byte);
    }
    Err(ENAMETOOLONG)
}

/// Walk a path from the Linux root and open what it names.
///
/// Absolute and relative are the same walk, because the working directory is
/// the root and does not move. `.` is skipped and `..` is refused: a directory
/// handle is the authority to reach what is under it, and a path that could
/// climb out of the subtree would make that authority mean nothing.
fn walk(path: &str) -> Result<Arc<Node>, u64> {
    let mut node = root()?;
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(EACCES);
        }
        if component.len() > MAX_COMPONENT {
            return Err(ENAMETOOLONG);
        }
        node = store::open_child(&node, component).map_err(errno)?;
    }
    Ok(node)
}

/// Split a path into the directory holding the last name, and that name.
fn split(path: &str) -> Result<(Arc<Node>, String), u64> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(error::EINVAL);
    }
    Ok((walk(parent)?, String::from(name)))
}

/// The calling process, or the error a call outside one gets.
fn process() -> Result<Arc<crate::process::Process>, u64> {
    crate::sched::current_process().ok_or(error::EBADF)
}

/// The node a descriptor names, and where it has got to.
fn opened(descriptor: u64) -> Result<(Arc<crate::process::Process>, u32, Arc<Node>), u64> {
    if descriptor <= STDERR || descriptor > u64::from(u32::MAX) {
        return Err(error::EBADF);
    }
    let process = process()?;
    let handle = descriptor as u32;
    let node = process
        .handles
        .node(handle, crate::ipc::Rights::READ)
        .map_err(|_| error::EBADF)?;
    Ok((process, handle, node))
}

/// What a descriptor turned out to name.
///
/// A Linux program's descriptors are not all the same kind of thing, and the
/// three it starts with are not handles at all. Asking once and matching is one
/// lock acquisition; asking "is it a file?", then "is it a pipe?", then "is it
/// the console?" and taking whichever succeeded would be three, to learn one
/// thing.
pub(super) enum Descriptor {
    /// The machine's log: 0, 1 or 2, or a duplicate of one of them.
    Console(u8),
    /// A file or directory, and the handle number its position is kept under.
    File(Arc<Node>, u32),
    /// One end of a pipe.
    Pipe(Arc<crate::pipe::PipeEnd>),
    /// Something else this layer holds a handle to -- a window's channel, a
    /// wait set. Named rather than refused here, because the caller knows
    /// whether that is an error for what it is doing.
    Other(crate::ipc::Object),
}

/// Work out what a descriptor is.
///
/// # Errors
///
/// `EBADF` if it names nothing.
pub(super) fn describe(descriptor: u64) -> Result<Descriptor, u64> {
    // The three a program starts with, which are a convention of this layer
    // rather than handles: see the note at the top of the file.
    if descriptor <= STDERR {
        return Ok(Descriptor::Console(descriptor as u8));
    }
    if descriptor > u64::from(u32::MAX) {
        return Err(error::EBADF);
    }
    let process = process()?;
    let handle = descriptor as u32;
    let object = process
        .handles
        .object(handle, crate::ipc::Rights::NONE)
        .map_err(|_| error::EBADF)?;
    Ok(match object {
        crate::ipc::Object::Node(node) => Descriptor::File(node, handle),
        crate::ipc::Object::Pipe(end) => Descriptor::Pipe(end),
        // An empty `memfd` is parked under a console number that is not a
        // stream, so that it names something until `ftruncate` gives it
        // memory. Reported as what it is rather than as a console.
        crate::ipc::Object::Console(console) if console.stream == EMPTY_MEMFD => {
            Descriptor::Other(crate::ipc::Object::Console(console))
        }
        crate::ipc::Object::Console(console) => Descriptor::Console(console.stream),
        other => Descriptor::Other(other),
    })
}

/// Write bytes to the machine's log, as one of a program's streams.
///
/// Where a translated program's standard output goes. The trailing newline is
/// dropped because the log adds its own, and a program that wrote one would
/// otherwise get a blank line after every message.
pub(super) fn to_console(stream: u8, bytes: &[u8]) -> u64 {
    if u64::from(stream) != STDOUT && u64::from(stream) != STDERR {
        // Standard input is not somewhere to write, and saying so is what
        // Linux does.
        return error::EBADF;
    }
    let text = alloc::string::String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    let name = crate::sched::current_process().map_or_else(
        || String::from("?"),
        |process| String::from(process.name.as_str()),
    );
    crate::kprintln!("[linux] {name} wrote: {text}");
    bytes.len() as u64
}

/// `dup(fd)`: a second descriptor for the same thing.
///
/// The number is this layer's choice, as it is on Linux -- the lowest free one
/// there, the next one here, and neither is something a program may rely on.
pub fn dup(descriptor: u64) -> u64 {
    let Ok(process) = process() else {
        return error::EBADF;
    };
    match duplicable(descriptor) {
        Ok(Duplicable::Console(stream)) => {
            // A duplicate of the log. It has to be a real handle, because a
            // descriptor is a handle here and a number this layer merely
            // remembered would be one nothing else could look up -- which is
            // exactly why `Object::Console` exists.
            u64::from(process.handles.insert(
                crate::ipc::Object::Console(crate::ipc::Console { stream }),
                crate::ipc::Rights::READ | crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE,
            ))
        }
        Ok(Duplicable::Handle(handle)) => {
            let Ok(rights) = process.handles.rights(handle) else {
                return error::EBADF;
            };
            match process.handles.duplicate(handle, rights) {
                Ok(copy) => {
                    // A file's position is per descriptor here. Linux shares
                    // one between a descriptor and its duplicate, and that
                    // difference is real: two descriptors from `dup` there move
                    // one cursor. Copying the position is the closer of the two
                    // wrong answers -- the duplicate starts where the original
                    // is, rather than at the beginning.
                    let at = position(&process, handle);
                    set_position(&process, copy, at);
                    u64::from(copy)
                }
                Err(_) => error::EBADF,
            }
        }
        Err(reason) => reason,
    }
}

/// `dup2(from, to)` and `dup3(from, to, flags)`.
///
/// The caller chooses the number, which is the whole reason a shell can
/// redirect: it puts the descriptor it wants at the number the program is about
/// to use.
pub fn dup2(from: u64, to: u64) -> u64 {
    // Linux's one special case, and it is worth keeping: duplicating a
    // descriptor onto itself is a no-op that returns it, rather than a close
    // followed by a copy of something that is no longer there.
    if from == to {
        return match describe(from) {
            Ok(_) => to,
            Err(reason) => reason,
        };
    }
    if to > u64::from(u32::MAX) || to <= STDERR {
        // Replacing one of the three standard descriptors would mean replacing
        // a convention rather than a handle. A program that redirected its own
        // standard output this way is doing something reasonable and this layer
        // cannot yet honour it, so it is refused rather than ignored.
        return error::EINVAL;
    }
    let Ok(process) = process() else {
        return error::EBADF;
    };
    match duplicable(from) {
        Ok(Duplicable::Console(stream)) => {
            let rights =
                crate::ipc::Rights::READ | crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE;
            // There is no `insert_at` for a fresh object, so one is made and
            // then moved: `duplicate_at` is the operation that displaces
            // whatever was at the target.
            let made = process.handles.insert(
                crate::ipc::Object::Console(crate::ipc::Console { stream }),
                rights,
            );
            match process.handles.duplicate_at(made, to as u32, rights) {
                Ok(at) => {
                    let _ = process.handles.close(made);
                    u64::from(at)
                }
                Err(_) => {
                    let _ = process.handles.close(made);
                    error::EBADF
                }
            }
        }
        Ok(Duplicable::Handle(handle)) => {
            let Ok(rights) = process.handles.rights(handle) else {
                return error::EBADF;
            };
            match process.handles.duplicate_at(handle, to as u32, rights) {
                Ok(at) => {
                    POSITIONS.lock().remove(&(process.id.0, to as u32));
                    let was = position(&process, handle);
                    set_position(&process, to as u32, was);
                    u64::from(at)
                }
                Err(_) => error::EBADF,
            }
        }
        Err(reason) => reason,
    }
}

/// What a descriptor is, for the purpose of copying it.
enum Duplicable {
    Console(u8),
    Handle(u32),
}

fn duplicable(descriptor: u64) -> Result<Duplicable, u64> {
    match describe(descriptor)? {
        Descriptor::Console(stream) => Ok(Duplicable::Console(stream)),
        _ => u32::try_from(descriptor)
            .map(Duplicable::Handle)
            .map_err(|_| error::EBADF),
    }
}

/// `pipe2(ends, flags)`: a stream of bytes with two ends.
///
/// The two descriptors go into the caller's memory as two `int`s, reading end
/// first -- the order is the ABI's and is the one every program relies on.
///
/// Both are real Nexus handles to the two ends of a [`crate::pipe::Pipe`],
/// which is an object of this system rather than a Linux one: see the note at
/// the top of that module for why a channel could not have been used.
pub fn pipe2(ends: u64, flags: u64) -> u64 {
    /// `O_CLOEXEC`, which every current program passes. There is no `execve`
    /// that keeps descriptors yet, so there is nothing for it to change --
    /// accepted rather than refused, because refusing would stop programs that
    /// are not relying on it.
    const CLOEXEC: u64 = 0o2_000_000;
    /// `O_NONBLOCK`, which changes what a read of an empty pipe does. Refused
    /// rather than ignored: a program that asked for it and got a blocking
    /// pipe would hang in a place it had specifically arranged not to.
    const NONBLOCK: u64 = 0o4000;

    if flags & !CLOEXEC != 0 {
        if flags & NONBLOCK != 0 {
            crate::kprintln!("[linux] a non-blocking pipe is not translated yet");
        }
        return error::EINVAL;
    }
    let Ok(process) = process() else {
        return error::EBADF;
    };
    let Some((pointer, _)) = crate::arch::syscall::user_range(ends, 8, 8) else {
        return error::EFAULT;
    };

    let (reading, writing) = crate::pipe::Pipe::pair();
    // The reading end cannot be written and the writing end cannot be read, and
    // that is the handle's rights rather than a flag this layer remembers --
    // the same rule `openat` follows for a file opened read-only.
    //
    // `TRANSFER` as well, because a Linux program may send any descriptor it
    // holds over a socket with `SCM_RIGHTS` -- and a pipe is the commonest
    // thing to send. Without it the handle table refuses to let go of it and
    // the send fails, which is what it did: `sendmsg` answered `EBADF` for a
    // descriptor the program had just been given.
    let read_handle = process.handles.insert(
        crate::ipc::Object::Pipe(reading),
        crate::ipc::Rights::READ | crate::ipc::Rights::CLOSE | crate::ipc::Rights::TRANSFER,
    );
    let write_handle = process.handles.insert(
        crate::ipc::Object::Pipe(writing),
        crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE | crate::ipc::Rights::TRANSFER,
    );

    // SAFETY: eight bytes inside the user half, checked above, written as the
    // two `int` the ABI says they are.
    unsafe {
        core::ptr::write_unaligned(
            pointer as *mut [i32; 2],
            [read_handle as i32, write_handle as i32],
        );
    }
    0
}

/// `memfd_create(name, flags)`: memory with a descriptor on it.
///
/// A file that is not on any filesystem. Its whole purpose is to be *passed*:
/// a program makes one, maps it, fills it, sends the descriptor to another
/// program over a socket, and both of them are then looking at the same bytes.
///
/// That is how every graphics protocol on Linux moves a frame. A Wayland
/// client's `wl_shm_pool` is a `memfd`; an X client's `MIT-SHM` segment is the
/// older version of the same idea. Without it a client can talk to a
/// compositor and cannot show it anything, because a megabyte of pixels is not
/// something to send down a socket a byte at a time.
///
/// Here it is a [`crate::ipc::MemoryObject`], which is what this system already
/// calls memory more than one process can see -- the same object the compositor
/// hands a window its surface in. The descriptor is a handle to it, so passing
/// it over a socket is the ordinary handle transfer, and mapping it is the
/// ordinary `memory_map`.
///
/// It starts with no size. `ftruncate` gives it one, once: see below.
pub fn memfd_create(name: u64, flags: u64) -> u64 {
    /// `MFD_CLOEXEC`, which every caller passes and which nothing here acts on
    /// yet -- there is no `execve` that closes descriptors.
    const CLOEXEC: u64 = 0x0001;
    /// `MFD_ALLOW_SEALING`. Sealing is a promise about what may be done to the
    /// memory later, and a promise this cannot keep is one to refuse.
    const ALLOW_SEALING: u64 = 0x0002;

    if flags & !CLOEXEC != 0 {
        if flags & ALLOW_SEALING != 0 {
            crate::kprintln!("[linux] a sealable memfd is not translated; answering EINVAL");
        }
        return error::EINVAL;
    }
    // The name is for `/proc/self/fd`, which does not exist here. Read anyway,
    // because a bad pointer is a bad pointer whether or not the value is used.
    if name != 0 && path_of(name).is_err() {
        return error::EFAULT;
    }
    let Ok(process) = process() else {
        return error::EBADF;
    };
    // An empty one. `ftruncate` is what gives it a size, and until then there
    // is nothing to map -- which is exactly what a `memfd` is before its owner
    // has said how large it should be.
    let descriptor = process.handles.insert(
        crate::ipc::Object::Console(crate::ipc::Console {
            stream: EMPTY_MEMFD,
        }),
        crate::ipc::Rights::READ | crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE,
    );
    EMPTY.lock().insert((process.id.0, descriptor));
    u64::from(descriptor)
}

/// The stream number an empty `memfd` is parked under.
///
/// Not a real stream: a descriptor has to name *something*, and a `memfd` that
/// has not been given a size has no memory to name yet. Anything reaching it
/// through the console path is refused, because [`to_console`] accepts only
/// standard output and standard error.
const EMPTY_MEMFD: u8 = 200;

/// The descriptors that are `memfd`s with no size yet.
///
/// Kept so that `ftruncate` can tell one from an ordinary descriptor: giving a
/// size to something that is not a `memfd` is a different operation with a
/// different answer.
static EMPTY: IrqSpinLock<alloc::collections::BTreeSet<(u64, u32)>> =
    IrqSpinLock::new(alloc::collections::BTreeSet::new());

/// `ftruncate(fd, length)`.
///
/// For a `memfd`, the call that gives it its memory. Once: a `MemoryObject` is
/// a fixed number of frames, and growing one would mean moving it out from
/// under everything that has it mapped. A second `ftruncate` to the same size
/// succeeds and does nothing, because that is what a program checking its work
/// does; a different size is refused.
///
/// For a file, refused. Truncating a file on the store is a real operation and
/// this is not it.
pub fn ftruncate(descriptor: u64, length: u64) -> u64 {
    let Ok(process) = process() else {
        return error::EBADF;
    };
    let Ok(handle) = u32::try_from(descriptor) else {
        return error::EBADF;
    };
    let empty = EMPTY.lock().contains(&(process.id.0, handle));
    if !empty {
        // Already given a size. The same size again is what a careful program
        // does; a different one would move memory somebody has mapped.
        if let Ok(crate::ipc::Object::Memory(memory)) =
            process.handles.object(handle, crate::ipc::Rights::READ)
        {
            return if memory.size() as u64 == length {
                0
            } else {
                crate::kprintln!("[linux] a memfd cannot be resized once it has memory");
                error::EINVAL
            };
        }
        return error::EINVAL;
    }
    if length == 0 {
        return error::EINVAL;
    }

    let Some(memory) = crate::ipc::MemoryObject::new(length as usize) else {
        return error::ENOMEM;
    };
    let rights = crate::ipc::Rights::READ
        | crate::ipc::Rights::WRITE
        | crate::ipc::Rights::CLOSE
        | crate::ipc::Rights::TRANSFER;
    // Made under a number of its own and then moved onto the caller's, because
    // the caller already has that number and a program's descriptor must not
    // change under it.
    let made = process
        .handles
        .insert(crate::ipc::Object::Memory(memory), rights);
    let placed = process.handles.duplicate_at(made, handle, rights);
    let _ = process.handles.close(made);
    if placed.is_err() {
        return error::ENOMEM;
    }
    EMPTY.lock().remove(&(process.id.0, handle));
    0
}

/// Forget a process's empty `memfd`s. Called when it ends.
pub fn forget_memfds(process: u64) {
    EMPTY.lock().retain(|(owner, _)| *owner != process);
}

/// The node a descriptor names, for something that is not a read or a write.
///
/// `mmap` of a file needs this: it does not move a cursor and does not go
/// through the caller's buffer, so it needs the node and nothing else. The
/// rights check is the same one every other use of a descriptor makes -- a
/// mapping is a read, so the handle has to carry `READ`, and a descriptor
/// opened write-only cannot be mapped.
pub(super) fn node_of(descriptor: u64) -> Result<Arc<Node>, u64> {
    let (_, _, node) = opened(descriptor)?;
    if node.is_directory() {
        return Err(EISDIR);
    }
    Ok(node)
}

/// Read a whole file from the Linux root, by path, for the kernel itself.
///
/// The loader needs this and cannot use `openat`: there is no process yet whose
/// handle table an interpreter's descriptor could go in, and the program that
/// is about to be started must not be given a descriptor it never opened.
///
/// It walks the same subtree every translated program's paths walk, through the
/// same `walk`, so an interpreter is reachable exactly when a program could
/// have opened it by name -- and `..` is refused here as it is everywhere else.
///
/// # Errors
///
/// The Linux error number for whatever stopped it, so a caller can report the
/// same reason a program would have been given.
pub fn read_file(path: &str) -> Result<Vec<u8>, u64> {
    let node = walk(path)?;
    if node.is_directory() {
        return Err(EISDIR);
    }
    store::read_node(&node).map_err(errno)
}

/// Put a file into the Linux root, making the directories above it.
///
/// A dynamically linked program names its interpreter by an absolute path, and
/// that path has to lead somewhere. Until a package installer exists, this is
/// how anything gets into the Linux root at all: something with the bytes and a
/// path calls this.
///
/// Every component is created as a directory if it is not there, which `openat`
/// deliberately does not do -- a program that opens `/a/b/c` with `O_CREAT`
/// wants the file and not three directories, and creating them would turn a
/// misspelt path into a tree. Installing is the other case: the caller is
/// placing a file at a path it chose, and the directories are part of what it
/// chose.
///
/// An existing file is replaced. That is what installing over something means,
/// and refusing would make a second boot fail where the first succeeded.
///
/// # Errors
///
/// The Linux error number for whatever stopped it.
pub fn install(path: &str, bytes: &[u8]) -> Result<(), u64> {
    let (directories, name) = match path.trim_end_matches('/').rsplit_once('/') {
        Some(split) => split,
        None => ("", path),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(error::EINVAL);
    }

    let mut node = root()?;
    for component in directories.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(EACCES);
        }
        node = match store::open_child(&node, component) {
            Ok(found) if found.is_directory() => found,
            // A file where a directory has to be is a refusal, not something to
            // replace: whatever is there was put there on purpose.
            Ok(_) => return Err(ENOTDIR),
            Err(StoreError::Fs(crate::fs::nexusfs::FsError::NotFound)) => {
                store::create_child(&node, component, true).map_err(errno)?
            }
            Err(error) => return Err(errno(error)),
        };
    }

    let file = match store::open_child(&node, name) {
        Ok(found) if found.is_directory() => return Err(EISDIR),
        Ok(found) => found,
        Err(StoreError::Fs(crate::fs::nexusfs::FsError::NotFound)) => {
            store::create_child(&node, name, false).map_err(errno)?
        }
        Err(error) => return Err(errno(error)),
    };
    store::write_node(&file, bytes).map_err(errno)
}

/// Whether a path in the Linux root already names a file.
///
/// Asked before installing, so a boot that has already installed something does
/// not rewrite it -- which would mean every boot wrote a few megabytes to the
/// disk to arrive at exactly what was there.
#[must_use]
pub fn exists(path: &str) -> bool {
    walk(path).is_ok_and(|node| !node.is_directory())
}

/// Read a descriptor's position.
fn position(process: &crate::process::Process, handle: u32) -> u64 {
    *POSITIONS.lock().get(&(process.id.0, handle)).unwrap_or(&0)
}

/// Set one.
fn set_position(process: &crate::process::Process, handle: u32, to: u64) {
    POSITIONS.lock().insert((process.id.0, handle), to);
}

/// Forget every position a process held.
///
/// Called when a process ends. Without it the map grows by one entry for every
/// file every translated program has ever opened, for the life of the machine.
pub fn forget(process: u64) {
    POSITIONS.lock().retain(|(owner, _), _| *owner != process);
}

/// How many descriptors are being tracked, for the monitor.
#[must_use]
pub fn open_count() -> usize {
    POSITIONS.lock().len()
}

// ---------------------------------------------------------------------------
// The calls
// ---------------------------------------------------------------------------

/// `openat(dirfd, path, flags, mode)`.
///
/// `dirfd` must be `AT_FDCWD`: opening relative to another descriptor needs a
/// directory handle to walk from, and this layer's working directory is the
/// root, so the two are the same walk. A real `dirfd` is refused rather than
/// quietly treated as the root, because those differ exactly when a program is
/// relying on the difference.
pub fn openat(directory: u64, pointer: u64, flags: u64) -> u64 {
    if !is_cwd(directory) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };

    // A device rather than a file. Checked here, before the walk, because the
    // walk would create `dev/nexus/display` as an ordinary empty file the first
    // time a program opened it with `O_CREAT` -- and then every boot after that
    // would find a file there and open it, and the program would draw into the
    // disk.
    if super::linux_display::is_device(&path) {
        return super::linux_display::open();
    }

    let writing = flags & (flag::WRONLY | flag::RDWR) != 0;
    let node = match walk(&path) {
        Ok(node) => Some(node),
        // Missing is only an error if the caller did not ask for it to be made.
        Err(code) if code == ENOENT && flags & flag::CREAT != 0 => None,
        // A missing file is not news: a program looking for its configuration in
        // four places finds nothing in three of them. Anything else is, and is
        // said below where it happens.
        Err(code) => return code,
    };

    let node = match node {
        Some(node) => {
            // `O_EXCL` means the caller wants to be the one that made it.
            if flags & flag::CREAT != 0 && flags & flag::EXCL != 0 {
                return EEXIST;
            }
            if flags & flag::DIRECTORY != 0 && !node.is_directory() {
                return ENOTDIR;
            }
            if node.is_directory() && writing {
                return EISDIR;
            }
            // Emptied here rather than at the first write, because a program
            // that opens with `O_TRUNC` and then never writes still expects to
            // find nothing there.
            if flags & flag::TRUNC != 0 && !node.is_directory() {
                if let Err(error) = store::write_node(&node, &[]) {
                    return errno(error);
                }
            }
            node
        }
        None => {
            let (parent, name) = match split(&path) {
                Ok(split) => split,
                Err(code) => {
                    crate::kprintln!(
                        "[linux] openat {path:?} refused: split gave {}",
                        code as i64
                    );
                    return code;
                }
            };
            match store::create_child(&parent, &name, flags & flag::DIRECTORY != 0) {
                Ok(node) => node,
                Err(error) => {
                    crate::kprintln!("[linux] openat {path:?} could not create: {error}");
                    return errno(error);
                }
            }
        }
    };

    // Writable only if it was asked for, so a descriptor opened for reading
    // cannot be written through -- the same rights check a Nexus handle gets,
    // rather than a flag this layer remembers and might forget to consult.
    //
    // `TRANSFER` in both cases, for the reason `pipe2` grants it: a Linux
    // program may hand any descriptor it holds to another program over a
    // socket, and a handle that could not be handed on would be a descriptor
    // that could not.
    let rights = if writing {
        crate::ipc::Rights::READ
            | crate::ipc::Rights::WRITE
            | crate::ipc::Rights::CLOSE
            | crate::ipc::Rights::TRANSFER
    } else {
        crate::ipc::Rights::READ | crate::ipc::Rights::CLOSE | crate::ipc::Rights::TRANSFER
    };

    let process = match process() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let handle = process
        .handles
        .insert(crate::ipc::Object::Node(node.clone()), rights);

    // Appending starts at the end, which is what `O_APPEND` means for the very
    // first write as much as for the ones after it.
    let start = if flags & flag::APPEND != 0 {
        store::size(&node).unwrap_or(0)
    } else {
        0
    };
    set_position(&process, handle, start);
    u64::from(handle)
}

/// `close(fd)`.
///
/// The three a program starts with are not handles and closing one is accepted
/// and does nothing: a program that closes its standard error before `exec`
/// must not be told it had none.
pub fn close(descriptor: u64) -> u64 {
    // A window goes back to the compositor when the program lets go of it. The
    // handle is closed below like any other -- this only drops what this layer
    // was remembering about it.
    if let (Ok(process), Ok(id)) = (process(), u32::try_from(descriptor)) {
        super::linux_display::close(process.id.0, id);
    }
    if descriptor <= STDERR {
        return 0;
    }
    let Ok(process) = process() else {
        return error::EBADF;
    };
    let Ok(handle) = u32::try_from(descriptor) else {
        return error::EBADF;
    };
    POSITIONS.lock().remove(&(process.id.0, handle));
    super::linux_poll::forget_descriptor(process.id.0, handle);
    // Closed whatever it is -- a file, a pipe end, the console, a window's
    // channel. Dropping the last handle to a pipe end is what tells the other
    // side there is nothing more coming, so this is not merely bookkeeping.
    match process.handles.close(handle) {
        Ok(()) => 0,
        Err(_) => error::EBADF,
    }
}

/// `read(fd, buffer, count)`.
pub fn read(descriptor: u64, buffer: u64, count: u64) -> u64 {
    read_at(descriptor, buffer, count, None)
}

/// Positional reads used by ELF loaders: do not change the descriptor cursor.
pub fn pread64(descriptor: u64, buffer: u64, count: u64, offset: u64) -> u64 {
    if (offset as i64) < 0 {
        return error::EINVAL;
    }
    read_at(descriptor, buffer, count, Some(offset))
}

fn read_at(descriptor: u64, buffer: u64, count: u64, offset: Option<u64>) -> u64 {
    // What the descriptor is decides what reading it means. A positional read
    // is only meaningful for a file: a pipe has no positions, and the console
    // has nothing behind it to seek in.
    let (node, handle) = match describe(descriptor) {
        // Standard input reads as end of file. The keyboard belongs to the
        // compositor, which hands keys to windows, and a program with no window
        // has none coming -- so zero is the truth, and a program that reads
        // gets nothing and stops rather than blocking for ever.
        Ok(Descriptor::Console(stream)) => {
            return if stream == STDIN as u8 {
                0
            } else {
                error::EBADF
            };
        }
        Ok(Descriptor::Pipe(end)) => {
            return if offset.is_some() {
                ESPIPE
            } else {
                read_pipe(&end, buffer, count)
            };
        }
        Ok(Descriptor::File(node, handle)) => (node, handle),
        // A socket, read with `read` rather than `recv`. Every program that
        // speaks a protocol over one does this: a socket is a descriptor, and
        // the whole point of a descriptor is that the code using it does not
        // have to know what it leads to.
        Ok(Descriptor::Other(crate::ipc::Object::Socket(_))) => {
            return if offset.is_some() {
                ESPIPE
            } else {
                super::linux_socket::receive(descriptor, buffer, count, 0)
            };
        }
        Ok(Descriptor::Other(_)) => return error::EBADF,
        Err(reason) => return reason,
    };
    let Ok(process) = process() else {
        return error::EBADF;
    };
    if process
        .handles
        .node(handle, crate::ipc::Rights::READ)
        .is_err()
    {
        return error::EBADF;
    }
    if node.is_directory() {
        return EISDIR;
    }
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };

    let at = offset.unwrap_or_else(|| position(&process, handle));
    let mut held = alloc::vec![0u8; length];
    let taken = match store::read_node_at(&node, at, &mut held) {
        Ok(taken) => taken,
        Err(error) => return errno(error),
    };

    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps, and `taken` is no larger than it.
    unsafe {
        core::ptr::copy_nonoverlapping(held.as_ptr(), pointer as *mut u8, taken);
    }
    if offset.is_none() {
        set_position(&process, handle, at + taken as u64);
    }
    taken as u64
}

/// `write(fd, buffer, count)`, whatever the descriptor turns out to be.
///
/// A file, a pipe, or the machine's log. One entry point rather than three,
/// because a program does not know which of them it has -- a shell that piped
/// one command into another handed the second one a descriptor, and whether it
/// leads to a pipe or to the terminal is exactly the thing the second command
/// is not supposed to care about.
pub fn write(descriptor: u64, buffer: u64, count: u64) -> u64 {
    let (node, handle) = match describe(descriptor) {
        Ok(Descriptor::Console(stream)) => return write_console(stream, buffer, count),
        Ok(Descriptor::Pipe(end)) => return write_pipe(&end, buffer, count),
        Ok(Descriptor::File(node, handle)) => (node, handle),
        Ok(Descriptor::Other(crate::ipc::Object::Socket(_))) => {
            return super::linux_socket::send(descriptor, buffer, count, 0);
        }
        Ok(Descriptor::Other(_)) => return error::EBADF,
        Err(reason) => return reason,
    };
    let Ok(process) = process() else {
        return error::EBADF;
    };
    // Writability is the handle's, not a flag remembered here.
    if process
        .handles
        .node(handle, crate::ipc::Rights::WRITE)
        .is_err()
    {
        return (-9i64) as u64; // EBADF, which is what Linux gives for this
    }
    if node.is_directory() {
        return EISDIR;
    }
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };

    // SAFETY: as in `read`.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    let at = position(&process, handle);
    match store::write_node_at(&node, at, bytes) {
        Ok(written) => {
            set_position(&process, handle, at + written);
            written
        }
        Err(error) => errno(error),
    }
}

/// Read from a pipe into the caller's memory.
///
/// Blocks while the pipe is empty and a writer could still arrive, which is
/// what a pipe is for -- and returns zero when every write end has gone, which
/// is what end of file is.
fn read_pipe(end: &Arc<crate::pipe::PipeEnd>, buffer: u64, count: u64) -> u64 {
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };
    // Read into the kernel's own memory first and copied out after. Reading
    // straight into the caller's would mean holding the pipe's lock across a
    // write to a page that may fault.
    match end.read(length) {
        Ok(bytes) => {
            // SAFETY: the range was checked to lie inside the user half, and
            // the pipe never returns more than it was asked for.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            }
            bytes.len() as u64
        }
        Err(crate::pipe::PipeError::WrongEnd) => error::EBADF,
        Err(crate::pipe::PipeError::Cancelled) => EINTR,
        Err(_) => error::EBADF,
    }
}

/// Write into a pipe.
fn write_pipe(end: &Arc<crate::pipe::PipeEnd>, buffer: u64, count: u64) -> u64 {
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };
    // SAFETY: as in `read_pipe`.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    // Copied before the pipe's lock is taken, for the same reason.
    let held = alloc::vec::Vec::from(bytes);
    match end.write(&held) {
        Ok(written) => written as u64,
        Err(crate::pipe::PipeError::WrongEnd) => error::EBADF,
        // On Linux this also raises `SIGPIPE`, which kills the program unless
        // it has said otherwise. There are no signals here, so a program gets
        // the error and carries on -- which is what a program that blocked
        // `SIGPIPE` would see, and is the safer of the two differences.
        Err(crate::pipe::PipeError::NoReader) => EPIPE,
        Err(crate::pipe::PipeError::Cancelled) => EINTR,
    }
}

/// Write to the machine's log.
fn write_console(stream: u8, buffer: u64, count: u64) -> u64 {
    if stream == STDIN as u8 {
        return error::EBADF;
    }
    let wanted = count.min(MAX_CONSOLE);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_CONSOLE)
    else {
        return error::EFAULT;
    };
    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    to_console(stream, bytes)
}

/// `lseek(fd, offset, whence)`.
pub fn lseek(descriptor: u64, offset: u64, whence: u64) -> u64 {
    let (process, handle, node) = match opened(descriptor) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let offset = offset as i64;
    let from = match whence {
        seek::SET => 0i64,
        seek::CUR => position(&process, handle) as i64,
        seek::END => match store::size(&node) {
            Ok(size) => size as i64,
            Err(error) => return errno(error),
        },
        _ => return error::EINVAL,
    };
    // A position before the start is an error; one past the end is allowed,
    // because that is how a sparse file is written on Linux and a program that
    // does it is not making a mistake.
    let Some(to) = from.checked_add(offset) else {
        return error::EINVAL;
    };
    if to < 0 {
        return error::EINVAL;
    }
    set_position(&process, handle, to as u64);
    to as u64
}

/// Bytes in the `struct stat` x86-64 Linux passes back.
const STAT_SIZE: u64 = 144;

/// Fill one in.
///
/// The shape is the ABI's, not a choice: a program reads `st_size` at offset 48
/// because that is where the C library's header says it is. Fields this machine
/// has no answer for are zero, which is what a program reading them will see
/// and is better than a plausible invention -- a fabricated modification time
/// makes `make` rebuild nothing.
fn fill_stat(node: &Node, out: u64) -> u64 {
    let Some((pointer, _)) = crate::arch::syscall::user_range(out, STAT_SIZE, STAT_SIZE) else {
        return error::EFAULT;
    };
    let size = store::size(node).unwrap_or(0);

    /// `S_IFDIR | 0755` and `S_IFREG | 0644`.
    const DIRECTORY: u32 = 0o040_000 | 0o755;
    const FILE: u32 = 0o100_000 | 0o644;

    let mut fields = [0u8; STAT_SIZE as usize];
    // st_nlink at 16: one, and for a directory two, because `.` counts. A
    // program that walks a tree by subtracting two from the link count would
    // otherwise conclude every directory has an impossible number of children.
    let links: u64 = if node.is_directory() { 2 } else { 1 };
    fields[16..24].copy_from_slice(&links.to_le_bytes());
    let mode = if node.is_directory() { DIRECTORY } else { FILE };
    fields[24..28].copy_from_slice(&mode.to_le_bytes());
    fields[48..56].copy_from_slice(&size.to_le_bytes());
    // st_blksize, which is what a program sizes its read buffer by.
    fields[56..64].copy_from_slice(&4096u64.to_le_bytes());
    fields[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes());

    // SAFETY: the range was checked to lie inside the user half and is exactly
    // the size of what is being written into it.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr(), pointer as *mut u8, fields.len());
    }
    0
}

/// `fstat(fd, statbuf)`.
pub fn fstat(descriptor: u64, out: u64) -> u64 {
    match opened(descriptor) {
        Ok((_, _, node)) => fill_stat(&node, out),
        Err(error) => error,
    }
}

/// `newfstatat(dirfd, path, statbuf, flags)`, which is also `stat` and `lstat`.
///
/// There are no symbolic links in this store, so `lstat` and `stat` cannot
/// differ and `AT_SYMLINK_NOFOLLOW` changes nothing. That is a fact about the
/// filesystem rather than a corner cut.
pub fn newfstatat(directory: u64, pointer: u64, out: u64) -> u64 {
    if !is_cwd(directory) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    match walk(&path) {
        Ok(node) => fill_stat(&node, out),
        Err(error) => error,
    }
}

/// `faccessat(dirfd, path, mode, flags)` and `access(path, mode)`.
///
/// Existence is the only question this can answer. There is one user on this
/// machine and no permission bits in the store, so a file that is there is
/// readable and writable, and saying otherwise would be inventing a refusal.
pub fn faccessat(directory: u64, pointer: u64) -> u64 {
    if !is_cwd(directory) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    match walk(&path) {
        Ok(_) => 0,
        Err(error) => error,
    }
}

/// `getcwd(buffer, size)`.
///
/// Always `/`. Returns the length including the terminator, which is what Linux
/// returns and not what a reading of the manual page would suggest.
pub fn getcwd(buffer: u64, size: u64) -> u64 {
    const CWD: &[u8] = b"/\0";
    if size < CWD.len() as u64 {
        return (-34i64) as u64; // ERANGE
    }
    let Some((pointer, _)) =
        crate::arch::syscall::user_range(buffer, CWD.len() as u64, CWD.len() as u64)
    else {
        return error::EFAULT;
    };
    // SAFETY: the range was checked and is exactly two bytes long.
    unsafe {
        core::ptr::copy_nonoverlapping(CWD.as_ptr(), pointer as *mut u8, CWD.len());
    }
    CWD.len() as u64
}

/// `getdents64(fd, buffer, count)`.
///
/// Returns as many whole entries as fit and leaves the rest for the next call,
/// which is what the interface requires: a program calls this in a loop until
/// it returns zero, and an implementation that returned everything or nothing
/// would break on a directory larger than one buffer.
pub fn getdents64(descriptor: u64, buffer: u64, count: u64) -> u64 {
    let (process, handle, node) = match opened(descriptor) {
        Ok(found) => found,
        Err(error) => return error,
    };
    if !node.is_directory() {
        return ENOTDIR;
    }
    let room = count.min(MAX_TRANSFER);
    if room == 0 {
        return error::EINVAL;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, room, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };

    let entries = match store::entries(&node) {
        Ok(entries) => entries,
        Err(error) => return errno(error),
    };

    let start = position(&process, handle) as usize;
    let mut out: Vec<u8> = Vec::new();
    let mut done = start;

    for entry in entries.iter().skip(start) {
        // `struct linux_dirent64`: the inode, the offset of the next one, the
        // length of this record, the kind, then the name and its terminator.
        // Padded to eight, because the next record's first field is a u64 and
        // a program reading it unaligned is a program this would have broken.
        let bare = 8 + 8 + 2 + 1 + entry.name.len() + 1;
        let record = bare.next_multiple_of(8);
        if out.len() + record > length {
            break;
        }
        let mut one = alloc::vec![0u8; record];
        one[0..8].copy_from_slice(&u64::from(entry.inode).to_le_bytes());
        // `d_off` is what to seek to for the entry after this one. Linux is
        // free to make it opaque, and here it is the index, which is exactly
        // what this layer seeks by.
        one[8..16].copy_from_slice(&((done as u64) + 1).to_le_bytes());
        one[16..18].copy_from_slice(&(record as u16).to_le_bytes());
        /// `DT_DIR` and `DT_REG`.
        const IS_DIRECTORY: u8 = 4;
        const IS_FILE: u8 = 8;
        one[18] = if entry.kind == crate::fs::nexusfs::Kind::Directory {
            IS_DIRECTORY
        } else {
            IS_FILE
        };
        one[19..19 + entry.name.len()].copy_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&one);
        done += 1;
    }

    if out.is_empty() {
        // Either the directory is finished, or the first entry will not fit in
        // the buffer at all. The two must not look alike: zero means finished,
        // and a program told that about a buffer too small would stop early and
        // miss the rest of the directory.
        if done < entries.len() {
            return (-22i64) as u64; // EINVAL, which is what Linux returns
        }
        return 0;
    }

    // SAFETY: the range was checked to lie inside the user half, and `out` was
    // built to be no longer than it.
    unsafe {
        core::ptr::copy_nonoverlapping(out.as_ptr(), pointer as *mut u8, out.len());
    }
    set_position(&process, handle, done as u64);
    out.len() as u64
}

/// `mkdirat(dirfd, path, mode)`.
pub fn mkdirat(directory: u64, pointer: u64) -> u64 {
    if !is_cwd(directory) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (parent, name) = match split(&path) {
        Ok(split) => split,
        Err(error) => return error,
    };
    match store::create_child(&parent, &name, true) {
        Ok(_) => 0,
        Err(error) => errno(error),
    }
}

/// `unlinkat(dirfd, path, flags)`, which is also `unlink` and `rmdir`.
pub fn unlinkat(directory: u64, pointer: u64) -> u64 {
    if !is_cwd(directory) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (parent, name) = match split(&path) {
        Ok(split) => split,
        Err(error) => return error,
    };
    match store::remove_child(&parent, &name) {
        Ok(()) => 0,
        Err(error) => errno(error),
    }
}

/// `getrandom(buffer, count, flags)`.
///
/// From the same source `nexus_user::random` draws on, which is the processor's
/// own generator. A program that seeds a hash table or a stack guard from this
/// gets real randomness; one that got zeroes would have a hash table an
/// attacker can predict and would never find out.
pub fn getrandom(buffer: u64, count: u64) -> u64 {
    let wanted = count.min(MAX_TRANSFER);
    if wanted == 0 {
        return 0;
    }
    let Some((pointer, length)) = crate::arch::syscall::user_range(buffer, wanted, MAX_TRANSFER)
    else {
        return error::EFAULT;
    };
    let mut bytes = alloc::vec![0u8; length];
    if !crate::random::bytes(&mut bytes) {
        return (-11i64) as u64; // EAGAIN, which is what Linux says when the
                                // pool is not ready
    }
    // SAFETY: the range was checked to lie inside the user half and is exactly
    // as long as what is written into it.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, length);
    }
    length as u64
}
