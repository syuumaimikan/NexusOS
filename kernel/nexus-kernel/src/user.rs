//! Ring 3.
//!
//! The first code NexusOS runs that the kernel does not trust. It is small on
//! purpose: the point of this module is not what the program does but that the
//! boundary around it is real — its pages are the only ones it can reach, its
//! stack cannot be executed, its code cannot be written, and the only way back
//! into the kernel is [`syscall`](crate::arch::syscall).
//!
//! # Why the program is copied rather than mapped where it lies
//!
//! It is assembled into the kernel image like everything else, and the obvious
//! thing would be to add a user mapping over the frames it already occupies.
//! That would hand ring 3 whatever else shares those pages — the kernel is not
//! laid out so that this blob has any to itself — so instead it is copied into
//! a frame allocated for it. The frame holds nothing else by construction, and
//! the check is a comparison rather than an argument about the linker.
//!
//! # What is not here yet
//!
//! One address space, shared with the kernel: the user mappings are added to
//! the same page tables, protected by the `USER` bit rather than by being
//! absent. That is enough for the boundary to be enforced and not enough to be
//! called isolation — a separate `cr3` per process comes with processes, and
//! with it the page-table lifetime and TLB work that make address spaces cost
//! something. Nothing above this line depends on which of the two it is.

use nexus_abi::layout;

use crate::arch::gdt;
use crate::memory::paging;
use crate::{kprintln, memory, sched};

/// Where the user program is mapped.
///
/// Four megabytes in: low enough to be obviously user space, high enough that a
/// null dereference in ring 3 is nowhere near it.
const CODE_BASE: u64 = 0x0000_0000_0040_0000;

/// One page below the top of the user stack region.
const STACK_TOP: u64 = 0x0000_0000_0080_0000;

const _: () = {
    assert!(CODE_BASE < layout::USER_SPACE_END);
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
    .global nexus_user_program_start
    .global nexus_user_program_end
nexus_user_program_start:
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
nexus_user_program_end:
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
    .global nexus_user_program_start
    .global nexus_user_program_end
nexus_user_program_start:
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
nexus_user_program_end:
"#,
    options(att_syntax)
);

extern "C" {
    /// First byte of the user program.
    static nexus_user_program_start: u8;
    /// One past its last byte.
    static nexus_user_program_end: u8;
}

/// Why user mode could not be brought up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserError {
    /// No frame for the program or its stack.
    OutOfMemory,
    /// The program does not fit the single page it is given.
    TooLarge(usize),
    /// A mapping could not be created.
    Map(paging::MapError),
    /// The thread could not be started.
    Spawn(sched::SpawnError),
}

impl core::fmt::Display for UserError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory for the user program"),
            Self::TooLarge(size) => write!(f, "the user program is {size} bytes, more than a page"),
            Self::Map(error) => write!(f, "could not map user memory: {error:?}"),
            Self::Spawn(error) => write!(f, "could not start the user thread: {error}"),
        }
    }
}

/// Map the user program and its stack, then start a thread that runs it.
///
/// # Safety
///
/// Call once, with the heap and the scheduler running.
pub unsafe fn start() -> Result<(), UserError> {
    let start = core::ptr::addr_of!(nexus_user_program_start) as usize;
    let end = core::ptr::addr_of!(nexus_user_program_end) as usize;
    let size = end - start;
    if size > layout::PAGE_SIZE as usize {
        return Err(UserError::TooLarge(size));
    }

    let code_frame = memory::allocate_frame().ok_or(UserError::OutOfMemory)?;
    let stack_frame = memory::allocate_frame().ok_or(UserError::OutOfMemory)?;

    // Copied through the direct map, which is where a frame is reachable before
    // it has a mapping of its own. The rest of the page is zeroed so that a
    // program that runs off its own end meets `add [rax], al` and faults,
    // rather than executing whatever the frame held last.
    // SAFETY: the frame was just allocated, so nothing else refers to it, and
    // the source is this image's own read-only data.
    unsafe {
        let destination = layout::phys_to_virt(code_frame) as *mut u8;
        core::ptr::write_bytes(destination, 0, layout::PAGE_SIZE as usize);
        core::ptr::copy_nonoverlapping(start as *const u8, destination, size);
        core::ptr::write_bytes(
            layout::phys_to_virt(stack_frame) as *mut u8,
            0,
            layout::PAGE_SIZE as usize,
        );
    }

    // Read-only and executable. Nothing in ring 3 may write its own code, which
    // is the one protection the kernel gets for free here and would have to
    // work to give up.
    // SAFETY: the frames are owned here and the addresses are in the user half,
    // which nothing else maps.
    unsafe {
        paging::map_page(CODE_BASE, code_frame, paging::USER).map_err(UserError::Map)?;
        paging::map_page(
            STACK_TOP - layout::PAGE_SIZE,
            stack_frame,
            paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
        )
        .map_err(UserError::Map)?;
    }

    kprintln!(
        "[user] {size} bytes of program at {CODE_BASE:#x}, stack at {:#x}",
        STACK_TOP - layout::PAGE_SIZE
    );

    sched::spawn_user("user", CODE_BASE, STACK_TOP).map_err(UserError::Spawn)?;
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
