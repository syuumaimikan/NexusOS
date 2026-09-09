//! The NexusOS user-space runtime.
//!
//! Everything a program needs to reach the kernel, and nothing else. There is
//! no allocator, no standard library, and no runtime that starts before `main`:
//! a program is its entry point, the system calls in this crate, and whatever
//! it brings itself.
//!
//! # The system-call ABI
//!
//! | Register | Meaning |
//! |----------|---------|
//! | `rax`    | call number, and the result on return |
//! | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | arguments one to six |
//! | `rcx`, `r11` | destroyed by the instruction itself |
//!
//! The argument registers are the SysV C ones with `r10` standing in for `rcx`,
//! which the `syscall` instruction takes for the return address. They are not
//! preserved; everything else is.
//!
//! # Errors
//!
//! A call returns its result in `rax`, and errors are values near the top of
//! the range rather than a separate register or a sign convention. A length is
//! bounded by [`MAX_MESSAGE`], so nothing a call can legitimately return
//! reaches them. [`Error`] turns one back into something to match on.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::asm;

/// Longest message a channel will carry.
pub const MAX_MESSAGE: usize = 256;
/// Most handles one message may carry.
pub const MAX_HANDLES: usize = 4;

/// What a call returned when it did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The kernel does not implement that call.
    NotImplemented,
    /// The arguments did not survive checking.
    Invalid,
    /// No such handle in this process.
    BadHandle,
    /// The handle does not carry the right the call needs.
    Denied,
    /// The other end of the channel is gone.
    Closed,
    /// The receiving end is full; try again once it has been drained.
    Again,
    /// The message is longer than a channel will carry.
    TooLong,
    /// The caller is not a user process.
    NotAProcess,
    /// No such file or directory.
    NotFound,
    /// That name is already taken.
    Exists,
    /// The buffer is smaller than what would go in it.
    TooBig,
    /// The filesystem refused: it is full, damaged, busy, or absent.
    Filesystem,
    /// Something the kernel returned that this runtime does not recognise.
    Unknown(u64),
}

/// Values at or above this are errors rather than results.
const ERROR_BASE: u64 = u64::MAX - 15;

impl Error {
    fn from_raw(value: u64) -> Self {
        match value {
            v if v == u64::MAX => Self::NotImplemented,
            v if v == u64::MAX - 1 => Self::Invalid,
            v if v == u64::MAX - 2 => Self::BadHandle,
            v if v == u64::MAX - 3 => Self::Denied,
            v if v == u64::MAX - 4 => Self::Closed,
            v if v == u64::MAX - 5 => Self::Again,
            v if v == u64::MAX - 6 => Self::TooLong,
            v if v == u64::MAX - 7 => Self::NotAProcess,
            v if v == u64::MAX - 8 => Self::NotFound,
            v if v == u64::MAX - 9 => Self::Exists,
            v if v == u64::MAX - 10 => Self::TooBig,
            v if v == u64::MAX - 11 => Self::Filesystem,
            other => Self::Unknown(other),
        }
    }
}

/// Turn a raw return value into a result.
fn check(value: u64) -> Result<u64, Error> {
    if value >= ERROR_BASE {
        Err(Error::from_raw(value))
    } else {
        Ok(value)
    }
}

/// The calls the kernel implements.
#[derive(Debug, Clone, Copy)]
#[repr(u64)]
enum Call {
    Exit = 0,
    Log = 1,
    Uptime = 2,
    Yield = 3,
    ThreadId = 4,
    ChannelCreate = 5,
    ChannelWrite = 6,
    ChannelRead = 7,
    HandleClose = 8,
    HandleRights = 9,
    MemoryCreate = 10,
    MemoryMap = 11,
    MemorySize = 12,
    NodeOpen = 13,
    NodeCreate = 14,
    NodeRemove = 15,
    NodeList = 16,
    NodeRead = 17,
    NodeWrite = 18,
    NodeSize = 19,
    ProcessWait = 20,
    WaitSetCreate = 21,
    WaitSetAdd = 22,
    WaitSetRemove = 23,
    WaitSetWait = 24,
    HandleDuplicate = 25,
    Sleep = 26,
    ProcessKill = 27,
}

