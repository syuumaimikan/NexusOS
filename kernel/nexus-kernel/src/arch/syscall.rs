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
    /// Stop the calling thread, with a status. `(status)`. Does not return.
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
    /// Set aside memory more than one process can see. `(size)`.
    MemoryCreate = 10,
    /// Map a memory object into this process. `(handle, address, writable)`.
    MemoryMap = 11,
    /// How large a memory object is. `(handle)`.
    MemorySize = 12,
    /// Open one name inside a directory.
    /// `(directory, pointer, length)`, returning a handle.
    NodeOpen = 13,
    /// Make a file or directory inside a directory.
    /// `(directory, pointer, length, directory?)`, returning a handle.
    NodeCreate = 14,
    /// Remove a name, and the thing it named.
    /// `(directory, pointer, length)`.
    NodeRemove = 15,
    /// Read a directory into a buffer. `(directory, pointer, capacity)`,
    /// returning the bytes written.
    NodeList = 16,
    /// Read a whole file. `(file, pointer, capacity)`, returning its length.
    NodeRead = 17,
    /// Replace a whole file. `(file, pointer, length)`.
    NodeWrite = 18,
    /// How many bytes a file or directory holds. `(handle)`.
    NodeSize = 19,
    /// Wait for a process to end, and take its status. `(handle)`.
    ProcessWait = 20,
    /// Somewhere to wait for several things at once.
    WaitSetCreate = 21,
    /// Watch a handle under a key. `(set, handle, key)`.
    WaitSetAdd = 22,
    /// Stop watching whatever has a key. `(set, key)`.
    WaitSetRemove = 23,
    /// Block until something is ready. `(set, pointer, capacity, milliseconds)`,
    /// returning how many keys were written. `u64::MAX` milliseconds waits for
    /// as long as it takes.
    WaitSetWait = 24,
    /// Another handle to the same object, with no more rights than this one.
    /// `(handle, rights)`.
    HandleDuplicate = 25,
    /// Stop running for a while. `(milliseconds)`.
    Sleep = 26,
    /// Ask a process to stop. `(handle)`.
    ProcessKill = 27,
    /// Take a memory object out of this process's address space.
    /// `(handle, address)`.
    MemoryUnmap = 28,
    /// Read part of a file. `(file, offset, pointer, capacity)`, returning how
    /// much of it was there.
    NodeReadAt = 29,
    /// Change part of a file. `(file, offset, pointer, length)`.
    NodeWriteAt = 30,
    /// What time it is, in seconds since 1970. `()`.
    ///
    /// Separate from [`Call::Uptime`] because the two answer different
    /// questions and a program that confused them would be a program whose
    /// clock resets every boot. Uptime is a duration and is always available;
    /// this is a moment and depends on the machine having a clock at all.
    Now = 31,
    /// Fill a buffer with unpredictable bytes. `(pointer, length)`.
    ///
    /// The only source of unguessable bytes a program has, and the reason TLS
    /// can exist above it. It fails rather than returning something plausible
    /// on a machine whose processor has no hardware generator; see
    /// [`crate::random`] for why that refusal is the whole point.
    Random = 32,
    /// Send part of the display to the screen.
    /// `(framebuffer, x | y << 32, width | height << 32)`.
    ///
    /// Nothing on a machine whose framebuffer the firmware handed over: those
    /// pixels are the screen, and there is nowhere to send them. It matters on
    /// a machine whose display is a device the kernel drives, where the memory
    /// a program draws into is the guest's copy and the screen is the host's,
    /// and the two are the same only when somebody says so.
    ///
    /// The handle is not decoration. Drawing on this machine is done by having
    /// the framebuffer, and so is saying that the drawing is finished -- a
    /// program that cannot map the display has no business telling the display
    /// to show anything. It is checked for the same right that mapping needs,
    /// so the authority arrives the same way and can be taken away the same
    /// way.
    ///
    /// Two coordinates to a word because a system call takes four arguments and
    /// a rectangle is five things counting the handle. Packed rather than
    /// passed through memory, which would be a pointer to validate for four
    /// numbers.
    DisplayFlush = 33,
}

