//! The `syscall` entry path.
//!
//! `syscall` is the boundary. Everything above it is the Nexus system-call
//! interface that the Linux and Windows compatibility layers will be written
//! *on top of* rather than inside; nothing here is anyone else's ABI.
//!
//! # The instruction, and what it does not do
//!
//! `syscall` is fast because it does almost nothing: it puts the return address
//! in `rcx` and `RFLAGS` in `r11`, loads `CS` and `SS` from `IA32_STAR`, masks
//! the flags named by `IA32_FMASK`, and jumps to `IA32_LSTAR`. It does not
//! switch stacks. The processor arrives in ring 0 still standing on the user
//! stack, which the kernel must not touch, so the first job of the entry stub
//! is to get off it — and it cannot use a register to do that without
//! destroying one belonging to the caller.
//!
//! That is what `swapgs` and the per-CPU area are for: one instruction brings
//! this processor's state into `gs:` reach, the user stack pointer is parked
//! there, and the kernel stack comes back out of it. See [`percpu`].
//!
//! # The NexusOS system-call ABI
//!
//! | Register | Meaning |
//! |----------|---------|
//! | `rax`    | call number, and the result on return |
//! | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | arguments one to six |
//! | `rcx`, `r11` | destroyed by the instruction itself |
//!
//! The argument registers are the SysV C ones with `r10` standing in for `rcx`,
//! which the instruction takes. They are *not* preserved: a caller that needs
//! them keeps its own copy. Everything else — `rbx`, `rbp`, `rsp`, `r12`
//! through `r15` — comes back untouched, which is what makes a call usable from
//! compiled code without a wrapper that saves the world.

use core::sync::atomic::{AtomicU64, Ordering};

use super::percpu;
use crate::kprintln;

/// `IA32_EFER`, whose bit 0 is what makes `syscall` a legal instruction.
const IA32_EFER: u32 = 0xC000_0080;
/// `IA32_STAR`, the segment selectors both instructions use.
const IA32_STAR: u32 = 0xC000_0081;
/// `IA32_LSTAR`, the 64-bit entry point.
const IA32_LSTAR: u32 = 0xC000_0082;
/// `IA32_FMASK`, the `RFLAGS` bits cleared on entry.
const IA32_FMASK: u32 = 0xC000_0084;

/// `EFER.SCE`, System Call Extensions.
const EFER_SCE: u64 = 1 << 0;

/// Flags cleared on entry to the kernel.
///
/// `IF` so the kernel is not interruptible while it is still on the user stack
/// with no frame; `DF` so `rep` instructions run forwards whatever the caller
/// left set; `TF` so a single-stepping debugger in ring 3 does not trap on the
/// first kernel instruction; `AC` so a user that set alignment checking cannot
/// make ordinary kernel accesses fault; `NT` because it changes what `iret`
/// does.
const FMASK: u64 = {
    const TF: u64 = 1 << 8;
    const IF: u64 = 1 << 9;
    const DF: u64 = 1 << 10;
    const NT: u64 = 1 << 14;
    const AC: u64 = 1 << 18;
    TF | IF | DF | NT | AC
};

/// System calls handled since boot.
static CALLS: AtomicU64 = AtomicU64::new(0);
/// System calls that named a number the kernel does not implement.
static UNKNOWN: AtomicU64 = AtomicU64::new(0);
/// Yields begun and yields returned from, counted rather than logged: printing
/// inside the path being investigated changes the timing that produces it.
static YIELD_ENTERED: AtomicU64 = AtomicU64::new(0);
static YIELD_RETURNED: AtomicU64 = AtomicU64::new(0);

/// Yields begun and yields returned from.
#[must_use]
pub fn yield_statistics() -> (u64, u64) {
    (
        YIELD_ENTERED.load(Ordering::Relaxed),
        YIELD_RETURNED.load(Ordering::Relaxed),
    )
}