/// Make a system call.
///
/// # Safety
///
/// The arguments must mean what the call being made expects, which for the
/// calls that take pointers means they must be valid for the length given.
unsafe fn syscall(call: Call, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    let result: u64;
    // SAFETY: upheld by the caller. `rcx` and `r11` are destroyed by the
    // instruction, which is why they are declared clobbered rather than used.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") call as u64 => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Stop this process, saying it worked. Never returns.
pub fn exit() -> ! {
    exit_with(0)
}

/// Stop this process with a status. Never returns.
///
/// The number goes to whoever holds a handle to this process and waits for it.
/// The kernel attaches no meaning to it; zero means success because that is
/// what everything here writes, not because anything enforces it.
pub fn exit_with(status: u32) -> ! {
    // SAFETY: `Exit` takes a status and does not return.
    unsafe {
        syscall(Call::Exit, u64::from(status), 0, 0, 0, 0, 0);
        // The kernel does not come back from this, but the compiler has no way
        // to know that, and running into whatever follows would be worse than
        // any fault.
        core::hint::unreachable_unchecked()
    }
}

/// Write a line to the kernel log.
pub fn log(text: &str) -> Result<usize, Error> {
    // SAFETY: the pointer and length describe a live string in this process.
    let result = unsafe {
        syscall(
            Call::Log,
            text.as_ptr() as u64,
            text.len() as u64,
            0,
            0,
            0,
            0,
        )
    };
    check(result).map(|written| written as usize)
}

/// Milliseconds since the system started.
#[must_use]
pub fn uptime() -> u64 {
    // SAFETY: takes no arguments.
    unsafe { syscall(Call::Uptime, 0, 0, 0, 0, 0, 0) }
}

/// Stop running for `milliseconds`.
///
/// The thread leaves the run queues and costs nothing until its time is up,
/// which is what separates this from a loop that yields: that costs a context
/// switch every time round for as long as it lasts.
pub fn sleep(milliseconds: u64) -> Result<(), Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::Sleep, milliseconds, 0, 0, 0, 0, 0) };
    check(result).map(|_| ())
}

/// Give up the rest of this thread's turn.
pub fn yield_now() {
    // SAFETY: takes no arguments.
    unsafe {
        syscall(Call::Yield, 0, 0, 0, 0, 0, 0);
    }
}

/// This thread's identifier.
#[must_use]
pub fn thread_id() -> u64 {
    // SAFETY: takes no arguments.
    unsafe { syscall(Call::ThreadId, 0, 0, 0, 0, 0, 0) }
}

/// One end of a channel.
///
/// A plain number, because that is what a handle is: an index into a table the
/// kernel keeps for this process. Wrapping it in a type is what stops it being
/// confused with a length or an identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle(pub u32);

/// Make a channel, returning both ends.
pub fn channel() -> Result<(Handle, Handle), Error> {
    // SAFETY: takes no arguments.
    let packed = unsafe { syscall(Call::ChannelCreate, 0, 0, 0, 0, 0, 0) };
    let packed = check(packed)?;
    Ok((
        Handle((packed >> 32) as u32),
        Handle((packed & 0xFFFF_FFFF) as u32),
    ))
}

/// Send a message, and any handles with it.
///
/// The handles are *moved*: they leave this process at the moment the message
/// is built, and using one afterwards is an error.
pub fn send(handle: Handle, message: &[u8], handles: &[Handle]) -> Result<usize, Error> {
    // SAFETY: both slices are live here, and `Handle` is a transparent `u32`,
    // which is what the kernel reads them as.
    let result = unsafe {
        syscall(
            Call::ChannelWrite,
            u64::from(handle.0),
            message.as_ptr() as u64,
            message.len() as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            0,
        )
    };
    check(result).map(|written| written as usize)
}

/// What a completed read produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Bytes written into the caller's buffer.
    pub bytes: usize,
    /// Handles written into the caller's handle buffer.
    pub handles: usize,
}

