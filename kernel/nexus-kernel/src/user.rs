//! Ring 3, and the processes that run there.
//!
//! The first code NexusOS runs that the kernel does not trust. The programs are
//! small on purpose: the point of this module is not what they do but that the
//! boundary around them is real. Each has an address space of its own, pages it
//! cannot write, a stack it cannot execute, and one way back into the kernel.
//!
//! # What each program is for
//!
//! `abi` exercises the system-call interface: it makes every call the kernel
//! implements, asks for one it does not, and checks that a callee-saved
//! register survived the round trip.
//!
//! `alpha` and `beta` run the *same* program at the *same* virtual addresses in
//! different address spaces. Each writes its own identifier to one address over
//! and over and reads it back. Two processes that shared a page would clobber
//! one another within the first few iterations, so the check passing is a
//! statement about isolation rather than about two programs being able to run.

use alloc::sync::Arc;

use nexus_abi::layout;

use crate::arch::gdt;
use crate::drivers;
use crate::fs;
use crate::ipc;
use crate::memory::{address_space, paging};
use crate::{kprintln, memory, sched};

/// Where a user program is mapped.
///
/// Four megabytes in: low enough to be obviously user space, high enough that a
/// null dereference in ring 3 is nowhere near it. The same in every process, on
/// purpose -- identical layouts are what make the isolation test mean something.
const CODE_BASE: u64 = 0x0000_0000_0040_0000;

/// The page a program keeps its own data in.
const DATA_BASE: u64 = 0x0000_0000_0060_0000;

/// One page below the top of the user stack region.
const STACK_TOP: u64 = 0x0000_0000_0080_0000;

/// How far below [`STACK_TOP`] a process starts.
///
/// Sixteen rather than eight so the initial stack pointer keeps the alignment
/// compiled code will expect once these programs are written in something other
/// than assembly.
const INITIAL_STACK_OFFSET: u64 = 16;

const _: () = {
    assert!(CODE_BASE < layout::USER_SPACE_END);
    assert!(DATA_BASE < layout::USER_SPACE_END);
    assert!(STACK_TOP < layout::USER_SPACE_END);
};

// AT&T syntax, unusually for this codebase, and for one reason: the program
// needs the *length* of a string as an immediate, and Intel syntax has no way
// to write a difference of two labels that an assembler will not read as a
// memory reference. `$(1b - 0b)` says what is meant; `mov esi, 1f - 0f` does
// not assemble.
#[cfg(not(feature = "inject-user-violation"))]
core::arch::global_asm!(
    r#"
    .section .rodata
    .p2align 4
    .global nexus_user_abi_start
    .global nexus_user_abi_end
nexus_user_abi_start:
    // Greet the kernel across the boundary. The message address is formed with
    // `lea` off `rip` rather than written absolutely: the program is assembled
    // into the kernel image and runs from a different virtual address, so only
    // RIP-relative references survive being copied somewhere else.
    movl $1, %eax                       // Call::Log
    leaq 1f(%rip), %rdi
    movl $(2f - 1f), %esi
    syscall

    // Keep the thread identifier somewhere the ABI promises to preserve, and
    // then make more calls. If `rbx` came back changed, the entry stub would be
    // losing registers it said it would not.
    movl $4, %eax                       // Call::ThreadId
    syscall
    movq %rax, %rbx

    movl $2, %eax                       // Call::Uptime
    syscall

    movl $3, %eax                       // Call::Yield
    syscall

    // Burn a measurable amount of time in ring 3. Not filler: every system
    // call so far is one the kernel could have serviced for code running at
    // any privilege, so nothing yet proves this program is at ring 3 at all.
    // A spin long enough to be preempted forces timer interrupts to arrive
    // *from* user mode, and the kernel counts those.
    movl $30000000, %ecx
7:
    dec %ecx
    jnz 7b

    // A number the kernel does not implement. Ring 3 asking for something that
    // does not exist has to come back as an error, not as a fault.
    movl $0x7fffffff, %eax
    syscall

    // Checked rather than trusted: if `rbx` no longer holds the identifier, say
    // so across the boundary instead of exiting quietly.
    movl $4, %eax
    syscall
    cmpq %rbx, %rax
    jne 8f

    movl $1, %eax
    leaq 2f(%rip), %rdi
    movl $(3f - 2f), %esi
    syscall
    jmp 9f
8:
    movl $1, %eax
    leaq 3f(%rip), %rdi
    movl $(4f - 3f), %esi
    syscall
9:
    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    // Exit does not return. If it ever did, fault here rather than run on into
    // the message data.
    ud2

1:  .ascii "hello from ring 3"
2:  .ascii "made six system calls; callee-saved registers survived"
3:  .ascii "FAILED: a callee-saved register did not survive a system call"
4:
nexus_user_abi_end:
"#,
    options(att_syntax)
);

// A user program that reaches for memory that is not its own.
//
// The ordinary program demonstrates that the boundary can be *crossed*. It
// cannot demonstrate that the boundary is *there*: a kernel that mapped
// everything readable from ring 3 would run it identically. Only an access that
// must fail, and does, says the protection exists -- and the way to find out is
// to make it.
#[cfg(feature = "inject-user-violation")]
core::arch::global_asm!(
    r#"
    .section .rodata
    .p2align 4
    .global nexus_user_abi_start
    .global nexus_user_abi_end
nexus_user_abi_start:
    movl $1, %eax                       // Call::Log
    leaq 1f(%rip), %rdi
    movl $(2f - 1f), %esi
    syscall

    // The base of the kernel image: mapped, present, and without the user bit.
    // Ring 3 reading it must take a page fault.
    movabsq $0xffffffff80000000, %rax
    movq (%rax), %rbx

    // Only reachable if the read succeeded, which would mean the kernel is
    // readable from ring 3.
    movl $1, %eax
    leaq 2f(%rip), %rdi
    movl $(3f - 2f), %esi
    syscall

    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    ud2

1:  .ascii "about to read kernel memory from ring 3"
2:  .ascii "FAILED: ring 3 read kernel memory and was allowed to"
3:
nexus_user_abi_end:
"#,
    options(att_syntax)
);

