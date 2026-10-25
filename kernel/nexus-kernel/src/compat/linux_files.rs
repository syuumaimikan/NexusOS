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

/// Where each open descriptor has got to, keyed by process and handle.
///
/// Keyed by both because a handle number is only unique within a process, and
/// process identifiers are never reused -- so a stale entry can name nothing
/// rather than somebody else's file.
static POSITIONS: IrqSpinLock<BTreeMap<(u64, u32), u64>> = IrqSpinLock::new(BTreeMap::new());

/// Named Linux FIFOs are transient runtime objects, not NexusFS files. Unlink
/// removes only the name; already-open descriptors retain the stream.
static FIFOS: IrqSpinLock<BTreeMap<String, Arc<crate::pipe::Pipe>>> =
    IrqSpinLock::new(BTreeMap::new());

/// Turn a store failure into the Linux error that means the same thing.
///
/// Not a single catch-all: a program that cannot tell "no such file" from "the
/// disk is broken" retries the wrong one.
pub(super) fn errno(error: StoreError) -> u64 {
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
pub(super) fn root() -> Result<Arc<Node>, u64> {
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

/// Standard temporary storage is part of the runtime filesystem, not a file
/// an application must create before it can create its own temporary folder.
pub fn prepare_root() -> Result<(), u64> {
    let root = root()?;
    match store::open_child(&root, "tmp") {
        Ok(node) if node.is_directory() => Ok(()),
        Ok(_) => Err(ENOTDIR),
        Err(StoreError::Fs(crate::fs::nexusfs::FsError::NotFound)) => {
            store::create_child(&root, "tmp", true)
                .map(|_| ())
                .map_err(errno)
        }
        Err(error) => Err(errno(error)),
    }
}

/// Read a path out of the caller's memory.
pub(super) fn path_of(pointer: u64) -> Result<String, u64> {
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

/// Open a path using the process's actual working directory.
pub(crate) fn walk(path: &str) -> Result<Arc<Node>, u64> {
    super::linux_paths::resolve(AT_FDCWD as u32 as u64, path)?.node()
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

/// Stable file identity for Linux's inode-scoped advisory locks. Anonymous
/// memfds return `None` and use their open-description identity instead.
pub(super) fn flock_identity(descriptor: u64) -> Result<Option<(u64, u64)>, u64> {
    match describe(descriptor)? {
        Descriptor::File(node, _) => Ok(Some((1, u64::from(node.inode())))),
        Descriptor::Other(crate::ipc::Object::ProcessData(view)) => {
            let leaf = match view.view {
                crate::process::ProcessView::Arguments => 1,
                crate::process::ProcessView::Environment => 2,
                crate::process::ProcessView::Stat => 4,
            };
            Ok(Some((2, view.owner.0 * 4 + leaf)))
        }
        Descriptor::Other(crate::ipc::Object::Console(console))
            if console.stream == EMPTY_MEMFD =>
        {
            Ok(None)
        }
        _ => Err(error::EBADF),
    }
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
                    super::linux_more::duplicate_description(
                        process.id.0,
                        u64::from(handle),
                        u64::from(copy),
                    );
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
                    super::linux_more::duplicate_description(
                        process.id.0,
                        u64::from(handle),
                        u64::from(at),
                    );
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
    super::linux_more::remember_open_description(process.id.0, u64::from(descriptor));
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
/// have opened it by name, with the same cwd and parent traversal rules.
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
#[allow(dead_code)] // In-memory installers; boot imports use the streamed path.
pub fn install(path: &str, bytes: &[u8]) -> Result<(), u64> {
    let (_, file, _) = prepare_install(path)?;
    store::write_node(&file, bytes).map_err(errno)
}

fn prepare_install(path: &str) -> Result<(Arc<Node>, Arc<Node>, &str), u64> {
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
    Ok((node, file, name))
}

/// Stream a boot runtime from FAT into the Linux root without a whole-file
/// kernel allocation. Failed copies are removed so a later boot can retry.
pub fn install_from_fat(
    volume: &crate::fs::fat32::Volume,
    from: &str,
    to: &str,
) -> Result<u64, crate::fs::fat32::CopyError<u64>> {
    use crate::fs::fat32::CopyError;
    if exists(to) {
        return Err(CopyError::Write(EEXIST));
    }
    let (parent, file, name) = prepare_install(to).map_err(CopyError::Write)?;
    let result = volume.read_file_chunks(from, |offset, bytes| {
        store::write_node_at(&file, offset, bytes)
            .map_err(errno)
            .and_then(|written| {
                if written == bytes.len() as u64 {
                    Ok(())
                } else {
                    Err((-5i64) as u64)
                }
            })
    });
    if result.is_err() {
        let _ = store::remove_child(&parent, name);
    }
    result
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

/// Copy the per-descriptor cursors when a process forks. The descriptor objects
/// themselves are duplicated by `HandleTable::fork_clone`.
pub fn inherit(parent: u64, child: u64) {
    let mut positions = POSITIONS.lock();
    let inherited: Vec<_> = positions
        .iter()
        .filter_map(|((owner, descriptor), offset)| {
            (*owner == parent).then_some(((child, *descriptor), *offset))
        })
        .collect();
    positions.extend(inherited);
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
/// Absolute names start at the Linux root; relative names use cwd or dirfd.
pub fn openat(directory: u64, pointer: u64, flags: u64) -> u64 {
    if flags & (flag::CREAT | flag::DIRECTORY) == (flag::CREAT | flag::DIRECTORY) {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let resolver = if flags & 0o400000 != 0
        || flags & (flag::CREAT | flag::EXCL) == (flag::CREAT | flag::EXCL)
    {
        super::linux_paths::resolve_no_follow
    } else {
        super::linux_paths::resolve
    };
    let resolved = match resolver(directory, &path) {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    if let Some(result) = super::linux_proc::open(&resolved.path, flags) {
        return result;
    }
    if super::linux_random::known(&resolved.path) {
        return super::linux_random::open(flags);
    }

    // A device rather than a file. Checked here, before the walk, because the
    // walk would create `dev/nexus/display` as an ordinary empty file the first
    // time a program opened it with `O_CREAT` -- and then every boot after that
    // would find a file there and open it, and the program would draw into the
    // disk.
    if super::linux_display::is_device(&resolved.path) {
        return super::linux_display::open();
    }

    // Drop the namespace lock before waiting for the opposite FIFO endpoint;
    // that peer needs the same lock to enter open.
    let fifo = { FIFOS.lock().get(&resolved.path).cloned() };
    if let Some(pipe) = fifo {
        if flags & flag::CREAT != 0 && flags & flag::EXCL != 0 {
            return EEXIST;
        }
        if flags & flag::DIRECTORY != 0 {
            return ENOTDIR;
        }
        let reading = flags & flag::WRONLY == 0;
        let writing = flags & (flag::WRONLY | flag::RDWR) != 0;
        let end = pipe.open(reading, writing);
        let nonblocking = flags & 0o4000 != 0;
        if let Err(error) = end.wait_for_peer(nonblocking) {
            return match error {
                crate::pipe::PipeError::NoReader => (-6i64) as u64, // ENXIO
                crate::pipe::PipeError::Cancelled => EINTR,
                crate::pipe::PipeError::WrongEnd => error::EINVAL,
            };
        }
        let Ok(process) = process() else {
            return error::EBADF;
        };
        let mut rights = crate::ipc::Rights::CLOSE | crate::ipc::Rights::TRANSFER;
        if reading {
            rights = rights | crate::ipc::Rights::READ;
        }
        if writing {
            rights = rights | crate::ipc::Rights::WRITE;
        }
        let handle = process
            .handles
            .insert(crate::ipc::Object::Pipe(end), rights);
        return u64::from(handle);
    }

    if resolved.node.as_ref().is_some_and(|node| node.is_symlink()) {
        return if flags & (flag::CREAT | flag::EXCL) == (flag::CREAT | flag::EXCL) {
            EEXIST
        } else {
            (-40i64) as u64
        };
    }

    let writing = flags & (flag::WRONLY | flag::RDWR) != 0;
    let node = match resolved.node() {
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
            // open(O_CREAT) creates files, never missing directories.
            if flags & flag::DIRECTORY != 0 {
                return ENOENT;
            }
            if path.ends_with('/') || resolved.path.ends_with('/') {
                return EISDIR;
            }
            let (parent, name) = match resolved.entry(&path) {
                Ok(entry) => entry,
                Err(error) => return error,
            };
            match store::create_child(parent, name, false) {
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
    super::linux_more::remember_open_description(process.id.0, u64::from(handle));

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
    super::linux_more::forget_descriptor(process.id.0, descriptor);
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
        Ok(Descriptor::Other(crate::ipc::Object::ProcessData(view))) => {
            let Ok(process) = process() else {
                return error::EBADF;
            };
            if process
                .handles
                .object(descriptor as u32, crate::ipc::Rights::READ)
                .is_err()
            {
                return error::EBADF;
            }
            return super::linux_proc::read(&view, buffer, count, offset);
        }
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
        Ok(Descriptor::Other(crate::ipc::Object::Counter(_))) => {
            return if offset.is_some() {
                ESPIPE
            } else {
                super::linux_eventfd::read(descriptor, buffer, count)
            };
        }
        Ok(Descriptor::Other(crate::ipc::Object::Random(_))) => {
            return if offset.is_some() {
                ESPIPE
            } else {
                super::linux_random::read(descriptor, buffer, count)
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
        Ok(Descriptor::Other(crate::ipc::Object::Counter(_))) => {
            return super::linux_eventfd::write(descriptor, buffer, count);
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
    if matches!(
        describe(descriptor),
        Ok(Descriptor::Other(crate::ipc::Object::Random(_)))
    ) {
        return ESPIPE;
    }
    if let Ok(Descriptor::Other(crate::ipc::Object::ProcessData(view))) = describe(descriptor) {
        return super::linux_proc::seek(&view, offset, whence);
    }
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

pub(super) struct Metadata {
    pub device: u64,
    pub rdevice: u64,
    pub inode: u64,
    pub mode: u32,
    pub links: u64,
    pub size: u64,
    pub blocks: u64,
}

impl Metadata {
    pub(super) fn node(node: &Node) -> Result<Self, u64> {
        let size = store::size(node).map_err(errno)?;
        Ok(Self {
            device: 1,
            rdevice: 0,
            inode: u64::from(node.inode()),
            mode: if node.is_directory() {
                0o040755
            } else if node.is_symlink() {
                0o120777
            } else {
                0o100644
            },
            links: if node.is_directory() { 2 } else { 1 },
            size,
            blocks: size.div_ceil(512),
        })
    }
}

pub(super) fn metadata_at(directory: u64, path: Option<&str>, flags: u64) -> Result<Metadata, u64> {
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        let resolved = if flags & 0x100 != 0 {
            super::linux_paths::resolve_no_follow(directory, path)?
        } else {
            super::linux_paths::resolve(directory, path)?
        };
        if let Some(result) = super::linux_proc::metadata(&resolved.path, flags & 0x100 != 0) {
            return result;
        }
        if super::linux_random::known(&resolved.path) {
            return Ok(super::linux_random::metadata());
        }
        return resolved.node().and_then(|node| Metadata::node(&node));
    }
    if path.is_some() && flags & 0x1000 == 0 {
        return Err(ENOENT);
    }
    if path.is_some() && is_cwd(directory) {
        return super::linux_paths::cwd(&*process()?).and_then(|node| Metadata::node(&node));
    }
    if let Some(result) = super::linux_proc::descriptor_metadata(directory) {
        return result;
    }
    if let Some(info) = super::linux_random::descriptor_metadata(directory) {
        return Ok(info);
    }
    opened(directory).and_then(|(_, _, node)| Metadata::node(&node))
}

/// Fill one in.
///
/// The shape is the ABI's, not a choice: a program reads `st_size` at offset 48
/// because that is where the C library's header says it is. Fields this machine
/// has no answer for are zero, which is what a program reading them will see
/// and is better than a plausible invention -- a fabricated modification time
/// makes `make` rebuild nothing.
fn fill_metadata(info: &Metadata, out: u64) -> u64 {
    let Ok(process) = process() else {
        return error::EBADF;
    };
    let Some((pointer, _)) =
        crate::arch::syscall::user_range_for(&process, out, STAT_SIZE, STAT_SIZE, true)
    else {
        return error::EFAULT;
    };

    let mut fields = [0u8; STAT_SIZE as usize];
    // The Linux root lives on the one mounted NexusFS volume. Dynamic loaders
    // use (st_dev, st_ino) to recognise a library opened through another name;
    // zero for every inode incorrectly identifies all libraries as one file.
    fields[0..8].copy_from_slice(&info.device.to_le_bytes());
    fields[8..16].copy_from_slice(&info.inode.to_le_bytes());
    // st_nlink at 16: one, and for a directory two, because `.` counts. A
    // program that walks a tree by subtracting two from the link count would
    // otherwise conclude every directory has an impossible number of children.
    fields[16..24].copy_from_slice(&info.links.to_le_bytes());
    fields[24..28].copy_from_slice(&info.mode.to_le_bytes());
    fields[40..48].copy_from_slice(&info.rdevice.to_le_bytes());
    fields[48..56].copy_from_slice(&info.size.to_le_bytes());
    // st_blksize, which is what a program sizes its read buffer by.
    fields[56..64].copy_from_slice(&4096u64.to_le_bytes());
    fields[64..72].copy_from_slice(&info.blocks.to_le_bytes());

    // SAFETY: the range was checked to lie inside the user half and is exactly
    // the size of what is being written into it.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr(), pointer as *mut u8, fields.len());
    }
    0
}

/// `fstat(fd, statbuf)`.
pub fn fstat(descriptor: u64, out: u64) -> u64 {
    match metadata_at(descriptor, None, 0) {
        Ok(info) => fill_metadata(&info, out),
        Err(error) => error,
    }
}

/// Linux i386 stat64 has 96 bytes, including unaligned 64-bit size at 44.
/// The fixed-width statx ABI is separate and may be shared with x86-64.
pub(super) fn stat64_i386(directory: u64, path: Option<u64>, out: u64, flags: u64) -> u64 {
    if flags & !(0x1000 | 0x100 | 0x800) != 0 {
        return error::EINVAL;
    }
    let path = match path.map(path_of).transpose() {
        Ok(path) => path,
        Err(error) => return error,
    };
    let info = match metadata_at(directory, path.as_deref(), flags) {
        Ok(info) => info,
        Err(error) => return error,
    };
    let Ok(process) = process() else {
        return error::EBADF;
    };
    let Some((pointer, _)) = crate::arch::syscall::user_range_for(&process, out, 96, 96, true)
    else {
        return error::EFAULT;
    };
    let mut bytes = [0u8; 96];
    bytes[0..8].copy_from_slice(&info.device.to_le_bytes());
    bytes[12..16].copy_from_slice(&(info.inode as u32).to_le_bytes());
    bytes[16..20].copy_from_slice(&info.mode.to_le_bytes());
    bytes[20..24].copy_from_slice(&(info.links as u32).to_le_bytes());
    bytes[32..40].copy_from_slice(&info.rdevice.to_le_bytes());
    bytes[44..52].copy_from_slice(&info.size.to_le_bytes());
    bytes[52..56].copy_from_slice(&4096u32.to_le_bytes());
    bytes[56..64].copy_from_slice(&info.blocks.to_le_bytes());
    bytes[88..96].copy_from_slice(&info.inode.to_le_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
    }
    0
}

/// `newfstatat(dirfd, path, statbuf, flags)`, which is also `stat` and `lstat`.
///
/// Ordinary store nodes have no symbolic links. Proc executable links are
/// handled separately, including AT_SYMLINK_NOFOLLOW.
pub fn newfstatat(directory: u64, pointer: u64, out: u64, flags: u64) -> u64 {
    const AT_EMPTY_PATH: u64 = 0x1000;
    const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    const AT_NO_AUTOMOUNT: u64 = 0x800;
    if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    match metadata_at(directory, Some(&path), flags) {
        Ok(info) => fill_metadata(&info, out),
        Err(error) => error,
    }
}

/// `faccessat(dirfd, path, mode, flags)` and `access(path, mode)`.
///
/// Existence is the only question this can answer. There is one user on this
/// machine and no permission bits in the store, so a file that is there is
/// readable and writable, and saying otherwise would be inventing a refusal.
pub fn faccessat(directory: u64, pointer: u64) -> u64 {
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    match metadata_at(directory, Some(&path), 0) {
        Ok(_) => 0,
        Err(error) => error,
    }
}

/// `getcwd(buffer, size)`.
///
/// Returns the current namespace path and the length including its terminator.
pub fn getcwd(buffer: u64, size: u64) -> u64 {
    super::linux_paths::getcwd(buffer, size)
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
    let Some((pointer, length)) =
        crate::arch::syscall::user_range_for(&process, buffer, room, MAX_TRANSFER, true)
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
        one[18] = if entry.kind == crate::fs::nexusfs::Kind::Symlink {
            10
        } else if entry.kind == crate::fs::nexusfs::Kind::Directory {
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
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let resolved = match super::linux_paths::resolve_entry(directory, &path) {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    if resolved.node.is_some() || resolved.special {
        return EEXIST;
    }
    let (parent, name) = match resolved.entry(&path) {
        Ok(entry) => entry,
        Err(error) => return error,
    };
    match store::create_child(parent, name, true) {
        Ok(_) => 0,
        Err(error) => errno(error),
    }
}

/// `mknodat` for named FIFOs. Other special node types need device and
/// permission machinery NexusOS does not expose yet and are explicitly denied.
pub fn mknodat(directory: u64, pointer: u64, mode: u64) -> u64 {
    const TYPE_MASK: u64 = 0o170000;
    const FIFO: u64 = 0o010000;
    if mode & TYPE_MASK != FIFO {
        return (-1i64) as u64; // EPERM
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let resolved = match super::linux_paths::resolve_entry(directory, &path) {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    if resolved.node.is_some() || resolved.special {
        return EEXIST;
    }
    if let Err(errno) = resolved.entry(&path) {
        return errno;
    }
    let mut fifos = FIFOS.lock();
    if fifos.contains_key(&resolved.path) {
        return EEXIST;
    }
    fifos.insert(resolved.path, crate::pipe::Pipe::named());
    0
}

/// `unlinkat(dirfd, path, flags)`, which is also `unlink` and `rmdir`.
pub fn unlinkat(directory: u64, pointer: u64, flags: u64) -> u64 {
    if flags & !0x200 != 0 {
        return error::EINVAL;
    }
    let path = match path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let resolved = match super::linux_paths::resolve_entry(directory, &path) {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    let mut fifos = FIFOS.lock();
    if fifos.contains_key(&resolved.path) {
        if flags & 0x200 != 0 {
            return ENOTDIR;
        }
        fifos.remove(&resolved.path);
        return 0;
    }
    drop(fifos);
    let (parent, name) = match resolved.entry(&path) {
        Ok(entry) => entry,
        Err(error) => return error,
    };
    let node = match resolved.node() {
        Ok(node) => node,
        Err(error) => return error,
    };
    if node.is_directory() && flags & 0x200 == 0 {
        return EISDIR;
    }
    if !node.is_directory() && flags & 0x200 != 0 {
        return ENOTDIR;
    }
    match store::remove_child(parent, name) {
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