/// Wait for a message and take it.
///
/// Blocks until one arrives. Returns [`Error::Closed`] when the other end has
/// gone and nothing is queued, because then no message can ever arrive and
/// waiting would be waiting forever.
pub fn receive(
    handle: Handle,
    buffer: &mut [u8],
    handles: &mut [Handle],
) -> Result<Received, Error> {
    // SAFETY: both slices are live and writable here.
    let result = unsafe {
        syscall(
            Call::ChannelRead,
            u64::from(handle.0),
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            handles.as_mut_ptr() as u64,
            handles.len() as u64,
            0,
        )
    };
    let packed = check(result)?;
    Ok(Received {
        bytes: (packed & 0xFFFF_FFFF) as usize,
        handles: (packed >> 32) as usize,
    })
}

/// Give up a handle.
pub fn close(handle: Handle) -> Result<(), Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::HandleClose, u64::from(handle.0), 0, 0, 0, 0, 0) };
    check(result).map(|_| ())
}

/// Set aside memory that more than one process can see.
///
/// The handle comes back; nothing is mapped. Mapping is separate because where
/// it goes is the caller's business, and because the *handle* is what travels:
/// a process hands it to another and each maps it wherever suits it.
pub fn memory_create(size: usize) -> Result<Handle, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::MemoryCreate, size as u64, 0, 0, 0, 0, 0) };
    check(result).map(|handle| Handle(handle as u32))
}

/// Map a memory object at `address`, returning how many bytes were mapped.
///
/// The address must be page aligned and must not be over anything already
/// mapped. Asking for write access to a handle that does not carry it is
/// refused rather than quietly downgraded, so a program cannot believe it has
/// writable memory that is not.
pub fn memory_map(handle: Handle, address: usize, writable: bool) -> Result<usize, Error> {
    // SAFETY: the kernel checks the address against this process's own space;
    // nothing is dereferenced here.
    let result = unsafe {
        syscall(
            Call::MemoryMap,
            u64::from(handle.0),
            address as u64,
            u64::from(writable),
            0,
            0,
            0,
        )
    };
    check(result).map(|bytes| bytes as usize)
}

/// How large a memory object is.
pub fn memory_size(handle: Handle) -> Result<usize, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::MemorySize, u64::from(handle.0), 0, 0, 0, 0, 0) };
    check(result).map(|size| size as usize)
}

/// Rights a handle can carry.
pub mod rights {
    /// Read from it, receive on it, wait on it.
    pub const READ: u32 = 1 << 0;
    /// Write to it, send on it, change it.
    pub const WRITE: u32 = 1 << 1;
    /// Close it.
    pub const CLOSE: u32 = 1 << 2;
    /// Send it to another process.
    pub const TRANSFER: u32 = 1 << 3;
    /// Everything.
    pub const ALL: u32 = READ | WRITE | CLOSE | TRANSFER;
}

/// Another handle to the same object, carrying no more than this one.
///
/// What a program needs to hand something on and *keep* it. Handles move when
/// they cross a channel, so a process that sends a client a buffer without
/// duplicating it first has given it away -- and the object dies with the
/// client, taking the frames out from under anyone else still mapping them.
///
/// Rights can only be dropped. Asking for one the original does not carry is
/// refused rather than quietly trimmed, because a program that believes it
/// handed out a read-only handle should find out that it did not.
pub fn duplicate(handle: Handle, rights: u32) -> Result<Handle, Error> {
    // SAFETY: takes two integers.
    let result = unsafe {
        syscall(
            Call::HandleDuplicate,
            u64::from(handle.0),
            u64::from(rights),
            0,
            0,
            0,
            0,
        )
    };
    check(result).map(|handle| Handle(handle as u32))
}

/// What a handle may be used for.
pub fn rights(handle: Handle) -> Result<u32, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::HandleRights, u64::from(handle.0), 0, 0, 0, 0, 0) };
    check(result).map(|bits| bits as u32)
}

// -- Files and directories ---------------------------------------------------
//
// There is no `open("/etc/passwd")` here and there will not be one. A program
// reaches a file by naming a single component inside a directory it already
// holds a handle to, so what it can reach is exactly the subtree under what it
// was given. A program handed nothing can open nothing, and there is no name it
// could use instead.
//
// Files are read and written whole. The filesystem underneath has no buffer
// cache, so a partial write would be a partial write to the disk; when there is
// one, this grows the interface that deserves.

