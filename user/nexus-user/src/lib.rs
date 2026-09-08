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

/// Stop this thread. Never returns.
pub fn exit() -> ! {
    // SAFETY: `Exit` takes no arguments and does not return.
    unsafe {
        syscall(Call::Exit, 0, 0, 0, 0, 0, 0);
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

/// What a handle may be used for.
pub fn rights(handle: Handle) -> Result<u32, Error> {
    // SAFETY: takes one integer.
    let result = unsafe { syscall(Call::HandleRights, u64::from(handle.0), 0, 0, 0, 0, 0) };
    check(result).map(|bits| bits as u32)
}