impl Call {
    /// How many calls exist.
    pub const COUNT: usize = 34;

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
            10 => Some(Self::MemoryCreate),
            11 => Some(Self::MemoryMap),
            12 => Some(Self::MemorySize),
            13 => Some(Self::NodeOpen),
            14 => Some(Self::NodeCreate),
            15 => Some(Self::NodeRemove),
            16 => Some(Self::NodeList),
            17 => Some(Self::NodeRead),
            18 => Some(Self::NodeWrite),
            19 => Some(Self::NodeSize),
            20 => Some(Self::ProcessWait),
            21 => Some(Self::WaitSetCreate),
            22 => Some(Self::WaitSetAdd),
            23 => Some(Self::WaitSetRemove),
            24 => Some(Self::WaitSetWait),
            25 => Some(Self::HandleDuplicate),
            26 => Some(Self::Sleep),
            27 => Some(Self::ProcessKill),
            28 => Some(Self::MemoryUnmap),
            29 => Some(Self::NodeReadAt),
            30 => Some(Self::NodeWriteAt),
            31 => Some(Self::Now),
            32 => Some(Self::Random),
            33 => Some(Self::DisplayFlush),
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
/// No such file or directory.
pub const ENOENT: u64 = u64::MAX - 8;
/// That name is already taken.
pub const EEXIST: u64 = u64::MAX - 9;
/// The buffer is not large enough for what would go in it.
pub const ETOOBIG: u64 = u64::MAX - 10;
/// The filesystem refused: it is full, damaged, busy, or absent.
pub const EFS: u64 = u64::MAX - 11;
/// The machine has no such device -- a clock, for instance.
pub const ENODEV: u64 = u64::MAX - 12;

/// Values at or above this are errors rather than results.
///
/// Four spare codes above the last one in use, so adding a call does not move
/// the boundary and make an old program read a new error as a length. The user
/// runtime carries the same number, and a call that could return a value this
/// large has to say so rather than let it be read as a failure.
pub const ERROR_BASE: u64 = u64::MAX - 15;

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

    // On the way in. A process that has been asked to stop does not get to make
    // another call: this is where a program that is busy rather than blocked
    // notices, and it is the earliest point at which it can, because the kernel
    // is holding nothing yet.
    crate::sched::stop_if_asked();

    // A program built for something else. The number in `rax` means what that
    // system says it means, so it goes to the layer that knows -- which turns
    // it into the same operations below that a Nexus call reaches, and nothing
    // more. The check is one comparison on a value the process was created
    // with; there is no sniffing and no guessing.
    if matches!(
        crate::sched::current_process().map(|process| process.personality),
        Some(crate::process::Personality::Linux)
    ) {
        let answer = crate::compat::linux::dispatch(
            number, argument0, argument1, argument2, argument3, argument4,
        );
        super::interrupts::disable();
        return answer;
    }