/// What a directory entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

/// One entry of a directory, borrowed from the buffer it was read into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryEntry<'a> {
    pub name: &'a str,
    pub kind: Kind,
}

/// Open one name inside a directory.
///
/// The name is one component. A `/` in it is refused rather than walked, which
/// is what keeps a directory handle meaning "this subtree".
pub fn open(directory: Handle, name: &str) -> Result<Handle, Error> {
    // SAFETY: the pointer and length describe a live string in this process.
    let result = unsafe {
        syscall(
            Call::NodeOpen,
            u64::from(directory.0),
            name.as_ptr() as u64,
            name.len() as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|handle| Handle(handle as u32))
}

/// Make a file or a directory, and open it.
pub fn create(directory: Handle, name: &str, kind: Kind) -> Result<Handle, Error> {
    // SAFETY: as above.
    let result = unsafe {
        syscall(
            Call::NodeCreate,
            u64::from(directory.0),
            name.as_ptr() as u64,
            name.len() as u64,
            u64::from(kind == Kind::Directory),
            0,
            0,
        )
    };
    check(result).map(|handle| Handle(handle as u32))
}

/// Remove a name, and the thing it named.
///
/// A directory has to be empty, and nothing may still hold a handle to what is
/// being removed.
pub fn remove(directory: Handle, name: &str) -> Result<(), Error> {
    // SAFETY: as above.
    let result = unsafe {
        syscall(
            Call::NodeRemove,
            u64::from(directory.0),
            name.as_ptr() as u64,
            name.len() as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|_| ())
}

/// Read a directory into `buffer`, returning how many bytes it filled.
///
/// Use [`entries`] to walk what comes back. Returns [`Error::TooBig`] rather
/// than a prefix when the buffer is too small: half a directory looks exactly
/// like a whole one.
pub fn list(directory: Handle, buffer: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the buffer is live and writable here.
    let result = unsafe {
        syscall(
            Call::NodeList,
            u64::from(directory.0),
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|bytes| bytes as usize)
}

/// Walk what [`list`] wrote.
///
/// Returns `None` at the first entry that does not parse, rather than skipping
/// it: the entries are packed one after another, so an unreadable one means
/// every byte after it is at an unknown offset.
#[must_use]
pub fn entries(packed: &[u8]) -> Entries<'_> {
    Entries { packed }
}

/// The iterator [`entries`] returns.
pub struct Entries<'a> {
    packed: &'a [u8],
}

impl<'a> Iterator for Entries<'a> {
    type Item = DirectoryEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.packed.is_empty() {
            return None;
        }
        if self.packed.len() < 8 {
            self.packed = &[];
            return None;
        }
        let length = self.packed[4] as usize;
        let kind = match self.packed[5] {
            2 => Kind::Directory,
            _ => Kind::File,
        };
        if self.packed.len() < 8 + length {
            self.packed = &[];
            return None;
        }
        let Ok(name) = core::str::from_utf8(&self.packed[8..8 + length]) else {
            self.packed = &[];
            return None;
        };
        self.packed = &self.packed[8 + length..];
        Some(DirectoryEntry { name, kind })
    }
}

/// Read a whole file into `buffer`, returning its length.
///
/// [`Error::TooBig`] when the buffer is smaller than the file. Ask with
/// [`size`] first if the length is not already known.
pub fn read(file: Handle, buffer: &mut [u8]) -> Result<usize, Error> {
    // SAFETY: the buffer is live and writable here.
    let result = unsafe {
        syscall(
            Call::NodeRead,
            u64::from(file.0),
            buffer.as_mut_ptr() as u64,
            buffer.len() as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|bytes| bytes as usize)
}

/// Replace a whole file, returning how many bytes it now holds.
///
/// An empty slice empties the file, which is the one length that is not an
/// error.
pub fn write(file: Handle, data: &[u8]) -> Result<usize, Error> {
    // SAFETY: the slice is live here.
    let result = unsafe {
        syscall(
            Call::NodeWrite,
            u64::from(file.0),
            data.as_ptr() as u64,
            data.len() as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|bytes| bytes as usize)
}

/// How many bytes a file or directory holds.
pub fn size(node: Handle) -> Result<usize, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::NodeSize, u64::from(node.0), 0, 0, 0, 0, 0) };
    check(result).map(|size| size as usize)
}

