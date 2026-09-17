//! A program built for i386 Linux.
//!
//! Thirty-two bit, `ET_EXEC`, `EM_386`, static, with no C runtime — the shape
//! of a great deal of older Linux software, and of the Steam bootstrap.
//!
//! It is not a smaller version of the programs next door. Every system call it
//! makes goes by a different number, through a different instruction, with its
//! arguments in different registers:
//!
//! | | x86-64 | i386 |
//! |---|---|---|
//! | the way in | `syscall` | `int 0x80` |
//! | `write` | 1 | 4 |
//! | `exit_group` | 231 | 252 |
//! | arguments | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `ebx`, `ecx`, `edx`, `esi`, `edi`, `ebp` |
//!
//! # What it checks
//!
//! | 240 | `write` did not report what it took |
//! | 241 | `getpid` |
//! | 242 | `mmap2` |
//! | 243 | what was written into the mapped page did not read back |
//! | 244 | `munmap` |
//! | 245 | the auxiliary vector is not thirty-two bit words |
//! | 246 | `AT_ENTRY` is not where this code actually is |
//!
//! Two hundred and forty-five is the one worth naming. A thirty-two bit
//! program's `argc`, arguments, environment and auxiliary vector are all
//! *four-byte* words, and a kernel that wrote them as eight would give this
//! program the top half of one value where the next should be. Walking the
//! vector and finding `AT_ENTRY` at all is the check; comparing it against
//! where the program really is, is the proof.

#![no_std]
#![no_main]

use core::arch::asm;

/// i386's numbers, which are not x86-64's.
mod call {
    pub const WRITE: u32 = 4;
    pub const GETPID: u32 = 20;
    pub const MUNMAP: u32 = 91;
    pub const MMAP2: u32 = 192;
    pub const EXIT_GROUP: u32 = 252;
}

/// `AT_ENTRY`, which is the same number at either width.
const AT_ENTRY: u32 = 9;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    stop(99)
}

/// A system call with three arguments.
fn syscall3(number: u32, a: u32, b: u32, c: u32) -> i32 {
    let out: i32;
    // SAFETY: `int 0x80` with a number this program chose. On i386 the kernel
    // preserves every register but `eax`, which is why nothing else is
    // declared clobbered.
    unsafe {
        asm!("int 0x80", inlateout("eax") number as i32 => out,
             in("ebx") a, in("ecx") b, in("edx") c, options(nostack));
    }
    out
}

// A six-argument system call, written as a whole function in assembly -- which
// is what every thirty-two bit C library does with this call, and for the same
// two reasons. `esi` and `ebp` are reserved by the compiler: `ebp` is the frame
// pointer and `esi` is used internally, so neither can be an operand of an
// inline `asm!`. And all four callee-saved registers have to be put back before
// returning, because the caller is entitled to assume nothing touched them.
//
// The arguments are on the stack, as the i386 C convention puts them: after
// four pushes and the return address, the first is twenty bytes up.
core::arch::global_asm!(
    ".globl guest32_syscall6",
    ".type guest32_syscall6, @function",
    "guest32_syscall6:",
    "push ebp",
    "push ebx",
    "push esi",
    "push edi",
    "mov eax, [esp + 20]",
    "mov ebx, [esp + 24]",
    "mov ecx, [esp + 28]",
    "mov edx, [esp + 32]",
    "mov esi, [esp + 36]",
    "mov edi, [esp + 40]",
    "mov ebp, [esp + 44]",
    "int 0x80",
    "pop edi",
    "pop esi",
    "pop ebx",
    "pop ebp",
    "ret",
);

unsafe extern "C" {
    /// The six-argument system call above.
    ///
    /// # Safety
    ///
    /// Makes whatever call `number` names, with whatever the arguments mean to
    /// it.
    fn guest32_syscall6(number: u32, a: u32, b: u32, c: u32, d: u32, e: u32, f: u32) -> i32;
}

fn syscall6(number: u32, a: u32, b: u32, c: u32, d: u32, e: u32, f: u32) -> i32 {
    // SAFETY: the stub above, with a number this program chose.
    unsafe { guest32_syscall6(number, a, b, c, d, e, f) }
}