// The program `alpha` and `beta` both run.
//
// Its data page is an interface with the kernel, and the layout is the whole of
// it: eight bytes of identifier, eight the program writes and reads back, eight
// of message length, then the message. Nothing is passed in registers, because
// nothing needs to be -- a process that can see its own page has everything.
core::arch::global_asm!(
    r#"
    .section .rodata
    .p2align 4
    .global nexus_user_isolation_start
    .global nexus_user_isolation_end
nexus_user_isolation_start:
    // The identifier comes off this process's own stack, which the kernel
    // seeded before entering ring 3. Not from the data page: that is the page
    // under test, and a test whose subject also supplies the expected answer
    // cannot fail -- two processes sharing it would read the same identifier,
    // write the same value, and congratulate each other.
    movq (%rsp), %r12
    movabsq $0x600000, %r13             // the data page
    movl $200, %r14d                    // rounds to run

2:
    movq %r12, 8(%r13)                  // claim the word

    // Spin long enough that the other process is scheduled in between. Without
    // this the two might never interleave, and a test that cannot fail is not
    // a test.
    movl $200000, %ecx
1:
    dec %ecx
    jnz 1b

    movq 8(%r13), %rax                  // and read it back
    cmpq %r12, %rax
    jne 3f
    dec %r14d
    jnz 2b

    // Survived every round. Report using the message from this process's own
    // page, which is how the log shows which process is speaking.
    movl $1, %eax                       // Call::Log
    leaq 24(%r13), %rdi
    movq 16(%r13), %rsi
    syscall
    jmp 4f
3:
    movl $1, %eax
    leaq 5f(%rip), %rdi
    movl $(6f - 5f), %esi
    syscall
4:
    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    ud2

5:  .ascii "FAILED: another process wrote into this one's memory"
6:
nexus_user_isolation_end:
"#,
    options(att_syntax)
);

// The program `ipc` runs: a channel, end to end, from ring 3.
//
// It checks the three things a capability system has to get right and that no
// amount of reading the kernel proves: a message written on one end comes out
// of the other, a handle nobody was given is refused, and an end whose peer has
// been closed reports that rather than swallowing the write.
core::arch::global_asm!(
    r#"
    .section .rodata
    .p2align 4
    .global nexus_user_ipc_start
    .global nexus_user_ipc_end
nexus_user_ipc_start:
    // Two handles come back packed into one word, the first in the high half.
    // A call that returns two values otherwise needs a user buffer, which needs
    // checking, for two integers.
    movl $5, %eax                       // Call::ChannelCreate
    syscall
    movq %rax, %r15
    movq %r15, %r12
    shrq $32, %r12                      // the writing end
    movl %r15d, %r13d                   // the reading end

    // Write on one end. The last two arguments say "no handles"; they are
    // registers like any other, so they are cleared rather than left to hold
    // whatever the previous call did.
    movl $6, %eax                       // Call::ChannelWrite
    movq %r12, %rdi
    leaq 1f(%rip), %rsi
    movq $(2f - 1f), %rdx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    cmpq $(2f - 1f), %rax
    jne 8f

    // And read it out of the other, into this process's data page.
    movl $7, %eax                       // Call::ChannelRead
    movq %r13, %rdi
    movabsq $0x600000, %rsi
    movl $256, %edx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    cmpq $(2f - 1f), %rax
    jne 8f

    // Say what came back, from the buffer rather than from the source, so the
    // log shows what crossed the channel and not what was sent.
    movl $1, %eax                       // Call::Log
    movabsq $0x600000, %rdi
    movq $(2f - 1f), %rsi
    syscall

    // A handle this process was never given must be refused. -3 is EBADF,
    // which is u64::MAX - 2.
    movl $9, %eax                       // Call::HandleRights
    movq $9999, %rdi
    syscall
    cmpq $-3, %rax
    jne 8f

    // Closing the reading end drops the last reference to it, which is what
    // the writing end sees as its peer going away.
    movl $8, %eax                       // Call::HandleClose
    movq %r13, %rdi
    syscall
    testq %rax, %rax
    jnz 8f

    movl $6, %eax                       // Call::ChannelWrite, again
    movq %r12, %rdi
    leaq 1f(%rip), %rsi
    movq $(2f - 1f), %rdx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    cmpq $-5, %rax                      // EPIPE, u64::MAX - 4
    jne 8f

    movl $1, %eax
    leaq 2f(%rip), %rdi
    movl $(3f - 2f), %esi
    syscall
    jmp 9f
8:
    movl $1, %eax
    leaq 3f(%rip), %rdi
    movl $(4f - 3f), %esi
    syscall
9:
    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    ud2

1:  .ascii "hello across a channel"
2:  .ascii "channel round trip, bad handle and closed peer all behaved"
3:  .ascii "FAILED: a channel system call did not behave as documented"
4:
nexus_user_ipc_end:
"#,
    options(att_syntax)
);