    let result = match Call::from_number(number) {
        Some(Call::Exit) => {
            match crate::sched::current_process() {
                Some(process) => {
                    // Recorded before the log line, so that a process whose
                    // parent is already waiting is woken as early as possible
                    // rather than after a serial write.
                    process.completion.finish(argument0);
                    // And everything it was holding goes back now, not when
                    // its thread is next reaped. A program's standard output is
                    // a channel somebody else is reading, and that reader
                    // learns the program has finished by the channel closing --
                    // so leaving it open until a five-second timer ran meant a
                    // shell that paused for five seconds after every command.
                    let released = process.handles.close_all();
                    kprintln!(
                        "[sys ] process {} \"{}\" exited with status {argument0} through the system-call boundary,                      {released} handles given back",
                        process.id,
                        process.name.as_str()
                    );
                }
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
        Some(Call::MemoryCreate) => memory_create(argument0),
        Some(Call::MemoryMap) => memory_map(argument0, argument1, argument2),
        Some(Call::MemorySize) => memory_size(argument0),
        Some(Call::NodeOpen) => node_open(argument0, argument1, argument2),
        Some(Call::NodeCreate) => node_create(argument0, argument1, argument2, argument3),
        Some(Call::NodeRemove) => node_remove(argument0, argument1, argument2),
        Some(Call::NodeList) => node_list(argument0, argument1, argument2),
        Some(Call::NodeRead) => node_read(argument0, argument1, argument2),
        Some(Call::NodeWrite) => node_write(argument0, argument1, argument2),
        Some(Call::NodeSize) => node_size(argument0),
        Some(Call::ProcessWait) => process_wait(argument0),
        Some(Call::WaitSetCreate) => wait_set_create(),
        Some(Call::WaitSetAdd) => wait_set_add(argument0, argument1, argument2),
        Some(Call::WaitSetRemove) => wait_set_remove(argument0, argument1),
        Some(Call::WaitSetWait) => wait_set_wait(argument0, argument1, argument2, argument3),
        Some(Call::HandleDuplicate) => handle_duplicate(argument0, argument1),
        Some(Call::Sleep) => sleep(argument0),
        Some(Call::ProcessKill) => process_kill(argument0),
        Some(Call::MemoryUnmap) => memory_unmap(argument0, argument1),
        Some(Call::NodeReadAt) => node_read_at(argument0, argument1, argument2, argument3),
        Some(Call::NodeWriteAt) => node_write_at(argument0, argument1, argument2, argument3),
        Some(Call::Now) => match crate::drivers::rtc::now() {
            Some(seconds) => seconds,
            // No clock. An error rather than zero, because zero is a real
            // moment -- the start of 1970 -- and a program that could not tell
            // the two apart would print 1970 on a machine with a dead battery
            // rather than saying it does not know.
            None => ENODEV,
        },
        Some(Call::Random) => random(argument0, argument1),
        Some(Call::DisplayFlush) => display_flush(argument0, argument1, argument2),
        None => {
            UNKNOWN.fetch_add(1, Ordering::Relaxed);
            kprintln!("[sys ] unimplemented system call {number}");
            ENOSYS
        }
    };

    // And on the way out. A call that blocked may have been woken *by* the kill
    // rather than by what it was waiting for, and returning its answer to a
    // process that no longer exists as far as anyone else is concerned would be
    // letting it run on after it was stopped.
    crate::sched::stop_if_asked();

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
pub fn user_range(pointer: u64, length: u64, limit: u64) -> Option<(u64, usize)> {
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
        crate::ipc::HandleError::WrongKind => EINVAL,
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

/// [`Call::MemoryCreate`]: set aside memory more than one process can see.
///
/// The caller gets a handle and nothing mapped. Mapping is a second step
/// because where it goes is the caller's business and because the handle is
/// what travels: a process hands the *handle* to another, and each maps it
/// wherever suits it.
fn memory_create(size: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(size) = usize::try_from(size) else {
        return EINVAL;
    };

    let Some(memory) = crate::ipc::MemoryObject::new(size) else {
        return EINVAL;
    };
    u64::from(
        process
            .handles
            .insert(crate::ipc::Object::Memory(memory), crate::ipc::Rights::ALL),
    )
}

/// [`Call::MemoryMap`]: put a memory object into the caller's address space.
///
/// The address is the caller's choice and is checked rather than trusted: page
/// aligned, wholly inside the user half, and not over anything already there.
/// Writability is asked for and granted only if the handle carries the right,
/// so a process can be handed memory it may read and not change.
fn memory_map(handle: u64, address: u64, writable: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };

    let wants_write = writable != 0;
    let needed = if wants_write {
        crate::ipc::Rights::READ | crate::ipc::Rights::WRITE
    } else {
        crate::ipc::Rights::READ
    };
    let memory = match process.handles.memory(handle, needed) {
        Ok(memory) => memory,
        Err(error) => return handle_error(error),
    };

    if !address.is_multiple_of(nexus_abi::layout::PAGE_SIZE) {
        return EINVAL;
    }
    let bytes = memory.pages() as u64 * nexus_abi::layout::PAGE_SIZE;
    let Some(end) = address.checked_add(bytes) else {
        return EINVAL;
    };
    if address == 0 || end > nexus_abi::layout::USER_SPACE_END {
        return EINVAL;
    }

    let mut flags = crate::memory::paging::USER
        | crate::memory::paging::NO_EXECUTE
        // The frames belong to the object, not to this address space. Without
        // this the second space to be dropped would free frames the first had
        // already returned.
        | crate::memory::paging::SHARED;
    if wants_write {
        flags |= crate::memory::paging::WRITABLE;
    }

    for index in 0..memory.pages() {
        let Some(frame) = memory.frame(index) else {
            return EINVAL;
        };
        let virt = address + index as u64 * nexus_abi::layout::PAGE_SIZE;

        // SAFETY: the frame belongs to an object this process holds a handle
        // to, and the address was checked to lie in the user half of this
        // process's own space.
        match unsafe { process.address_space.map(virt, frame, flags) } {
            Ok(()) => {}
            Err(crate::memory::paging::MapError::AlreadyMapped) => {
                // Undo the pages already mapped, so a request that collides
                // partway through leaves the caller as it found it.
                for done in 0..index {
                    let virt = address + done as u64 * nexus_abi::layout::PAGE_SIZE;
                    // SAFETY: mapped by this loop a moment ago, and marked
                    // shared, so unmapping does not free the frame.
                    unsafe {
                        let _ = crate::memory::paging::unmap_page_in(
                            process.address_space.root(),
                            virt,
                        );
                    }
                }
                return EINVAL;
            }
            Err(_) => return EINVAL,
        }
    }

    bytes
}

/// [`Call::MemorySize`]: how large a memory object is.
fn memory_size(handle: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    match process.handles.memory(handle, crate::ipc::Rights::READ) {
        Ok(memory) => memory.size() as u64,
        Err(error) => handle_error(error),
    }
}

/// Read a model-specific register.
///
/// # Safety
///
/// `register` must exist on this processor.
pub unsafe fn read_msr(register: u32) -> u64 {
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
pub unsafe fn write_msr(register: u32, value: u64) {
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

// -- Files ---------------------------------------------------------------------

/// Longest name a filesystem call will accept from ring 3.
///
/// What NexusFS itself allows. Bounded here as well because the length comes
/// from user memory, and a call is not entitled to make the kernel walk further
/// than the format could ever need.
const MAX_NAME: u64 = crate::fs::nexusfs::MAX_NAME as u64;

/// Largest buffer a filesystem call will read or write in one go.
///
/// The whole of the largest file the format can describe, so no legitimate
/// operation is refused by this, and a length far past it is refused before the
/// kernel touches any of it.
const MAX_TRANSFER: u64 = crate::fs::nexusfs::MAX_FILE as u64;

/// Turn a filesystem refusal into the value the caller sees.
///
/// Deliberately not one code per cause. A program needs to tell "there is no
/// such name" and "that name is taken" apart, because both are ordinary
/// outcomes it will act on; the rest -- a full disk, a corrupt directory, an
/// unreachable disk -- are all "the filesystem said no", and inventing a code
/// per internal condition would be publishing the implementation as an
/// interface. The kernel log carries the detail.
fn store_error(error: crate::fs::store::StoreError) -> u64 {
    use crate::fs::nexusfs::FsError;
    use crate::fs::store::StoreError;
    match error {
        StoreError::Fs(FsError::NotFound) => ENOENT,
        StoreError::Fs(FsError::Exists) => EEXIST,
        StoreError::Fs(FsError::BadName | FsError::WrongKind) => EINVAL,
        StoreError::Fs(FsError::TooLarge) => EMSGSIZE,
        _ => EFS,
    }
}

/// Read a name out of user memory.
fn user_name<'a>(pointer: u64, length: u64) -> Option<&'a str> {
    let (pointer, length) = user_range(pointer, length, MAX_NAME)?;
    // SAFETY: the range was checked to lie wholly inside the user half, which
    // this thread's address space maps. An unmapped range faults, which is the
    // caller's own page fault.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    core::str::from_utf8(bytes).ok()
}

/// The node a handle names, together with what that handle is good for.
///
/// Both at once, because every call below needs the rights as well: a handle
/// opened through a read-only directory has to stay read-only, and the only
/// place that can be decided is where the directory handle is looked up.
fn caller_node(
    handle: u64,
    needed: crate::ipc::Rights,
) -> Result<(alloc::sync::Arc<crate::fs::store::Node>, crate::ipc::Rights), u64> {
    let process = caller()?;
    let handle = u32::try_from(handle).map_err(|_| EBADF)?;
    let node = process.handles.node(handle, needed).map_err(handle_error)?;
    let rights = process.handles.rights(handle).map_err(handle_error)?;
    Ok((node, rights))
}

/// Put a node in the caller's table, returning the handle.
fn insert_node(node: alloc::sync::Arc<crate::fs::store::Node>, rights: crate::ipc::Rights) -> u64 {
    let Ok(process) = caller() else {
        return ENOPROC;
    };
    // Inherited from the directory it was reached through -- a program cannot
    // open its way to more authority than it was given -- plus the right to let
    // go of it.
    //
    // Closing is not an authority over the file. It is disposing of a reference
    // this call has just handed the caller, and withholding it protects nobody:
    // it makes a handle that can never be released, whose inode can therefore
    // never be removed, by anybody, for as long as the process lives. That is
    // not attenuation, it is a leak with a capability argument attached to it.
    //
    // It was a real one. `init` hands the installer a directory with read,
    // write and transfer and no close; the installer opened a file it was
    // replacing, read it, could not close it, and the removal underneath was
    // then refused because something still had it open. Installing the same
    // package twice failed on every machine, and the error it gave named the
    // wrong step: "cannot create: Exists".
    let rights = rights | crate::ipc::Rights::CLOSE;
    u64::from(
        process
            .handles
            .insert(crate::ipc::Object::Node(node), rights),
    )
}

/// [`Call::NodeOpen`]: open one name inside a directory.
///
/// One component, never a path. A directory handle is the authority to reach
/// what is under it, and a call that took `../..` would make that authority
/// mean nothing -- so the separator is refused by the name check rather than
/// interpreted.
///
/// The new handle carries no more than the one it came from. Opening through a
/// read-only directory cannot produce something writable, which is what makes
/// handing a program a read-only directory mean anything.
fn node_open(directory: u64, pointer: u64, length: u64) -> u64 {
    let (node, rights) = match caller_node(directory, crate::ipc::Rights::READ) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let Some(name) = user_name(pointer, length) else {
        return EINVAL;
    };

    match crate::fs::store::open_child(&node, name) {
        Ok(child) => insert_node(child, rights),
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeCreate`]: make a file or directory, and open it.
fn node_create(directory: u64, pointer: u64, length: u64, is_directory: u64) -> u64 {
    let (node, rights) = match caller_node(directory, crate::ipc::Rights::WRITE) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let Some(name) = user_name(pointer, length) else {
        return EINVAL;
    };

    match crate::fs::store::create_child(&node, name, is_directory != 0) {
        Ok(child) => insert_node(child, rights),
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeRemove`]: remove a name, and the thing it named.
fn node_remove(directory: u64, pointer: u64, length: u64) -> u64 {
    let (node, _) = match caller_node(directory, crate::ipc::Rights::WRITE) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let Some(name) = user_name(pointer, length) else {
        return EINVAL;
    };

    match crate::fs::store::remove_child(&node, name) {
        Ok(()) => 0,
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeList`]: write a directory's entries into a user buffer.
///
/// The entries go out in the shape they have on disk -- four bytes of inode
/// number, one of name length, one of kind, two spare, then the name -- because
/// a second layout would be a second thing to keep in step with the first for
/// no gain.
fn node_list(directory: u64, pointer: u64, capacity: u64) -> u64 {
    let (node, _) = match caller_node(directory, crate::ipc::Rights::READ) {
        Ok(found) => found,
        Err(error) => return error,
    };
    let entries = match crate::fs::store::entries(&node) {
        Ok(entries) => entries,
        Err(error) => return store_error(error),
    };

    let mut packed = alloc::vec::Vec::new();
    for entry in &entries {
        packed.extend_from_slice(&entry.inode.to_le_bytes());
        packed.push(entry.name.len() as u8);
        packed.push(match entry.kind {
            crate::fs::nexusfs::Kind::Directory => 2,
            _ => 1,
        });
        packed.extend_from_slice(&[0, 0]);
        packed.extend_from_slice(entry.name.as_bytes());
    }

    copy_out(&packed, pointer, capacity)
}

/// [`Call::NodeRead`]: read a whole file into a user buffer.
fn node_read(file: u64, pointer: u64, capacity: u64) -> u64 {
    let (node, _) = match caller_node(file, crate::ipc::Rights::READ) {
        Ok(found) => found,
        Err(error) => return error,
    };
    match crate::fs::store::read_node(&node) {
        Ok(bytes) => copy_out(&bytes, pointer, capacity),
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeWrite`]: replace a whole file from a user buffer.
///
/// A whole file, because that is what the filesystem underneath offers: there
/// is no buffer cache, so a partial write would be a partial write to the disk.
/// A zero length is a legitimate call -- it empties the file -- so unlike every
/// other length here it is allowed.
fn node_write(file: u64, pointer: u64, length: u64) -> u64 {
    let (node, _) = match caller_node(file, crate::ipc::Rights::WRITE) {
        Ok(found) => found,
        Err(error) => return error,
    };

    let data: alloc::vec::Vec<u8> = if length == 0 {
        alloc::vec::Vec::new()
    } else {
        let Some((pointer, length)) = user_range(pointer, length, MAX_TRANSFER) else {
            return EINVAL;
        };
        // SAFETY: the range lies inside the user half, which this thread's
        // address space maps. It is copied before the filesystem is touched, so
        // nothing below this holds a pointer into user memory across a call
        // that can block and let the caller unmap it.
        unsafe { core::slice::from_raw_parts(pointer as *const u8, length) }.to_vec()
    };

    match crate::fs::store::write_node(&node, &data) {
        Ok(()) => data.len() as u64,
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeSize`]: how many bytes a file or directory holds.
fn node_size(handle: u64) -> u64 {
    let (node, _) = match caller_node(handle, crate::ipc::Rights::READ) {
        Ok(found) => found,
        Err(error) => return error,
    };
    match crate::fs::store::size(&node) {
        Ok(size) => size,
        Err(error) => store_error(error),
    }
}

/// Copy bytes out to a user buffer, saying how many there were.
///
/// A buffer too small is an error and not a truncation. Half a file that
/// reports its own length looks exactly like a whole one, and a program that
/// acted on it would act on a fragment.
fn copy_out(bytes: &[u8], pointer: u64, capacity: u64) -> u64 {
    if bytes.len() as u64 > capacity {
        return ETOOBIG;
    }
    if bytes.is_empty() {
        return 0;
    }
    let Some((pointer, length)) = user_range(pointer, bytes.len() as u64, MAX_TRANSFER) else {
        return EINVAL;
    };
    // SAFETY: the range was checked to lie wholly inside the user half, which
    // this thread's address space maps, and is exactly as long as what is being
    // written into it.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, length);
    }
    bytes.len() as u64
}

// -- Randomness ----------------------------------------------------------------

/// The most unpredictable bytes one call will produce.
///
/// Enough for any key this machine makes -- a TLS client needs two lots of
/// thirty-two -- and small enough that the scratch buffer lives on the kernel
/// stack. A program that wants more asks again, which is free.
const MAX_RANDOM: u64 = 256;

/// [`Call::Random`]: fill a user buffer with unpredictable bytes.
///
/// Returns the length on success, and [`ENODEV`] when this processor has no
/// hardware generator. That refusal is the point of the call rather than a
/// shortcoming of it: the alternative is handing back something derived from
/// the clock, which would let every TLS connection this machine makes be
/// Send a rectangle of the display to the screen.
///
/// The handle has to be a memory object this process may write, which is the
/// same right mapping the framebuffer needs. That is the whole permission
/// check and it is the right one: on this machine the authority to draw *is*
/// having the framebuffer, so the authority to say "I have drawn" is the same
/// object. A process that was never lent the display cannot make the display
/// do anything.
///
/// It is not checked that the handle is *the* framebuffer. It does not need to
/// be: `display::flush` sends the display's own memory whatever it is handed,
/// so the worst a process with some other memory object can do is ask for a
/// rectangle of the screen to be shown again, which is what the screen is
/// already showing.
fn display_flush(handle: u64, origin: u64, size: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    if let Err(error) = process
        .handles
        .memory(handle, crate::ipc::Rights::READ | crate::ipc::Rights::WRITE)
    {
        return handle_error(error);
    }

    // Two per word, low half first.
    let x = origin as u32;
    let y = (origin >> 32) as u32;
    let width = size as u32;
    let height = (size >> 32) as u32;
    crate::display::flush(x, y, width, height);
    0
}

/// broken by somebody who knows roughly when it booted.
///
/// Nothing is written unless everything was produced. [`crate::random::bytes`]
/// is all-or-nothing for the same reason -- half a key is not a weaker key, it
/// is a key with a known half -- and this preserves that all the way out to the
/// caller.
fn random(pointer: u64, length: u64) -> u64 {
    if length == 0 || length > MAX_RANDOM {
        return EINVAL;
    }
    let mut scratch = [0u8; MAX_RANDOM as usize];
    let wanted = length as usize;
    if !crate::random::bytes(&mut scratch[..wanted]) {
        return ENODEV;
    }
    let written = copy_out(&scratch[..wanted], pointer, length);
    // The bytes were real key material a moment ago and this stack frame is
    // about to be reused by whatever runs next on this kernel stack.
    scratch.fill(0);
    written
}

// -- Processes -----------------------------------------------------------------

/// [`Call::ProcessWait`]: block until a process ends, and take its status.
///
/// The handle is the authority. There is no call that waits on a process
/// identifier, because an identifier is a number a program could guess and a
/// handle is something it had to be given -- the same argument as everywhere
/// else here, applied to the question "is it done yet".
///
/// A process that has already ended returns immediately, which is the case that
/// matters most: a parent that reads the answer after the child has exited must
/// get the answer and not wait forever for an event that has been and gone.
///
/// The status is whatever the process passed to [`Call::Exit`], and the kernel
/// attaches no meaning to it. Zero conventionally means it worked, because that
/// is what every program here writes, not because anything enforces it.
fn process_wait(handle: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let completion = match process.handles.process(handle, crate::ipc::Rights::READ) {
        Ok(completion) => completion,
        Err(error) => return handle_error(error),
    };

    // The Arc is cloned out of the table above, so the wait below does not hold
    // the handle table's lock -- which it must not, because waking this thread
    // means the exiting process is running code that touches its own table, and
    // a thread blocked holding a lock that a waker needs is a system that stops.
    let status = completion.wait();

    // A status is a small number and errors live at the top of the range. A
    // process that returned one of those would be reported as an error to its
    // parent, so it is clamped and the clamping is visible in the log rather
    // than silent.
    if status >= ERROR_BASE {
        kprintln!(
            "[sys ] process {} \"{}\" exited with {status:#x}, which is not a status a caller can be given",
            completion.id,
            completion.name.as_str()
        );
        return EINVAL;
    }
    status
}

// -- Wait sets -----------------------------------------------------------------

/// Bytes one key takes in the buffer [`Call::WaitSetWait`] fills.
const KEY_SIZE: u64 = 8;

/// The timeout that is not a timeout.
///
/// Spelled as the largest number rather than as zero, because zero is a real
/// answer to "how long will you wait" and forever is not a duration at all.
const FOREVER: u64 = u64::MAX;

/// Turn a wait-set refusal into the value the caller sees.
fn waitset_error(error: crate::waitset::WaitSetError) -> u64 {
    use crate::waitset::WaitSetError;
    match error {
        WaitSetError::Full => EMSGSIZE,
        WaitSetError::DuplicateKey => EEXIST,
        WaitSetError::NoSuchKey => ENOENT,
    }
}

/// The wait set a handle names, with the rights the operation needs.
fn caller_set(
    handle: u64,
    needed: crate::ipc::Rights,
) -> Result<alloc::sync::Arc<crate::waitset::WaitSet>, u64> {
    let process = caller()?;
    let handle = u32::try_from(handle).map_err(|_| EBADF)?;
    process
        .handles
        .wait_set(handle, needed)
        .map_err(handle_error)
}

/// [`Call::WaitSetCreate`]: somewhere to wait for several things at once.
fn wait_set_create() -> u64 {
    let Ok(process) = caller() else {
        return ENOPROC;
    };
    let set = alloc::sync::Arc::new(crate::waitset::WaitSet::new());
    u64::from(
        process
            .handles
            .insert(crate::ipc::Object::WaitSet(set), crate::ipc::Rights::ALL),
    )
}

/// [`Call::WaitSetAdd`]: watch a handle, under a key of the caller's choosing.
///
/// The key is the caller's and not the kernel's. A wait that answered with
/// handle numbers would make the answer a thing to look up in a table the
/// program keeps anyway; a key is whatever the program already calls that
/// client, and comes back unchanged.
///
/// Adding needs the right to read the thing being watched, because knowing that
/// a message has arrived is most of the way to reading it: a set that would
/// watch a handle its holder cannot read would leak the timing of everything
/// happening on it.
fn wait_set_add(set: u64, handle: u64, key: u64) -> u64 {
    let set = match caller_set(set, crate::ipc::Rights::WRITE) {
        Ok(set) => set,
        Err(error) => return error,
    };
    let Ok(process) = caller() else {
        return ENOPROC;
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };

    // A channel or a process. Everything else is either always ready or never
    // becomes ready, and adding one would be a program waiting for something
    // that cannot arrive.
    let watched = match process.handles.watchable(handle, crate::ipc::Rights::READ) {
        Ok(watched) => watched,
        Err(error) => return handle_error(error),
    };

    match set.add(key, watched) {
        Ok(()) => 0,
        Err(error) => waitset_error(error),
    }
}

/// [`Call::WaitSetRemove`]: stop watching whatever has this key.
fn wait_set_remove(set: u64, key: u64) -> u64 {
    let set = match caller_set(set, crate::ipc::Rights::WRITE) {
        Ok(set) => set,
        Err(error) => return error,
    };
    match set.remove(key) {
        Ok(()) => 0,
        Err(error) => waitset_error(error),
    }
}

/// [`Call::WaitSetWait`]: block until something is ready, and say what.
///
/// Every ready key is returned, not the first: a caller that got one at a time
/// would make a system call per ready client, which is the cost a wait set
/// exists to avoid.
///
/// An empty set returns zero rather than blocking. Nothing can ever make it
/// ready, so waiting would be waiting forever -- the same judgement `receive`
/// makes about a channel whose peer has gone.
///
/// `milliseconds` is how long to wait at most, with `u64::MAX` meaning forever.
/// A wait that ran out of time returns zero, the same as an empty set and the
/// same as a process being asked to stop: all three mean "nothing you asked
/// about is ready", and a caller that cares which can tell from what it put in
/// the set and what the clock says. The bound is the same one `sleep` uses --
/// a timeout of four hundred million years is indistinguishable from a hang to
/// whoever is reading the log.
fn wait_set_wait(set: u64, pointer: u64, capacity: u64, milliseconds: u64) -> u64 {
    let set = match caller_set(set, crate::ipc::Rights::READ) {
        Ok(set) => set,
        Err(error) => return error,
    };

    // Checked before blocking, so a caller with a buffer that could never hold
    // the answer is told so now rather than after an unbounded wait.
    let room = capacity / KEY_SIZE;
    if room == 0 {
        return ETOOBIG;
    }

    let deadline = if milliseconds == FOREVER {
        None
    } else {
        if milliseconds > MAX_SLEEP_MS {
            return EINVAL;
        }
        Some(crate::arch::time::ticks() + crate::arch::time::ms_to_ticks(milliseconds))
    };

    let ready = set.wait_until(deadline);
    if ready.is_empty() {
        return 0;
    }
    if ready.len() as u64 > room {
        return ETOOBIG;
    }

    let mut packed = alloc::vec::Vec::with_capacity(ready.len() * KEY_SIZE as usize);
    for key in &ready {
        packed.extend_from_slice(&key.to_le_bytes());
    }
    if copy_out(&packed, pointer, capacity) >= ERROR_BASE {
        return EINVAL;
    }
    ready.len() as u64
}

/// [`Call::HandleDuplicate`]: another handle to the same object.
///
/// What a program needs to hand something on and keep it. A process that gives
/// a client a buffer without this has *given it away*: handles move when they
/// cross a channel, so the object would die with the client and take the frames
/// with it, out from under anyone else still mapping them.
///
/// Rights can only be dropped. A duplicate that could add one would make every
/// handle equal to the most powerful handle in the system, which is a
/// capability system undone in a single call -- so the requested set has to be
/// a subset of what the original carries, and asking for more is refused rather
/// than quietly trimmed.
fn handle_duplicate(handle: u64, rights: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let Ok(rights) = u32::try_from(rights) else {
        return EINVAL;
    };

    match process
        .handles
        .duplicate(handle, crate::ipc::Rights::from_bits(rights))
    {
        Ok(new) => u64::from(new),
        Err(error) => handle_error(error),
    }
}

/// Longest a single call will sleep for.
///
/// A bound rather than a trust. The argument comes from ring 3, and a thread
/// asked to sleep for four hundred million years is a thread that never comes
/// back -- which is a program's own business, except that it looks exactly like
/// a hang to whoever is reading the boot log.
const MAX_SLEEP_MS: u64 = 60 * 1000;

/// [`Call::Sleep`]: stop running for a while.
///
/// The thread leaves the run queues entirely and is put back when the tick
/// counter passes its deadline, so a sleeping program costs nothing. That is
/// the difference between this and a loop that yields, which costs a context
/// switch every time round for as long as it lasts.
fn sleep(milliseconds: u64) -> u64 {
    if milliseconds > MAX_SLEEP_MS {
        return EINVAL;
    }
    if milliseconds > 0 {
        crate::sched::sleep_ms(milliseconds);
    }
    0
}

/// [`Call::ProcessKill`]: ask a process to stop.
///
/// The handle is the authority, as everywhere: a process that was never handed
/// one cannot stop anything, and there is no identifier it could use instead.
/// Write, not read -- being able to *watch* something end is not the same right
/// as being able to end it, and a program handed a read-only process handle can
/// wait for it and nothing more.
///
/// Returns at once. Stopping is asking: the flag is set, everything the process
/// had asleep is woken, and its threads leave when they notice. A caller that
/// wants to know it has actually gone waits for it, which is what the handle is
/// also for.
fn process_kill(handle: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let completion = match process.handles.process(handle, crate::ipc::Rights::WRITE) {
        Ok(completion) => completion,
        Err(error) => return handle_error(error),
    };

    // Already finished. Not an error: a caller that asks twice, or asks about
    // something that ended while it was deciding to, has got what it wanted.
    if completion.status().is_some() {
        return 0;
    }

    if completion.cancel() {
        let woken = crate::sched::wake_process_threads(completion.id);
        kprintln!(
            "[sys ] process {} \"{}\" was asked to stop; {woken} of its threads woken to notice",
            completion.id,
            completion.name.as_str()
        );
    }
    0
}

/// [`Call::MemoryUnmap`]: take a memory object out of this address space.
///
/// What a program needs to map something *else* where it was. Without it an
/// address, once used, is used forever -- and a program handed a replacement
/// for something it already maps has to put the new one somewhere else and
/// leak the old address, which is how a window that is resized often runs out
/// of address space rather than out of memory.
///
/// The handle says *what* to unmap, and it has to: the frames belong to the
/// object rather than to this space, and unmapping the wrong number of pages
/// would leave part of it mapped or take a page out from under whatever was
/// next. The frames are not freed -- they belong to the object, which is still
/// alive as long as somebody holds a handle to it.
fn memory_unmap(handle: u64, address: u64) -> u64 {
    let process = match caller() {
        Ok(process) => process,
        Err(error) => return error,
    };
    let Ok(handle) = u32::try_from(handle) else {
        return EBADF;
    };
    let memory = match process.handles.memory(handle, crate::ipc::Rights::READ) {
        Ok(memory) => memory,
        Err(error) => return handle_error(error),
    };

    if !address.is_multiple_of(nexus_abi::layout::PAGE_SIZE) || address == 0 {
        return EINVAL;
    }
    let bytes = memory.pages() as u64 * nexus_abi::layout::PAGE_SIZE;
    let Some(end) = address.checked_add(bytes) else {
        return EINVAL;
    };
    if end > nexus_abi::layout::USER_SPACE_END {
        return EINVAL;
    }

    // Every page has to be one this object is actually mapped at, checked
    // before any of them is removed. An address that happens to be mapped to
    // something else is not this object, and unmapping it would be unmapping
    // whatever the caller really had there.
    for index in 0..memory.pages() {
        let virt = address + index as u64 * nexus_abi::layout::PAGE_SIZE;
        let Some(frame) = memory.frame(index) else {
            return EINVAL;
        };
        if process.address_space.translate(virt) != Some(frame) {
            return EINVAL;
        }
    }

    for index in 0..memory.pages() {
        let virt = address + index as u64 * nexus_abi::layout::PAGE_SIZE;
        // SAFETY: checked above to be exactly this object's pages in this
        // process's own space. The frame is not freed: it belongs to the
        // object, which outlives this mapping.
        unsafe {
            let _ = process.address_space.unmap(virt);
        }
    }

    bytes
}

/// [`Call::NodeReadAt`]: read part of a file.
///
/// Short at the end of the file rather than an error: a caller that asks for
/// more than is there has reached the end, which is a thing that happens and
/// not a thing that went wrong. Zero means there was nothing at that offset.
fn node_read_at(file: u64, offset: u64, pointer: u64, capacity: u64) -> u64 {
    let (node, _) = match caller_node(file, crate::ipc::Rights::READ) {
        Ok(found) => found,
        Err(error) => return error,
    };
    if capacity == 0 {
        return 0;
    }
    let Some((_, length)) = user_range(pointer, capacity, MAX_TRANSFER) else {
        return EINVAL;
    };

    // Read into the kernel's own buffer and copied out afterwards, rather than
    // read straight into the caller's. The filesystem can block, and a caller
    // that unmapped the range while it did would have the disk write into
    // whatever took its place.
    let mut buffer = alloc::vec![0u8; length];
    match crate::fs::store::read_node_at(&node, offset, &mut buffer) {
        Ok(0) => 0,
        Ok(read) => copy_out(&buffer[..read], pointer, capacity),
        Err(error) => store_error(error),
    }
}

/// [`Call::NodeWriteAt`]: change part of a file.
///
/// Writing past the end grows the file, and the gap reads as zeroes -- a block
/// is zeroed when it is allocated, so a file with a hole in it cannot show
/// whatever the last file to own that block left there.
fn node_write_at(file: u64, offset: u64, pointer: u64, length: u64) -> u64 {
    let (node, _) = match caller_node(file, crate::ipc::Rights::WRITE) {
        Ok(found) => found,
        Err(error) => return error,
    };
    if length == 0 {
        return 0;
    }
    let Some((pointer, length)) = user_range(pointer, length, MAX_TRANSFER) else {
        return EINVAL;
    };

    // SAFETY: the range lies inside the user half, which this thread's address
    // space maps. Copied before the filesystem is touched, so nothing below
    // holds a pointer into user memory across a call that can block.
    let data = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) }.to_vec();

    match crate::fs::store::write_node_at(&node, offset, &data) {
        Ok(written) => written,
        Err(error) => store_error(error),
    }
}