// -- Processes -----------------------------------------------------------------

/// Ask a process to stop.
///
/// Returns at once, because stopping is asking: the kernel sets a flag and
/// wakes whatever the process had asleep, and its threads leave when they
/// notice. Wait for it afterwards to know it has actually gone.
///
/// Needs write on the handle. Being able to watch something end is not the same
/// right as being able to end it, so a program handed a read-only process
/// handle can wait for it and nothing more.
pub fn kill(process: Handle) -> Result<(), Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::ProcessKill, u64::from(process.0), 0, 0, 0, 0, 0) };
    check(result).map(|_| ())
}

/// Wait for a process to end, and take its status.
///
/// Blocks. A process that has already ended answers immediately, which is the
/// case that matters: a caller that asks after the fact must get the answer
/// rather than wait forever for something that has been and gone.
///
/// The handle is the authority. There is no call that waits on a process
/// identifier, because an identifier is a number that could be guessed and a
/// handle is something that had to be given.
pub fn wait(process: Handle) -> Result<Ending, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::ProcessWait, u64::from(process.0), 0, 0, 0, 0, 0) };
    check(result).map(|status| {
        if status >= 1 << 32 {
            Ending::Stopped
        } else {
            Ending::Exited(status as u32)
        }
    })
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It decided to stop, with this status. Zero conventionally means it
    /// worked, because that is what every program here writes.
    Exited(u32),
    /// Somebody stopped it.
    Stopped,
}

// -- Waiting for several things ------------------------------------------------
//
// Every other blocking call names one object, which is enough for a program with
// one thing to do and not enough for a server: something holding channels to
// four clients cannot serve the second while blocked on the first. A wait set is
// the answer, and it is an object held by a handle like everything else here.
//
// Put handles into one under keys of your own choosing, wait on the set, and be
// told which keys are ready. The keys come back unchanged -- they are whatever
// the program already calls that client, not something to look up.

/// Somewhere to wait for whichever of several things happens first.
pub fn wait_set() -> Result<Handle, Error> {
    // SAFETY: takes no arguments.
    let result = unsafe { syscall(Call::WaitSetCreate, 0, 0, 0, 0, 0, 0) };
    check(result).map(|handle| Handle(handle as u32))
}

/// Watch `handle` as part of `set`, under `key`.
///
/// Channels and processes only. A channel is ready when a message is waiting
/// *or* its peer has gone, because both are things the holder has to act on; a
/// process is ready when it has ended. Anything else is never not ready, and
/// watching one would be waiting for something that cannot arrive.
pub fn watch(set: Handle, handle: Handle, key: u64) -> Result<(), Error> {
    // SAFETY: takes three integers.
    let result = unsafe {
        syscall(
            Call::WaitSetAdd,
            u64::from(set.0),
            u64::from(handle.0),
            key,
            0,
            0,
            0,
        )
    };
    check(result).map(|_| ())
}

/// Stop watching whatever has this key.
pub fn unwatch(set: Handle, key: u64) -> Result<(), Error> {
    // SAFETY: takes two integers.
    let result = unsafe { syscall(Call::WaitSetRemove, u64::from(set.0), key, 0, 0, 0, 0) };
    check(result).map(|_| ())
}

/// Block until something in `set` is ready, and fill `keys` with what is.
///
/// Returns how many keys were written. Every ready key comes back, not the
/// first, so a server with four clients ready serves four of them for one
/// system call.
///
/// Returns zero when the set is empty: nothing can ever make it ready, so
/// waiting would be waiting forever.
pub fn wait_any(set: Handle, keys: &mut [u64]) -> Result<usize, Error> {
    // SAFETY: the buffer is live and writable here, and its length is given in
    // bytes because that is what the kernel checks the range against.
    let result = unsafe {
        syscall(
            Call::WaitSetWait,
            u64::from(set.0),
            keys.as_mut_ptr() as u64,
            core::mem::size_of_val(keys) as u64,
            0,
            0,
            0,
        )
    };
    check(result).map(|count| count as usize)
}