// The two programs that talk to each other.
//
// Both are given one end of a channel by the kernel before they start, as
// handle 1 -- the first thing in an empty table. Neither can name the other,
// and neither needs to: the authority to talk is the handle itself.
//
// What they do with it is the point. The client makes a *second* channel and
// sends one end of it down the first, so the conversation continues somewhere
// the kernel never arranged. That is what a handle in a message buys: a process
// can hand on authority it holds, and two processes can end up connected by
// something neither of them was born with.
//
// The handle array lives in the data page rather than in `.rodata`, because the
// kernel has to read the numbers out of writable memory the program filled in.
core::arch::global_asm!(
    r#"
    .section .rodata
    .p2align 4
    .global nexus_user_client_start
    .global nexus_user_client_end
nexus_user_client_start:
    // A channel of its own. The high half of the result is one end, the low
    // half the other.
    movl $5, %eax                       // Call::ChannelCreate
    syscall
    movq %rax, %r15
    movq %r15, %r14
    shrq $32, %r14                      // the end to keep
    movl %r15d, %r13d                   // the end to give away

    // Hand that end to the server, with a note saying what it is.
    movabsq $0x600100, %rbx
    movl %r13d, (%rbx)
    movl $6, %eax                       // Call::ChannelWrite
    movl $1, %edi
    leaq 1f(%rip), %rsi
    movq $(2f - 1f), %rdx
    movq %rbx, %r10                     // the handles to send
    movl $1, %r8d
    syscall
    testq %rax, %rax
    js 8f

    // And now talk on the new channel, which the kernel never introduced.
    movl $6, %eax
    movq %r14, %rdi
    leaq 2f(%rip), %rsi
    movq $(3f - 2f), %rdx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    testq %rax, %rax
    js 8f

    // Blocks until the server answers. The thread is off every run queue while
    // it waits, which is the point of the whole arrangement.
    movl $7, %eax                       // Call::ChannelRead
    movq %r14, %rdi
    movabsq $0x600000, %rsi
    movl $256, %edx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    // Errors come back near the top of the range, so the sign bit separates
    // them from any length a channel will carry.
    testq %rax, %rax
    js 8f
    movq %rax, %r12

    movl $1, %eax                       // Call::Log
    movabsq $0x600000, %rdi
    movq %r12, %rsi
    syscall

    // Closing now, rather than letting the process teardown do it, is what
    // lets the server find out promptly instead of at the next reaping.
    movl $8, %eax                       // Call::HandleClose
    movq %r14, %rdi
    syscall
    movl $8, %eax
    movl $1, %edi
    syscall
    jmp 9f
8:
    movl $1, %eax
    leaq 3f(%rip), %rdi
    movl $(4f - 3f), %esi
    syscall
9:
    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    ud2

1:  .ascii "here is a channel of my own"
2:  .ascii "a request from the client, on the channel it passed over"
3:  .ascii "FAILED: the client could not talk to the server"
4:
nexus_user_client_end:

    .global nexus_user_server_start
    .global nexus_user_server_end
    .p2align 4
nexus_user_server_start:
    movabsq $0x600100, %rbx

    // Wait for an introduction: a message carrying a handle to talk on.
    movl $7, %eax                       // Call::ChannelRead
    movl $1, %edi
    movabsq $0x600000, %rsi
    movl $256, %edx
    movq %rbx, %r10                     // where the handles should land
    movl $4, %r8d
    syscall
    testq %rax, %rax
    js 8f
    movq %rax, %r12
    movl %eax, %r15d                    // the length, in the low half
    shrq $32, %r12                      // the handle count, in the high half
    cmpq $1, %r12
    jne 8f

    movl $1, %eax                       // Call::Log
    movabsq $0x600000, %rdi
    movq %r15, %rsi
    syscall

    movl (%rbx), %r13d                  // the handle that arrived

    // Everything after this happens on a channel the kernel never gave either
    // of them.
2:
    movl $7, %eax
    movq %r13, %rdi
    movabsq $0x600000, %rsi
    movl $256, %edx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    cmpq $-5, %rax                      // EPIPE: the client has gone
    je 7f
    testq %rax, %rax
    js 8f
    movq %rax, %r14

    movl $1, %eax
    movabsq $0x600000, %rdi
    movq %r14, %rsi
    syscall

    movl $6, %eax                       // Call::ChannelWrite
    movq %r13, %rdi
    leaq 1f(%rip), %rsi
    movq $(3f - 1f), %rdx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    testq %rax, %rax
    js 8f
    jmp 2b
7:
    movl $1, %eax
    leaq 3f(%rip), %rdi
    movl $(4f - 3f), %esi
    syscall
    jmp 9f
8:
    movl $1, %eax
    leaq 4f(%rip), %rdi
    movl $(5f - 4f), %esi
    syscall
9:
    // A status of zero, and it has to be written: `Exit` reads `rdi`, and
    // whatever a program happened to leave there would otherwise become
    // what its waiter is told about how it went.
    xorl %edi, %edi
    movl $0, %eax                       // Call::Exit
    syscall
    ud2

1:  .ascii "an answer from the server, on the channel it was handed"
3:  .ascii "server: the passed channel closed, so there is nothing left to answer"
4:  .ascii "FAILED: the server got an error it did not expect"
5:
nexus_user_server_end:
"#,
    options(att_syntax)
);

extern "C" {
    /// First byte of the program that exercises the system-call ABI.
    static nexus_user_abi_start: u8;
    /// One past its last byte.
    static nexus_user_abi_end: u8;
    /// First byte of the program that guards its own memory.
    static nexus_user_isolation_start: u8;
    /// One past its last byte.
    static nexus_user_isolation_end: u8;
    /// First byte of the program that exercises channels and handles.
    static nexus_user_ipc_start: u8;
    /// One past its last byte.
    static nexus_user_ipc_end: u8;
    /// First byte of the program that asks a question of another process.
    static nexus_user_client_start: u8;
    /// One past its last byte.
    static nexus_user_client_end: u8;
    /// First byte of the program that answers one.
    static nexus_user_server_start: u8;
    /// One past its last byte.
    static nexus_user_server_end: u8;
}

/// Why user mode could not be brought up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserError {
    /// No frame for a program, its stack or its data.
    OutOfMemory,
    /// A program does not fit the single page it is given.
    TooLarge(usize),
    /// An address space could not be created.
    Space(address_space::SpaceError),
    /// A mapping could not be created.
    Map(paging::MapError),
    /// The thread could not be started.
    Spawn(sched::SpawnError),
    /// The partition table could not be read.
    PartitionTable(fs::gpt::GptError),
    /// The disk has no filesystem to read a program from.
    NoFilesystem,
    /// The filesystem could not be read.
    Filesystem(fs::fat32::FatError),
    /// The file is not a program this kernel can load.
    Image(nexus_abi::elf::ElfError),
}

impl core::fmt::Display for UserError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory for a user process"),
            Self::TooLarge(size) => write!(f, "the user program is {size} bytes, more than a page"),
            Self::Space(error) => write!(f, "could not create an address space: {error}"),
            Self::Map(error) => write!(f, "could not map user memory: {error:?}"),
            Self::Spawn(error) => write!(f, "could not start the user thread: {error}"),
            Self::PartitionTable(error) => write!(f, "could not read the partition table: {error}"),
            Self::NoFilesystem => f.write_str("the disk has no EFI system partition"),
            Self::Filesystem(error) => write!(f, "could not read the filesystem: {error}"),
            Self::Image(error) => write!(f, "not a program this kernel can load: {error}"),
        }
    }
}

/// One page of a program, copied out of the kernel image.
///
/// Copied rather than mapped where it lies: the blob is assembled into the
/// kernel image like everything else, and adding a user mapping over the frames
/// it already occupies would hand ring 3 whatever else shares those pages. A
/// frame allocated for it holds nothing else by construction, which is a fact
/// rather than an argument about the linker.
///
/// # Safety
///
/// `start` and `end` must bound a blob inside this image.
unsafe fn copy_program(start: usize, end: usize) -> Result<u64, UserError> {
    let size = end - start;
    if size > layout::PAGE_SIZE as usize {
        return Err(UserError::TooLarge(size));
    }

    let frame = memory::allocate_frame().ok_or(UserError::OutOfMemory)?;
    // SAFETY: the frame was just allocated, so nothing else refers to it, and
    // the source is this image's own read-only data. The rest of the page is
    // zeroed so a program that runs off its own end meets `add [rax], al` and
    // faults, rather than executing whatever the frame held last.
    unsafe {
        let destination = layout::phys_to_virt(frame) as *mut u8;
        core::ptr::write_bytes(destination, 0, layout::PAGE_SIZE as usize);
        core::ptr::copy_nonoverlapping(start as *const u8, destination, size);
    }
    Ok(frame)
}