/// Enable `syscall` on this processor.
///
/// Per processor, not once: `EFER` and the three companion MSRs are
/// per-processor state, and a core that skipped this would take an invalid
/// opcode on the first system call a thread made after migrating to it.
///
/// # Safety
///
/// The GDT must already be installed on this processor, with the selector
/// layout [`super::gdt`] documents, and per-CPU state must be installed —
/// the entry stub reaches through `gs:` before it does anything else.
pub unsafe fn init() {
    // `sysret` derives both selectors from STAR[63:48]: `CS = base + 16` and
    // `SS = base + 8`. With the 32-bit user code descriptor sitting between the
    // kernel pair and the 64-bit user pair, a base of `USER_CODE32_SELECTOR`
    // lands on user code 64 and user data respectively. `syscall` takes
    // STAR[47:32] as `CS` and that plus eight as `SS`, which is the kernel
    // pair. Both halves are therefore forced by the GDT layout rather than
    // chosen here.
    let star = (u64::from(super::gdt::USER_CODE32_SELECTOR) << 48)
        | (u64::from(super::gdt::KERNEL_CODE_SELECTOR) << 32);

    // SAFETY: the caller guarantees the GDT and per-CPU state are in place.
    unsafe {
        write_msr(IA32_EFER, read_msr(IA32_EFER) | EFER_SCE);
        write_msr(IA32_STAR, star);
        write_msr(IA32_LSTAR, syscall_entry as usize as u64);
        write_msr(IA32_FMASK, FMASK);
    }
}

/// Report that the boundary is open, once, from the boot processor.
pub fn report() {
    kprintln!(
        "[sys ] syscall entry at {:#018x}, {} calls implemented",
        syscall_entry as usize,
        Call::COUNT
    );
}

/// System calls handled, and calls that named an unknown number.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        CALLS.load(Ordering::Relaxed),
        UNKNOWN.load(Ordering::Relaxed),
    )
}

core::arch::global_asm!(
    r#"
    .section .text
    .global nexus_syscall_entry
    .p2align 4
nexus_syscall_entry:
    // Ring 0 already, but still on the user's stack and with the user's GS.
    // Nothing here may touch memory until both are dealt with.
    swapgs
    mov gs:[{user_rsp}], rsp
    mov rsp, gs:[{kernel_rsp}]

    // Move the caller's stack pointer off the per-CPU slot and onto this
    // thread's own stack, immediately.
    //
    // The slot is scratch for exactly the two instructions above, where there
    // was nowhere else to put it. It cannot be where the value *lives*: a
    // system call may block, and a thread that blocks can be resumed on a
    // different processor, whose slot holds some other thread's stack pointer
    // or nothing at all. Leaving it there was a bug that only appeared when a
    // call both blocked and migrated.
    push qword ptr gs:[{user_rsp}]

    // `sysretq` needs these two back exactly as the instruction left them.
    // They are pushed rather than kept in registers because the dispatcher is
    // ordinary Rust and may use any caller-saved register it likes.
    push rcx
    push r11

    // Three pushes leave the stack eight bytes out of the alignment the C ABI
    // requires at a `call`. One more restores it.
    sub rsp, 8

    // Shuffle the system-call registers into the C argument registers, right
    // to left so that nothing is overwritten before it is read.
    mov r9, r8
    mov r8, r10
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call {dispatch}

    // The result is already in rax, which is where the caller wants it.
    add rsp, 8
    pop r11
    pop rcx

    // Back onto the user stack -- this thread's, taken from this thread's
    // stack -- and back to the user's GS, with interrupts still masked: an
    // interrupt between these two would arrive with the kernel's GS active and
    // a user stack pointer, and the entry guard would swap the wrong way.
    pop rsp
    swapgs
    sysretq
"#,
    user_rsp = const percpu::offset::USER_STACK_POINTER,
    kernel_rsp = const percpu::offset::SYSCALL_STACK_TOP,
    dispatch = sym dispatch,
);

extern "sysv64" {
    /// The entry point `IA32_LSTAR` holds. Never called from Rust.
    fn nexus_syscall_entry();
}

/// Address of the entry stub, as a plain function item for `LSTAR`.
#[allow(non_upper_case_globals)]
const syscall_entry: unsafe extern "sysv64" fn() = nexus_syscall_entry;

/// The system calls NexusOS implements.
///
/// Deliberately few. Each one here is reachable from ring 3 and is exercised by
/// the user program the kernel starts at boot, because a system call that has
/// never been made from user mode is a function with an unusual name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Call {
    /// Stop the calling thread. Does not return.
    Exit = 0,
    /// Write a string to the kernel log. `(pointer, length)`.
    Log = 1,
    /// Milliseconds since boot.
    Uptime = 2,
    /// Yield the rest of this thread's time slice.
    Yield = 3,
    /// The calling thread's identifier.
    ThreadId = 4,
    /// Create a channel. Returns both handles packed into one word: the first
    /// in the high half, the second in the low.
    ChannelCreate = 5,
    /// Write one message. `(handle, pointer, length)`.
    ChannelWrite = 6,
    /// Read one message, blocking until one arrives.
    /// `(handle, pointer, capacity)`, returning the length.
    ChannelRead = 7,
    /// Close a handle. `(handle)`.
    HandleClose = 8,
    /// What a handle may be used for. `(handle)`, returning the rights bits.
    HandleRights = 9,
}