/// Stop, with a status that says which step failed.
fn stop(status: u32) -> ! {
    let _ = syscall3(call::EXIT_GROUP, status, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

fn expect(condition: bool, step: u32) {
    if !condition {
        stop(step);
    }
}

/// Write to standard output.
fn say(text: &[u8]) -> i32 {
    syscall3(call::WRITE, 1, text.as_ptr() as u32, text.len() as u32)
}

/// Where the kernel left the stack: `argc`, then the rest, in four-byte words.
static mut STACK: *const u32 = core::ptr::null();

core::arch::global_asm!(
    ".globl _start",
    ".type _start, @function",
    "_start:",
    // The same reasoning as at sixty-four bits: `_start` is jumped to with the
    // stack pointer aligned, and a compiled function expects it eight past a
    // boundary. Sixteen here too -- the i386 ABI has required sixteen-byte
    // stack alignment at a call since 2005, and a compiler emits `movaps`
    // against it.
    "xor ebp, ebp",
    "mov eax, esp",
    "and esp, -16",
    "push eax",
    "call __guest32_main",
    "ud2",
);

/// The first Rust code in the program.
///
/// # Safety
///
/// Called once, by the stub above, with the kernel's stack pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __guest32_main(stack: *const u32) -> ! {
    // SAFETY: called once, before anything else in this program runs.
    unsafe { STACK = stack };
    run()
}

fn run() -> ! {
    // Compared against the slice's own length rather than a number counted by
    // hand, which is the sort of thing to get wrong once and then not notice.
    const GREETING: &[u8] = b"guest32: a thirty-two bit linux program is running\n";
    expect(say(GREETING) == GREETING.len() as i32, 240);

    let pid = syscall3(call::GETPID, 0, 0, 0);
    expect(pid > 0, 241);

    // A page, through `mmap2` -- whose offset is in pages rather than bytes,
    // which is the whole reason i386 has a second `mmap`.
    const PROT_READ_WRITE: u32 = 3;
    const MAP_PRIVATE_ANONYMOUS: u32 = 0x22;
    let page = syscall6(
        call::MMAP2,
        0,
        4096,
        PROT_READ_WRITE,
        MAP_PRIVATE_ANONYMOUS,
        u32::MAX,
        0,
    );
    expect(page > 0, 242);

    // SAFETY: a page this program just asked for and nothing else holds.
    unsafe {
        core::ptr::write_volatile(page as *mut u32, 0x5A5A_3232);
        expect(
            core::ptr::read_volatile(page as *const u32) == 0x5A5A_3232,
            243,
        );
    }
    expect(syscall3(call::MUNMAP, page as u32, 4096, 0) == 0, 244);

    // The auxiliary vector, in four-byte words. Walked past `argc`, the
    // arguments and their null, the environment and its null.
    // SAFETY: the stack the kernel left, recorded by the entry stub.
    let mut at = unsafe { STACK };
    // SAFETY: `argc` is the first word, and the pointers follow it.
    let count = unsafe { core::ptr::read(at) } as usize;
    // SAFETY: past `argc` and its `count` pointers, to the null that ends them.
    at = unsafe { at.add(1 + count + 1) };
    loop {
        // SAFETY: the environment's pointers, ending at a null.
        let value = unsafe { core::ptr::read(at) };
        at = unsafe { at.add(1) };
        if value == 0 {
            break;
        }
    }

    let mut entry = 0u32;
    for _ in 0..64 {
        // SAFETY: the pairs, ending at `AT_NULL`.
        let kind = unsafe { core::ptr::read(at) };
        // SAFETY: as above.
        let value = unsafe { core::ptr::read(at.add(1)) };
        if kind == 0 {
            break;
        }
        if kind == AT_ENTRY {
            entry = value;
        }
        // SAFETY: as above.
        at = unsafe { at.add(2) };
    }
    expect(entry != 0, 245);

    // And where this code really is. `_start` is the entry point, so the
    // auxiliary vector's answer and the linker's have to agree.
    let here = start_address();
    expect(entry == here, 246);

    let _ = say(b"guest32: mmap2, getpid and a 32-bit auxiliary vector all behaved\n");
    stop(0)
}

/// The address of `_start`, as the linker placed it.
fn start_address() -> u32 {
    unsafe extern "C" {
        fn _start();
    }
    _start as *const () as u32
}