/// Allocate a zeroed frame for user data.
fn zeroed_frame() -> Result<u64, UserError> {
    let frame = memory::allocate_frame().ok_or(UserError::OutOfMemory)?;
    // SAFETY: just allocated, so nothing else refers to it.
    unsafe {
        core::ptr::write_bytes(
            layout::phys_to_virt(frame) as *mut u8,
            0,
            layout::PAGE_SIZE as usize,
        );
    }
    Ok(frame)
}

/// Build a process around `program` and start its thread.
///
/// `data` is the contents of the page at [`DATA_BASE`], or nothing if the
/// program does not use one.
///
/// # Safety
///
/// Call with the heap, the frame allocator and the scheduler running.
unsafe fn spawn_process(
    name: &str,
    program: (usize, usize),
    identifier: u64,
    data: Option<&[u8]>,
    endowment: Option<(ipc::Object, ipc::Rights)>,
) -> Result<Arc<address_space::AddressSpace>, UserError> {
    let space = address_space::AddressSpace::new().map_err(UserError::Space)?;

    // SAFETY: the bounds come from this image's own symbols.
    let code = unsafe { copy_program(program.0, program.1) }?;
    let stack = zeroed_frame()?;

    // The identifier goes at the very top of the stack, where the program finds
    // it under its initial `rsp`. A stack is the one page a process cannot be
    // sharing with another and still be running at all, which is what makes it
    // the right place for the one value the isolation test must not have in
    // common.
    // SAFETY: the frame was just allocated and is reachable through the direct
    // map; the offset is inside it.
    unsafe {
        let top = layout::phys_to_virt(stack) + layout::PAGE_SIZE;
        core::ptr::write_volatile((top - INITIAL_STACK_OFFSET) as *mut u64, identifier);
    }

    // Read-only and executable. Nothing in ring 3 may write its own code, which
    // is the one protection that costs nothing here and would take work to
    // give up.
    //
    // SAFETY: the frames are owned by this process and the addresses are in the
    // user half, which nothing else maps in this space.
    unsafe {
        space
            .map(CODE_BASE, code, paging::USER)
            .map_err(UserError::Map)?;
        space
            .map(
                STACK_TOP - layout::PAGE_SIZE,
                stack,
                paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
            )
            .map_err(UserError::Map)?;
    }

    if let Some(contents) = data {
        let frame = data_frame(contents)?;
        // SAFETY: as above.
        unsafe {
            space
                .map(
                    DATA_BASE,
                    frame,
                    paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
                )
                .map_err(UserError::Map)?;
        }
    }

    let root = space.root();
    let space = Arc::new(space);
    let process = crate::process::Process::new(name, Arc::clone(&space));
    let id = process.id;

    // Whatever the kernel decided this process may reach, handed over before it
    // runs. A process starts with exactly the authority it was given and no way
    // to ask for more, which is the whole of what a capability system means at
    // the moment a process begins.
    if let Some((object, rights)) = endowment {
        let handle = process.handles.insert(object, rights);
        kprintln!("[user] process {id} \"{name}\" starts holding handle {handle}");
    }

    sched::spawn_user(name, CODE_BASE, STACK_TOP - INITIAL_STACK_OFFSET, process)
        .map_err(UserError::Spawn)?;

    kprintln!("[user] process {id} \"{name}\" in address space {root:#x}");
    Ok(space)
}

/// The one data frame every process gets in the shared-page injection build.
#[cfg(feature = "inject-shared-user-page")]
static SHARED_DATA_FRAME: crate::sync::IrqSpinLock<Option<u64>> =
    crate::sync::IrqSpinLock::new(None);

/// Fill a frame with a program's data page.
///
/// In the shared-page injection build every process is handed the *same* frame,
/// which is what having no address-space separation would look like from inside
/// a program. See the feature's note in Cargo.toml.
#[cfg(feature = "inject-shared-user-page")]
fn data_frame(contents: &[u8]) -> Result<u64, UserError> {
    let mut slot = SHARED_DATA_FRAME.lock();
    let frame = match *slot {
        Some(frame) => frame,
        None => {
            let frame = zeroed_frame()?;
            *slot = Some(frame);
            frame
        }
    };
    // SAFETY: the frame is reachable through the direct map and the length is
    // bounded by the callers that build these.
    unsafe {
        core::ptr::copy_nonoverlapping(
            contents.as_ptr(),
            layout::phys_to_virt(frame) as *mut u8,
            contents.len().min(layout::PAGE_SIZE as usize),
        );
    }
    Ok(frame)
}

/// Fill a frame with a program's data page.
#[cfg(not(feature = "inject-shared-user-page"))]
fn data_frame(contents: &[u8]) -> Result<u64, UserError> {
    let frame = zeroed_frame()?;
    // SAFETY: just allocated, and the length is checked against the page size
    // by the callers that build these.
    unsafe {
        core::ptr::copy_nonoverlapping(
            contents.as_ptr(),
            layout::phys_to_virt(frame) as *mut u8,
            contents.len().min(layout::PAGE_SIZE as usize),
        );
    }
    Ok(frame)
}

/// Lay out the data page the isolation program expects.
///
/// Eight unused bytes, eight of scratch for the program to write into and read
/// back, eight of message length, and then the message. The program never
/// learns any of this from the kernel at run time; the layout *is* the
/// interface. The identifier is deliberately not here — it goes on the stack,
/// so that it stays private even when this page does not.
fn isolation_data(message: &str) -> [u8; 128] {
    let mut page = [0u8; 128];
    page[16..24].copy_from_slice(&(message.len() as u64).to_le_bytes());
    page[24..24 + message.len()].copy_from_slice(message.as_bytes());
    page
}

