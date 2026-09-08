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

    // Write on one end.
    movl $6, %eax                       // Call::ChannelWrite
    movq %r12, %rdi
    leaq 1f(%rip), %rsi
    movq $(2f - 1f), %rdx
    syscall
    cmpq $(2f - 1f), %rax
    jne 8f

    // And read it out of the other, into this process's data page.
    movl $7, %eax                       // Call::ChannelRead
    movq %r13, %rdi
    movabsq $0x600000, %rsi
    movl $256, %edx
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
}

impl core::fmt::Display for UserError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory for a user process"),
            Self::TooLarge(size) => write!(f, "the user program is {size} bytes, more than a page"),
            Self::Space(error) => write!(f, "could not create an address space: {error}"),
            Self::Map(error) => write!(f, "could not map user memory: {error:?}"),
            Self::Spawn(error) => write!(f, "could not start the user thread: {error}"),
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

    // SAFETY: the heap, the frame allocator and the scheduler are all running.
    let (alpha, beta) = unsafe {
        spawn_process("abi", abi, 0, None)?;

        // A data page it uses as a receive buffer rather than as a message.
        spawn_process("ipc", channels, 0, Some(&[0u8; 128]))?;

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
            )?,
            spawn_process(
                "beta",
                isolation,
                2,
                Some(&isolation_data("process beta kept its own memory")),
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
