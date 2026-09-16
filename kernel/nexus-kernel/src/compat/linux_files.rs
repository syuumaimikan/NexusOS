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
    let rights = if writing {
        crate::ipc::Rights::READ | crate::ipc::Rights::WRITE | crate::ipc::Rights::CLOSE
    } else {
        crate::ipc::Rights::READ | crate::ipc::Rights::CLOSE
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
    if descriptor <= STDERR {
        return 0;
    }
    let Ok((process, handle, _)) = opened(descriptor) else {
        return error::EBADF;
    };
    POSITIONS.lock().remove(&(process.id.0, handle));
    match process.handles.close(handle) {
        Ok(()) => 0,
        Err(_) => error::EBADF,
    }
}

/// `read(fd, buffer, count)`.
pub fn read(descriptor: u64, buffer: u64, count: u64) -> u64 {
    let (process, handle, node) = match opened(descriptor) {
        Ok(found) => found,
        Err(error) => return error,
    };
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

    let at = position(&process, handle);
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
    set_position(&process, handle, at + taken as u64);
    taken as u64
}

/// `write(fd, buffer, count)`, for a descriptor that is a file.
///
/// Standard output and standard error are not handled here: they go to the boot
/// log, which is [`super::linux`]'s business.
pub fn write(descriptor: u64, buffer: u64, count: u64) -> u64 {
    let (process, handle, node) = match opened(descriptor) {
        Ok(found) => found,
        Err(error) => return error,
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