impl Call {
    /// How many calls exist.
    pub const COUNT: usize = 10;

    /// The call `number` names, if it names one.
    fn from_number(number: u64) -> Option<Self> {
        match number {
            0 => Some(Self::Exit),
            1 => Some(Self::Log),
            2 => Some(Self::Uptime),
            3 => Some(Self::Yield),
            4 => Some(Self::ThreadId),
            5 => Some(Self::ChannelCreate),
            6 => Some(Self::ChannelWrite),
            7 => Some(Self::ChannelRead),
            8 => Some(Self::HandleClose),
            9 => Some(Self::HandleRights),
            _ => None,
        }
    }
}

/// Errors are returned in `rax` as values close to the top of the range.
///
/// Not a separate register or a sign convention: a call that can return a
/// length and an error needs one word to carry both, and lengths are bounded by
/// [`crate::ipc::MAX_MESSAGE`] while these are not reachable by any of them.
/// Returned by a call that names a number the kernel does not implement.
pub const ENOSYS: u64 = u64::MAX;
/// Returned by a call whose arguments do not survive checking.
pub const EINVAL: u64 = u64::MAX - 1;
/// No such handle in the calling process.
pub const EBADF: u64 = u64::MAX - 2;
/// The handle does not carry the right the call needs.
pub const EPERM: u64 = u64::MAX - 3;
/// The other end of the channel is gone.
pub const EPIPE: u64 = u64::MAX - 4;
/// The receiving end is full; try again once it has been drained.
pub const EAGAIN: u64 = u64::MAX - 5;
/// The message is longer than a channel will carry.
pub const EMSGSIZE: u64 = u64::MAX - 6;
/// The caller is not a user process, so it has no handle table.
pub const ENOPROC: u64 = u64::MAX - 7;

/// Longest string [`Call::Log`] will accept.
///
/// A bound rather than a trust: the length comes from ring 3, and without one a
/// caller could ask the kernel to walk to the end of its address space.
const MAX_LOG: u64 = 256;

/// Turn a system call into work.
///
/// Runs on the calling thread's own kernel stack, with interrupts enabled after
/// the first thing it does, so a call may block and be preempted exactly like
/// any other kernel code.
extern "sysv64" fn dispatch(
    number: u64,
    argument0: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    argument4: u64,
) -> u64 {
    // The stub arrives with interrupts masked, because until it had switched
    // stacks there was nowhere safe to take one. There is now.
    super::interrupts::enable();
    CALLS.fetch_add(1, Ordering::Relaxed);

    let result = match Call::from_number(number) {
        Some(Call::Exit) => {
            match crate::sched::current_process() {
                Some(process) => kprintln!(
                    "[sys ] process {} \"{}\" exited through the system-call boundary,                      {} handles open",
                    process.id,
                    process.name.as_str(),
                    process.handles.len()
                ),
                None => kprintln!("[sys ] a kernel thread exited through the boundary"),
            }
            // Masked again on the way out of the kernel, which `exit` never
            // takes -- it does not return.
            super::interrupts::disable();
            crate::sched::exit()
        }
        Some(Call::Log) => log(argument0, argument1),
        Some(Call::Uptime) => super::time::uptime_ms(),
        Some(Call::Yield) => {
            YIELD_ENTERED.fetch_add(1, Ordering::Relaxed);
            crate::sched::yield_now();
            YIELD_RETURNED.fetch_add(1, Ordering::Relaxed);
            0
        }
        Some(Call::ThreadId) => percpu::current_thread(),
        Some(Call::ChannelCreate) => channel_create(),
        Some(Call::ChannelWrite) => {
            channel_write(argument0, argument1, argument2, argument3, argument4)
        }
        Some(Call::ChannelRead) => {
            channel_read(argument0, argument1, argument2, argument3, argument4)
        }
        Some(Call::HandleClose) => handle_close(argument0),
        Some(Call::HandleRights) => handle_rights(argument0),
        None => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            kprintln!("[sys ] unimplemented system call {number}");
            ENOSYS
        }
    };

    // The stub restores the user stack pointer and swaps `GS` back; an
    // interrupt between those two would be taken with a mismatched pair.
    super::interrupts::disable();
    result
}