/// Bring up user mode: one process per program, each in its own address space.
///
/// # Safety
///
/// Call once, with the heap and the scheduler running.
pub unsafe fn start() -> Result<(), UserError> {
    let abi = (
        core::ptr::addr_of!(nexus_user_abi_start) as usize,
        core::ptr::addr_of!(nexus_user_abi_end) as usize,
    );
    let isolation = (
        core::ptr::addr_of!(nexus_user_isolation_start) as usize,
        core::ptr::addr_of!(nexus_user_isolation_end) as usize,
    );
    let channels = (
        core::ptr::addr_of!(nexus_user_ipc_start) as usize,
        core::ptr::addr_of!(nexus_user_ipc_end) as usize,
    );
    let client = (
        core::ptr::addr_of!(nexus_user_client_start) as usize,
        core::ptr::addr_of!(nexus_user_client_end) as usize,
    );
    let server = (
        core::ptr::addr_of!(nexus_user_server_start) as usize,
        core::ptr::addr_of!(nexus_user_server_end) as usize,
    );

    // The first program that is a *file*. Everything else here is assembled
    // into the kernel and copied into a page; this one is read off a
    // filesystem, which is the difference between running user code and running
    // programs. Reported rather than fatal: a machine with no disk still boots.
    //
    // SAFETY: the heap, the scheduler and the block device are all running.
    if drivers::virtio_blk::is_present() {
        // SAFETY: the scheduler is running.
        let spawner = unsafe { start_spawn_service() }?;
        // SAFETY: as above, and the block device is up.
        if let Err(error) =
            unsafe { start_from_disk("BIN/INIT.ELF", "init", &init_endowments(spawner)) }
        {
            kprintln!("[user] could not start init from disk: {error}");
        }

        // And the compositor, which is where the display goes. Everything on
        // screen so far was drawn by whoever could reach the framebuffer -- the
        // kernel, because it has it. From here a process has it, and everything
        // else that draws goes through that process.
        //
        // SAFETY: as above, with the display up.
        let compositor_spawner = unsafe { start_spawn_service() }?;
        if let Err(error) = unsafe { start_compositor(compositor_spawner) } {
            kprintln!("[user] could not start the compositor: {error}");
        }
    }

    // SAFETY: the heap, the frame allocator and the scheduler are all running.
    let (alpha, beta) = unsafe {
        spawn_process("abi", abi, 0, None, None)?;

        // A data page it uses as a receive buffer rather than as a message.
        spawn_process("ipc", channels, 0, Some(&[0u8; 128]), None)?;

        // Two processes that can only reach each other, and only through the
        // one thing the kernel handed each of them. Neither can name the
        // other; the handle is the introduction and the authority at once.
        let (to_server, to_client) = ipc::Endpoint::pair();
        spawn_process(
            "server",
            server,
            0,
            Some(&[0u8; 128]),
            Some((ipc::Object::Channel(to_client), ipc::Rights::ALL)),
        )?;
        spawn_process(
            "client",
            client,
            0,
            Some(&[0u8; 128]),
            Some((ipc::Object::Channel(to_server), ipc::Rights::ALL)),
        )?;

        // Two processes with identical virtual layouts and different contents.
        // Both write their own identifier to the same address, over and over,
        // and check it back. If they shared a page the check would fail almost
        // at once, which is what makes this a test of isolation and not of
        // whether two programs can run.
        (
            spawn_process(
                "alpha",
                isolation,
                1,
                Some(&isolation_data("process alpha kept its own memory")),
                None,
            )?,
            spawn_process(
                "beta",
                isolation,
                2,
                Some(&isolation_data("process beta kept its own memory")),
                None,
            )?,
        )
    };

    // The same claim the two programs will make about themselves, checked from
    // the other side of the boundary before either of them has run. The
    // programs can only report what they observe; the page tables can be asked
    // directly, and if these ever agreed the user-side test would be watching
    // for something that had already happened.
    match (alpha.translate(DATA_BASE), beta.translate(DATA_BASE)) {
        (Some(first), Some(second)) if first != second => kprintln!(
            "[user] alpha and beta both map {DATA_BASE:#x}, to {first:#x} and {second:#x}"
        ),
        (Some(first), Some(_)) => kprintln!(
            "[user] FAILED: alpha and beta both map {DATA_BASE:#x} to {first:#x}, the same frame"
        ),
        _ => kprintln!("[user] FAILED: a process has no data page at {DATA_BASE:#x}"),
    }

    let (created, destroyed) = address_space::statistics();
    kprintln!("[user] {created} address spaces created, {destroyed} destroyed");
    Ok(())
}

/// The service end of each spawn channel, held by the thread that answers on it.
///
/// A slot per service rather than one place, because there is more than one
/// now: `init` has a spawner and so does the compositor, and they must not be
/// the same channel -- a compositor that could be asked for programs on
/// `init`'s behalf is a compositor answering for somebody else.
///
/// It was one place, and that was a race with a very confusing shape. Starting
/// the second service overwrote the first before the first thread had read it,
/// so both threads served the *same* endpoint and the other channel had nobody
/// answering on it. Whoever was waiting for a reply waited forever, and which
/// of the two it was depended on how the two threads were scheduled.
static SPAWN_SERVICES: crate::sync::IrqSpinLock<[Option<Arc<ipc::Endpoint>>; MAX_SERVICES]> =
    crate::sync::IrqSpinLock::new([None, None, None, None]);

/// How many spawn services there may be.
const MAX_SERVICES: usize = 4;

/// Which slot the next service takes.
static NEXT_SERVICE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Longest program path the spawn service will accept.
///
/// A bound rather than a trust: the path arrives from a process that need not
/// be cooperating, and every byte of it is used to walk a filesystem.
const MAX_PATH: usize = 64;

/// Answer requests to start a program.
///
/// This is what makes process creation something a *process* can ask for
/// without the kernel trusting it to be allowed. There is no system call to
/// create a process; there is a channel, and holding one end of it is the
/// authority. A process that was never given that handle cannot ask, and there
/// is no name it could use instead — which is the whole argument for handles
/// over a global namespace, made concrete.
///
/// The reply carries a handle to a channel connected to whatever was started,
/// so the asker gets a way to talk to it and not merely a yes.
fn spawn_service(slot: usize) {
    // The slot is this thread's own, given to it when it was started, so no
    // second service can take its endpoint away between the two.
    let endpoint = SPAWN_SERVICES.lock().get(slot).and_then(Clone::clone);
    let Some(endpoint) = endpoint else {
        return;
    };

    loop {
        // Blocks. When this returns nothing, every process that could ask has
        // gone, and so has the reason for this thread to exist.
        let Some(request) = endpoint.receive() else {
            kprintln!("[spawn] no one left to ask; the spawn service is stopping");
            return;
        };

        let (text, handles) = handle_spawn_request(&request.bytes);
        if let Err(error) = endpoint.send(text.as_bytes(), handles) {
            kprintln!("[spawn] could not reply: {error}");
        }
    }
}