/// [`Call::Log`]: write a string from user memory to the kernel log.
///
/// Every part of the argument is checked, because every part of it came from
/// ring 3. The length is bounded, the range has to lie wholly below
/// [`USER_SPACE_END`](nexus_abi::layout::USER_SPACE_END) so that a user pointer
/// can never name kernel memory, and the bytes have to be valid UTF-8.
fn log(pointer: u64, length: u64) -> u64 {
    if length == 0 || length > MAX_LOG {
        return EINVAL;
    }
    let Some(end) = pointer.checked_add(length) else {
        return EINVAL;
    };
    if end > nexus_abi::layout::USER_SPACE_END {
        return EINVAL;
    }

    // SAFETY: the range lies inside the user half, which this thread's address
    // space maps, and the length is bounded. A range the caller has unmapped
    // faults, which is the caller's own page fault and not a kernel bug.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length as usize) };
    let Ok(text) = core::str::from_utf8(bytes) else {
        return EINVAL;
    };

    kprintln!("[user] {text}");
    length
}

/// A user range, checked before the kernel will look at it.
///
/// Every byte of this came from ring 3, so nothing about it is assumed: the
/// length is bounded, the range must not wrap, and it must lie wholly below
/// [`USER_SPACE_END`](nexus_abi::layout::USER_SPACE_END) so that a user pointer
/// can never name kernel memory. What is *not* checked is whether the range is
/// mapped -- that is the caller's own page fault, and treating it as one is
/// deliberate until there is a fixup table to turn it into an error return.
fn user_range(pointer: u64, length: u64, limit: u64) -> Option<(u64, usize)> {
    if length == 0 || length > limit {
        return None;
    }
    let end = pointer.checked_add(length)?;
    if end > nexus_abi::layout::USER_SPACE_END {
        return None;
    }
    Some((pointer, length as usize))
}

/// The calling process's handle table, or an error for a kernel thread.
fn caller() -> Result<alloc::sync::Arc<crate::process::Process>, u64> {
    crate::sched::current_process().ok_or(ENOPROC)
}

/// Turn a handle-table failure into the value the caller sees.
fn handle_error(error: crate::ipc::HandleError) -> u64 {
    match error {
        crate::ipc::HandleError::NotFound => EBADF,
        crate::ipc::HandleError::Denied => EPERM,
    }
}

/// [`Call::ChannelCreate`]: a connected pair, one handle each.
///
/// Both handles go to the caller, which is the only thing that can happen while
/// there is no way to pass a handle to another process. That is the next piece:
/// a channel with both ends in one process is a queue, and it becomes a channel
/// when one end can be given away.
fn channel_create() -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };

    let (first, second) = crate::ipc::Endpoint::pair();
    let a = process
        .handles
        .insert(crate::ipc::Object::Channel(first), crate::ipc::Rights::ALL);
    let b = process
        .handles
        .insert(crate::ipc::Object::Channel(second), crate::ipc::Rights::ALL);

    // Two 32-bit handles in one 64-bit result. A call that returns two values
    // otherwise needs a user buffer, which needs checking, for two integers.
    (u64::from(a) << 32) | u64::from(b)
}

/// [`Call::ChannelWrite`]: send one message, and any handles with it.
///
/// The handles are *moved*: they leave the sender's table at the moment the
/// message is built, so there is never an instant where both processes hold
/// one. A send that fails after they have been taken puts them back, because
/// dropping authority on the floor because a queue was full would be a leak the
/// caller could not have avoided.
fn channel_write(
    handle: u64,
    pointer: u64,
    length: u64,
    handles_pointer: u64,
    handle_count: u64,
) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let Some((pointer, length)) = user_range(pointer, length, crate::ipc::MAX_MESSAGE as u64)
    else {
        return EMSGSIZE;
    };

    let endpoint = match process.handles.channel(handle, crate::ipc::Rights::WRITE) {
        Ok(endpoint) => endpoint,
        Err(error) => return handle_error(error),
    };

    let passed = match take_handles(&process, handles_pointer, handle_count) {
        Ok(handles) => handles,
        Err(error) => return error,
    };

    // SAFETY: the range was checked to lie inside the user half, which this
    // thread's address space maps, and its length is bounded.
    let message = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };

    match endpoint.send(message, passed) {
        Ok(written) => written as u64,
        Err(crate::ipc::ChannelError::TooLong) => EMSGSIZE,
        Err(crate::ipc::ChannelError::TooManyHandles) => EINVAL,
        Err(crate::ipc::ChannelError::Full) => EAGAIN,
        Err(crate::ipc::ChannelError::PeerClosed) => EPIPE,
    }
}