/// Do one request, and say what happened.
///
/// One reply, carrying both the text and the channel, because they are one
/// answer: a caller that had to read two messages to learn one thing would have
/// to know how many to expect, and a refusal sends fewer than a success.
///
/// The text is what the asker logs, so it is written for someone reading a boot
/// log rather than for a program to parse. A program that needs to know whether
/// it worked has the handle or does not.
fn handle_spawn_request(request: &[u8]) -> (alloc::string::String, alloc::vec::Vec<ipc::Handle>) {
    use alloc::format;
    use alloc::vec::Vec;

    // A path, and after a zero byte whatever the asker wants the new program to
    // be told. Arguments are not a separate mechanism here: they are the first
    // message on the channel the program is given, sent by the kernel on the
    // asker's behalf because the asker does not have that channel until the
    // reply comes back -- and a program that had to wait for its arguments
    // until after it had started would have started without them.
    let split = request.iter().position(|byte| *byte == 0);
    let (path, arguments) = match split {
        Some(at) => (&request[..at], &request[at + 1..]),
        None => (request, &request[request.len()..]),
    };

    if path.len() > MAX_PATH {
        return (
            format!("refused: a path of {} bytes is too long", path.len()),
            Vec::new(),
        );
    }
    if arguments.len() > ipc::MAX_MESSAGE {
        return (
            format!("refused: {} bytes of argument is too much", arguments.len()),
            Vec::new(),
        );
    }
    let Ok(path) = core::str::from_utf8(path) else {
        return (
            alloc::string::String::from("refused: the path is not text"),
            Vec::new(),
        );
    };

    // A program built for something else is asked for by saying so. Nothing in
    // an executable distinguishes a static Linux binary from a NexusOS one --
    // both are ET_EXEC, EM_X86_64, ELFOSABI_SYSV with no interpreter -- so the
    // asker says which, and a caller that says nothing gets this system's own
    // interface. Guessing would mean occasionally reading a program's first
    // system call as a completely different request.
    let (personality, path) = match path.strip_prefix("linux:") {
        Some(rest) => (crate::process::Personality::Linux, rest),
        None => (crate::process::Personality::Nexus, path),
    };

    // A channel between the asker and whatever is about to run. Both ends are
    // made here because the kernel is the only thing that can hand one to a
    // process that does not exist yet.
    let (to_child, to_parent) = ipc::Endpoint::pair();

    // SAFETY: the heap, the scheduler and the block device are all running;
    // this thread does nothing else while it loads.
    let result = unsafe {
        start_from_disk_as(
            path,
            "spawned",
            &[(ipc::Object::Channel(to_parent), ipc::Rights::ALL)],
            personality,
        )
    };

    match result {
        Ok(completion) => {
            let id = completion.id;

            // The arguments go down the channel before its other end is handed
            // over, so they are already waiting when the program makes its
            // first read -- and before the asker can send anything of its own,
            // so a program can rely on its arguments being the first thing it
            // hears and not merely an early one.
            if !arguments.is_empty() {
                if let Err(error) = to_child.send(arguments, Vec::new()) {
                    kprintln!("[spawn] could not give {id} its arguments: {error}");
                }
            }

            kprintln!("[spawn] started {id} from {path} at a process's request");
            // Two handles, in a fixed order: the channel to talk to it, and the
            // process to wait for it. Both go back with the one reply, because
            // they are one answer -- an introduction and an undertaking to say
            // when it is over.
            (
                format!("started {id}"),
                alloc::vec![
                    ipc::Handle {
                        object: ipc::Object::Channel(to_child),
                        rights: ipc::Rights::ALL,
                    },
                    ipc::Handle {
                        object: ipc::Object::Process(completion),
                        rights: ipc::Rights::ALL,
                    },
                ],
            )
        }
        Err(error) => (format!("refused: {error}"), Vec::new()),
    }
}

/// Start the spawn service, and return the end a process should be given.
///
/// # Safety
///
/// Call once, with the scheduler running.
unsafe fn start_spawn_service() -> Result<Arc<ipc::Endpoint>, UserError> {
    let slot = NEXT_SERVICE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // More services than slots is a mistake in this file rather than a
    // condition to handle, so it is reported and refused rather than made to
    // work by growing something.
    if slot >= MAX_SERVICES {
        kprintln!("[spawn] no slot left for another spawn service");
        return Err(UserError::NoFilesystem);
    }

    let (service, client) = ipc::Endpoint::pair();
    SPAWN_SERVICES.lock()[slot] = Some(service);

    sched::spawn(
        "spawn",
        sched::thread::Priority::Normal,
        spawn_service,
        slot,
    )
    .map_err(UserError::Spawn)?;

    Ok(client)
}