/// Take the handles a `channel_write` names out of the caller's table.
///
/// Every one of them needs the transfer right, and the whole set is taken or
/// none of it: a partial transfer would leave the caller having given away some
/// of what it named and been refused the rest, with no way to find out which.
fn take_handles(
    process: &crate::process::Process,
    pointer: u64,
    count: u64,
) -> Result<alloc::vec::Vec<crate::ipc::Handle>, u64> {
    if count == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    if count > crate::ipc::MAX_HANDLES as u64 {
        return Err(EINVAL);
    }

    let bytes = count * 4;
    let Some((pointer, _)) = user_range(pointer, bytes, bytes) else {
        return Err(EINVAL);
    };
    // Alignment is checked rather than assumed: the pointer came from ring 3.
    if pointer % 4 != 0 {
        return Err(EINVAL);
    }

    // SAFETY: the range was checked to lie inside the user half and to be
    // aligned, and a `u32` has no invalid bit patterns.
    let named =
        unsafe { core::slice::from_raw_parts(pointer as *const u32, count as usize) }.to_vec();

    let mut taken = alloc::vec::Vec::new();
    for id in named {
        match process.handles.take(id, crate::ipc::Rights::TRANSFER) {
            Ok(handle) => taken.push(handle),
            Err(error) => {
                // Put back everything already taken, so a bad handle partway
                // through the list costs the caller nothing.
                for handle in taken {
                    process.handles.restore(handle);
                }
                return Err(handle_error(error));
            }
        }
    }
    Ok(taken)
}

/// [`Call::ChannelRead`]: take one message, blocking until there is one.
///
/// Blocking is the point. A thread waiting for a message is off every run queue
/// and costs nothing until one arrives, which is what makes a message-passing
/// system usable as the way processes wait for each other rather than something
/// to poll around.
fn channel_read(
    handle: u64,
    pointer: u64,
    capacity: u64,
    handles_pointer: u64,
    handle_capacity: u64,
) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let Some((pointer, capacity)) = user_range(pointer, capacity, crate::ipc::MAX_MESSAGE as u64)
    else {
        return EINVAL;
    };

    let endpoint = match process.handles.channel(handle, crate::ipc::Rights::READ) {
        Ok(endpoint) => endpoint,
        Err(error) => return handle_error(error),
    };

    let Some(message) = endpoint.receive() else {
        return EPIPE;
    };
    if message.bytes.len() > capacity || message.handles.len() > handle_capacity as usize {
        // The message is gone either way -- a channel delivers whole messages,
        // and putting it back would let a reader with a small buffer block the
        // queue for everyone behind it. Saying so is better than truncating
        // silently, and any handles go down with it rather than being stranded
        // in a message nobody will read.
        return EMSGSIZE;
    }

    // SAFETY: the range was checked as above, and it is at least as long as the
    // message.
    unsafe {
        core::ptr::copy_nonoverlapping(
            message.bytes.as_ptr(),
            pointer as *mut u8,
            message.bytes.len(),
        );
    }

    let count = message.handles.len();
    if count > 0 {
        let bytes = count as u64 * 4;
        let Some((handles_pointer, _)) = user_range(handles_pointer, bytes, bytes) else {
            return EINVAL;
        };
        if handles_pointer % 4 != 0 {
            return EINVAL;
        }
        for (index, handle) in message.handles.into_iter().enumerate() {
            let id = process.handles.restore(handle);
            // SAFETY: the range was checked and is long enough for `count`
            // identifiers.
            unsafe {
                core::ptr::write((handles_pointer as *mut u32).add(index), id);
            }
        }
    }

    // Two answers in one word again: how many handles arrived, and how many
    // bytes. A length is bounded by `MAX_MESSAGE`, so the high half is free.
    ((count as u64) << 32) | message.bytes.len() as u64
}

/// [`Call::HandleClose`]: give up a handle.
fn handle_close(handle: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };

    match process.handles.rights(handle) {
        Ok(rights) if !rights.contains(crate::ipc::Rights::CLOSE) => return EPERM,
        Ok(_) => {}
        Err(error) => return handle_error(error),
    }

    match process.handles.close(handle) {
        Ok(()) => 0,
        Err(error) => handle_error(error),
    }
}

/// [`Call::HandleRights`]: what a handle may be used for.
fn handle_rights(handle: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    match process.handles.rights(handle) {
        Ok(rights) => u64::from(rights.bits()),
        Err(error) => handle_error(error),
    }
}

/// Read a model-specific register.
///
/// # Safety
///
/// `register` must exist on this processor.
unsafe fn read_msr(register: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") register,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Write a model-specific register.
///
/// # Safety
///
/// `register` must exist, and `value` must be one it accepts.
unsafe fn write_msr(register: u32, value: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") register,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}