/// Hand the display to a process, and let it hand out surfaces.
///
/// The framebuffer is memory the firmware chose, so it is *described* to a
/// memory object rather than allocated for one, and the object does not free it
/// when the last handle goes: those frames are the display, and handing them
/// back to the page allocator would hand out the display with them.
///
/// The rectangle it may use, and the shape of the framebuffer, go with the
/// handle as eight little-endian numbers. Nothing is left to be discovered: a
/// program that guessed the stride would draw a diagonal smear on the first
/// machine whose scanlines are padded.
///
/// It is also given an end of the spawn service, because a compositor with no
/// way to start a client is a compositor with nothing to composite. That is two
/// pieces of authority and they are separable on purpose -- the display and the
/// right to ask for programs are different things, and a program that needed
/// only one would be given only one.
///
/// # Safety
///
/// Call once, with the display, the scheduler and the block device running.
unsafe fn start_compositor(spawner: Arc<ipc::Endpoint>) -> Result<(), UserError> {
    let Some(info) = crate::display::geometry() else {
        return Ok(());
    };
    let Some((x, y, width, height)) = crate::display::unclaimed_region() else {
        return Ok(());
    };

    // SAFETY: the firmware reported this region and the bootloader mapped it;
    // it stays valid for the life of the system, and handing it to a process is
    // the point.
    let Some(memory) = (unsafe { ipc::MemoryObject::borrowed(info.phys_addr, info.size as usize) })
    else {
        kprintln!("[user] the framebuffer is not shaped like something a process can map");
        return Ok(());
    };

    let (service, client) = ipc::Endpoint::pair();

    // Keys go to it too, so that deciding which program a keystroke is for is
    // its decision and not the kernel's. The kernel goes on showing what was
    // typed on its own panel; what crosses here is a copy.
    let (keys_here, keys_there) = ipc::Endpoint::pair();

    // And the pointer, on a channel of its own rather than mixed in with the
    // keys. Two devices, two streams: a compositor that had to tell them apart
    // by the length of a message would be one format change away from routing a
    // keystroke as a movement.
    let (pointer_here, pointer_there) = ipc::Endpoint::pair();

    // In this order, because the program names them by the numbers they get:
    // the channel the display arrives on, the one it asks for clients on, and
    // the two that carry keys and pointer movements.
    // SAFETY: as above.
    unsafe {
        start_from_disk(
            "BIN/COMP.ELF",
            "compositor",
            &[
                (ipc::Object::Channel(client), ipc::Rights::ALL),
                (ipc::Object::Channel(spawner), ipc::Rights::ALL),
                (ipc::Object::Channel(keys_there), ipc::Rights::ALL),
                (ipc::Object::Channel(pointer_there), ipc::Rights::ALL),
            ],
        )?;
    }
    crate::input::route_to(keys_here);
    crate::drivers::mouse::route_to(pointer_here);

    let mut message = [0u8; 32];
    for (index, value) in [
        x,
        y,
        width,
        height,
        info.stride,
        info.bytes_per_pixel,
        info.width,
        info.height,
    ]
    .iter()
    .enumerate()
    {
        message[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }

    let handle = ipc::Handle {
        object: ipc::Object::Memory(memory),
        rights: ipc::Rights::ALL,
    };
    if let Err(error) = service.send(&message, alloc::vec![handle]) {
        kprintln!("[user] could not hand the screen to the compositor: {error}");
        return Ok(());
    }

    kprintln!("[user] handed a {width}x{height} rectangle at ({x}, {y}) to a process");

    // Kept so the channel does not close the moment this returns, which the
    // compositor would see as its parent going away before it had drawn.
    *COMPOSITOR.lock() = Some(service);
    Ok(())
}

/// The kernel's end of the compositor's channel, held open for its lifetime.
///
/// Nothing reads from it. It exists so the compositor's end stays open: an
/// endpoint whose peer has gone reports as closed, and a compositor that saw
/// that would conclude the display had been taken back.
static COMPOSITOR: crate::sync::IrqSpinLock<Option<Arc<ipc::Endpoint>>> =
    crate::sync::IrqSpinLock::new(None);

/// Where a program loaded from disk gets its stack.
///
/// Above anything a link script puts an image at, and below the halfway line by
/// a long way. One page, which is enough for a program that does not recurse
/// and is the right amount to notice when one does.
const DISK_STACK_TOP: u64 = 0x0000_0000_0100_0000;

/// Read a program from the filesystem and start it in a process of its own.
///
/// This is the difference between a system that can run user code and one that
/// can run *programs*. Everything in ring 3 before this was assembled into the
/// kernel and copied into a page; this is an ELF file on a disk, built as its
/// own binary, loaded into an address space that did not exist a moment ago.
///
/// # Safety
///
/// Call with the heap, the scheduler and the block device running.
pub unsafe fn start_from_disk(
    path: &str,
    name: &str,
    endowments: &[(ipc::Object, ipc::Rights)],
) -> Result<alloc::sync::Arc<crate::process::Completion>, UserError> {
    // SAFETY: forwarded to the caller's promise.
    unsafe { start_from_disk_as(path, name, endowments, crate::process::Personality::Nexus) }
}

/// The same, for a program built for something other than this system.
///
/// The personality is *given*, never guessed. A static Linux executable and a
/// NexusOS one are both `ET_EXEC`, `EM_X86_64`, `ELFOSABI_SYSV` images with no
/// interpreter: nothing in the file distinguishes them, and a loader that tried
/// would eventually read one as the other -- which is not a failure, it is a
/// program whose first system call means something else entirely.
///
/// # Safety
///
/// As [`start_from_disk`].
pub unsafe fn start_from_disk_as(
    path: &str,
    name: &str,
    endowments: &[(ipc::Object, ipc::Rights)],
    personality: crate::process::Personality,
) -> Result<alloc::sync::Arc<crate::process::Completion>, UserError> {
    let partitions = fs::gpt::read().map_err(UserError::PartitionTable)?;
    let esp = partitions
        .iter()
        .find(|partition| partition.is_esp())
        .ok_or(UserError::NoFilesystem)?;
    let volume = fs::fat32::Volume::mount(esp.first_lba).map_err(UserError::Filesystem)?;
    let image = volume.read_file(path).map_err(UserError::Filesystem)?;

    let space = address_space::AddressSpace::new().map_err(UserError::Space)?;

    // One contiguous span for the whole image, which is what the loader wants:
    // it computes every segment's place as an offset from the base. The pages
    // are handed back individually when the address space is dropped, and the
    // allocator merges them back into the block they came from -- which is why
    // every page of the span is mapped below, gaps included. A page that was
    // allocated and never mapped would never be freed, and the block it came
    // from would stay split for the life of the system.
    let mut span_base = 0u64;
    let mut span_pages = 0usize;

    // SAFETY: the closure returns a block reachable through the direct map,
    // which is what `to_virtual` says.
    let loaded = unsafe {
        nexus_abi::elf::load(
            &image,
            |pages| {
                let order = order_for_pages(pages);
                let base = memory::allocate_block(order)?;
                span_base = base;
                span_pages = 1usize << order;
                Some(base)
            },
            layout::phys_to_virt,
        )
    }
    .map_err(UserError::Image)?;

    // Every page of the image span, with the permissions of whichever segment
    // covers it. A page in a gap between segments belongs to the image and has
    // to be mapped so that it is freed with it, but nothing should be able to
    // read or run it, so it gets the least of everything.
    for page in 0..(loaded.image_size / layout::PAGE_SIZE) {
        let virt = loaded.virt_base + page * layout::PAGE_SIZE;
        let phys = loaded.phys_base + page * layout::PAGE_SIZE;

        let segment = loaded.segments[..loaded.segment_count]
            .iter()
            .find(|segment| virt >= segment.virt_start && virt < segment.virt_start + segment.size);

        let mut flags = paging::USER;
        match segment {
            Some(segment) => {
                if segment.flags.writable {
                    flags |= paging::WRITABLE;
                }
                if !segment.flags.executable {
                    flags |= paging::NO_EXECUTE;
                }
            }
            None => flags |= paging::NO_EXECUTE,
        }

        // SAFETY: the frame is part of the block just allocated for this image,
        // and the address is in the user half of a space nothing else has.
        unsafe { space.map(virt, phys, flags) }.map_err(UserError::Map)?;
    }

    let stack = zeroed_frame()?;
    // SAFETY: as above.
    unsafe {
        space
            .map(
                DISK_STACK_TOP - layout::PAGE_SIZE,
                stack,
                paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
            )
            .map_err(UserError::Map)?;
    }

    let process = crate::process::Process::with_personality(name, Arc::new(space), personality);
    let id = process.id;
    // Taken before the process is handed to the scheduler, because after that
    // it may have exited by the time this function returns and the `Arc` in
    // hand is the only thing that will still be able to say so.
    let completion = Arc::clone(&process.completion);

    // Whatever authority the caller decided this program should have, handed
    // over before it runs. A program starts with exactly what it was given, in
    // the order it was given it, so a program can name its endowments by the
    // handle numbers they will have rather than by discovering them.
    for (object, rights) in endowments {
        let kind = object.kind();
        let handle = process.handles.insert(object.clone(), *rights);
        kprintln!("[user] process {id} \"{name}\" starts holding {kind} handle {handle}");
    }

    // Where the program finds its stack pointer, which is not the same question
    // in the two worlds. A Nexus program is handed one number below the top; a
    // program built for Linux expects a whole structure there, and expects it
    // before its first instruction runs.
    let stack_pointer = match personality {
        crate::process::Personality::Nexus => DISK_STACK_TOP - INITIAL_STACK_OFFSET,
        // SAFETY: `stack` is the frame just mapped at the top of this address
        // space, it is this kernel's to write through the direct map until the
        // process runs, and the layout is written entirely inside it.
        crate::process::Personality::Linux => unsafe { system_v_stack(stack, name) },
    };

    sched::spawn_user(name, loaded.entry_point, stack_pointer, process)
        .map_err(UserError::Spawn)?;

    kprintln!(
        "[user] process {id} \"{name}\" loaded from {path}: {} bytes, entry {:#x}, \
         {} segments in {} KiB",
        image.len(),
        loaded.entry_point,
        loaded.segment_count,
        span_pages * layout::PAGE_SIZE as usize / 1024
    );
    Ok(completion)
}

/// Lay out the stack a System V program expects, and say where its `rsp` goes.
///
/// At the entry point of a program built for Linux, `rsp` points at `argc`,
/// followed by the argument pointers, a null, the environment pointers, another
/// null, and then the auxiliary vector terminated by `AT_NULL`. That is not a
/// convenience the C library sets up -- it is what the kernel is required to
/// have put there, and a program that reads it finds whatever was in the page
/// if nobody did.
///
/// This is the smallest honest version of it: one argument, which is the
/// program's name, no environment, and an empty auxiliary vector. A program
/// that needs `AT_PHDR` or `AT_RANDOM` will find them absent, which is what
/// `AT_NULL` immediately means and what such a program is required to cope with.
///
/// `rsp` is sixteen-byte aligned, because the ABI says so and because the first
/// `movaps` in any compiled function faults if it is not.
///
/// # Safety
///
/// `stack` must be the physical frame mapped at the top of the target address
/// space, and nothing else may be writing it.
unsafe fn system_v_stack(stack: u64, name: &str) -> u64 {
    /// Bytes set aside at the very top for the program's name.
    const NAME_ROOM: u64 = 32;

    let page = layout::phys_to_virt(stack);
    let bottom = DISK_STACK_TOP - layout::PAGE_SIZE;

    // The name string, as high in the page as it will go.
    let name_at = DISK_STACK_TOP - NAME_ROOM;
    let bytes = name.as_bytes();
    let taken = bytes.len().min(NAME_ROOM as usize - 1);
    // SAFETY: the destination is inside the page, and the length is bounded by
    // the room set aside for it.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (page + (name_at - bottom)) as *mut u8,
            taken,
        );
        // The terminator, which is what makes it a C string -- and every
        // program that reads `argv[0]` reads it as one.
        core::ptr::write_volatile((page + (name_at - bottom) + taken as u64) as *mut u8, 0);
    }

    // Six words: argc, one argument, the null that ends the arguments, the null
    // that ends the environment, and the two that are `AT_NULL`.
    let words: u64 = 6;
    let vector_at = (name_at - words * 8) & !15;

    // SAFETY: as above; the vector lies below the name and inside the page.
    unsafe {
        let slot = (page + (vector_at - bottom)) as *mut u64;
        slot.write_volatile(1); // argc
        slot.add(1).write_volatile(name_at); // argv[0]
        slot.add(2).write_volatile(0); // argv is null-terminated
        slot.add(3).write_volatile(0); // and so is the environment
        slot.add(4).write_volatile(0); // AT_NULL
        slot.add(5).write_volatile(0); // with a value of nothing
    }

    vector_at
}

/// What `init` starts holding.
///
/// Handle 1 is the channel to the spawn service, and handle 2 is the root of
/// the system's filesystem. That is the whole of what `init` can reach: a
/// program has no ambient authority here, so a filesystem it was not handed is
/// a filesystem it cannot name, and the numbers are fixed only because they are
/// issued in this order and `init` has to call them something.
///
/// The directory comes second and may not come at all -- a machine whose disk
/// has no NexusFS on it still starts `init`, which then finds handle 2 is not a
/// handle. That is the honest shape: the program asks and is told no, rather
/// than the kernel inventing an empty filesystem so that the call succeeds.
fn init_endowments(
    spawner: alloc::sync::Arc<ipc::Endpoint>,
) -> alloc::vec::Vec<(ipc::Object, ipc::Rights)> {
    let mut endowments = alloc::vec![(ipc::Object::Channel(spawner), ipc::Rights::ALL)];
    match fs::store::root() {
        Ok(root) => endowments.push((ipc::Object::Node(root), ipc::Rights::ALL)),
        Err(error) => kprintln!("[user] init gets no filesystem: {error}"),
    }
    endowments
}

/// Smallest buddy order whose block holds `pages` pages.
fn order_for_pages(pages: usize) -> usize {
    let mut order = 0;
    while (1usize << order) < pages {
        order += 1;
    }
    order
}

/// Leave the kernel for ring 3. Never returns.
///
/// `iretq` rather than `sysretq` for the first entry: `sysretq` insists on
/// taking the target address from `rcx` and the flags from `r11`, which is the
/// right trade for a system-call return and a needless constraint here, where
/// there is no system call to return from. `iretq` takes all five values from
/// the stack, so the transition is a stack layout rather than a register dance.
///
/// # Safety
///
/// `entry` and `stack_top` must be mapped `USER`, executable and writable
/// respectively, in the address space that is active. The caller must have set
/// this processor's `rsp0` and syscall stack, or the first interrupt from ring
/// 3 will have nowhere to land.
pub unsafe fn enter(entry: u64, stack_top: u64) -> ! {
    // Interrupts on in ring 3: a user thread has to be preemptible, and a
    // thread that could not be preempted would be a thread that could hang the
    // system with a loop.
    const USER_FLAGS: u64 = 1 << 9;

    // SAFETY: upheld by the caller. `swapgs` puts the user's `GS` base in place
    // and leaves this processor's own in `IA32_KERNEL_GS_BASE`, which is where
    // every entry back into the kernel expects to find it.
    unsafe {
        core::arch::asm!(
            "swapgs",
            "push {ss}",
            "push {rsp}",
            "push {flags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) u64::from(gdt::USER_DATA_SELECTOR),
            rsp = in(reg) stack_top,
            flags = in(reg) USER_FLAGS,
            cs = in(reg) u64::from(gdt::USER_CODE64_SELECTOR),
            rip = in(reg) entry,
            options(noreturn),
        )
    }
}
