//! A Linux program with two threads and a lock between them.
//!
//! `clone` and `futex` are the two calls a C library builds threading out of,
//! and neither can be demonstrated by a program that does not actually run
//! concurrently: a `clone` that returned a plausible number and made no thread
//! would pass any check short of waiting for that thread to do something.
//!
//! So this program waits. The parent blocks in `FUTEX_WAIT` until the word the
//! child writes changes, and then checks a *second* value the child left in the
//! same page — because a futex that woke the parent without the child having
//! run would pass the first check and fail this one.
//!
//! # What it is checking, in order
//!
//! | 70, 71 | `mmap` for the shared page, and for the child's stack |
//! | 72 | `clone` |
//! | 73 | the parent gave up waiting: the child never woke it |
//! | 74 | the child ran but left the wrong value, so the page is not shared |
//! | 75 | the child's own `mmap`, made from the new thread |
//!
//! Seventy-five matters more than it looks. The child makes a system call of
//! its own before it signals, which is the only way to find out that the new
//! thread can reach the kernel at all — a thread entered with a broken register
//! frame can still execute a few instructions and store to memory.

use crate::Assembler;

/// What the child leaves in the shared page. Any value would do; this one is
/// recognisable in a memory dump and is not a small integer that could be there
/// by accident.
const MARKER: u64 = 0x4E58_5553_5448_5244; // "NXUSTHRD"

/// The flags a C library passes for a thread, and the ones this layer requires.
///
/// `CLONE_VM` so it is a thread and not a fork, `CLONE_FS` and `CLONE_FILES` so
/// it shares the working directory and the descriptors, `CLONE_SIGHAND` and
/// `CLONE_THREAD` so it is a thread of the same group. Linux refuses
/// `CLONE_THREAD` without `CLONE_SIGHAND`, and this layer refuses it too.
const THREAD_FLAGS: u32 = 0x100 | 0x200 | 0x400 | 0x800 | 0x1_0000;

/// `FUTEX_WAIT_PRIVATE` and `FUTEX_WAKE_PRIVATE`.
///
/// Private, because both threads are in one process, and because a shared futex
/// is refused here rather than silently treated as private -- see the note in
/// `compat/linux_threads.rs`.
const FUTEX_WAIT: u32 = 128;
const FUTEX_WAKE: u32 = 128 | 1;

/// Offsets inside the shared page.
///
/// The futex word is at zero -- a futex is a thirty-two bit word at a four-byte
/// aligned address, and the start of a page is the least surprising place for
/// one, which is why it is `[rbx]` throughout below rather than a named offset.
/// The rest are spread out so that one overrunning shows up as wrong data
/// rather than as a quiet overlap.
const MARKER_AT: u32 = 16;
const TIMEOUT_AT: u32 = 32;

/// How long the parent waits in one go, and how many times.
///
/// A `FUTEX_WAIT` with no timeout is the right thing for a real lock and the
/// wrong thing for a test: a child that never runs would leave the parent
/// asleep for ever, and what that looks like from outside is a boot that stops
/// with no message. Ten waits of a second each is long enough that a slow
/// machine does not fail and short enough that a broken one says so.
const WAIT_SECONDS: u64 = 1;
const WAIT_TRIES: u32 = 10;

/// The program.
pub fn machine_code(message: u64, length: u32) -> Vec<u8> {
    let mut a = Assembler::default();

    // A page both threads will use. It is ordinary anonymous memory: what makes
    // it shared is that a thread is in the same address space, which is the
    // whole claim `clone` is making.
    page(&mut a, 70);
    a.rbx_from_rax();

    // And a page for the child's stack. A thread needs one of its own -- Linux
    // would take a null stack to mean the parent's, which for a thread means
    // two threads writing one stack, so this layer refuses that and this
    // program does not ask for it.
    page(&mut a, 71);
    a.r12_from_rax();

    // The futex word starts at zero, which is what the parent will wait on, and
    // the marker starts at zero so that finding it set means the child set it.
    a.raw(&[0xC7, 0x03]).raw(&0u32.to_le_bytes()); // mov dword [rbx], 0
    a.raw(&[0x48, 0x31, 0xC0]); // xor rax, rax
    a.raw(&[0x48, 0x89, 0x43, MARKER_AT as u8]); // mov [rbx + MARKER_AT], rax

    // The timeout the parent will hand to `FUTEX_WAIT`: a `struct timespec`,
    // seconds then nanoseconds, written into the shared page because the
    // program has nowhere else to put a structure.
    a.raw(&[0x48, 0xB8]).raw(&WAIT_SECONDS.to_le_bytes()); // movabs rax, seconds
    a.raw(&[0x48, 0x89, 0x43, TIMEOUT_AT as u8]);
    a.raw(&[0x48, 0x31, 0xC0]); // xor rax, rax
    a.raw(&[0x48, 0x89, 0x43, (TIMEOUT_AT + 8) as u8]);

    // ---- clone ----------------------------------------------------------
    //
    // The child stack is the *top* of the page, because a stack grows down.
    a.raw(&[0x49, 0x8D, 0xB4, 0x24]).raw(&4096u32.to_le_bytes()); // lea rsi, [r12+4096]
    a.mov_edi(THREAD_FLAGS);
    a.raw(&[0x48, 0x31, 0xD2]); // xor rdx, rdx    -- no parent_tid
    a.raw(&[0x4D, 0x31, 0xD2]); // xor r10, r10    -- no child_tid
    a.raw(&[0x4D, 0x31, 0xC0]); // xor r8, r8      -- no CLONE_SETTLS
    a.mov_eax(56).syscall();

    // `syscall` does not touch the flags, so the value has to be tested before
    // it can be branched on. Leaving this out is a branch on whatever the last
    // comparison set -- which in this program was the `mmap` check above, and
    // which therefore sent *both* threads down the parent's path on a machine
    // where everything worked.
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax

    // Zero means this is the child. The jump distance is not known yet: the
    // child's code is emitted last, after the parent's, so this is a
    // placeholder patched below.
    a.raw(&[0x0F, 0x84]); // jz rel32
    let to_child = a.at();
    a.raw(&0u32.to_le_bytes());

    // ---- the parent -----------------------------------------------------
    a.expect_not_negative(72);
    a.raw(&[0x41, 0xBD]).raw(&WAIT_TRIES.to_le_bytes()); // mov r13d, WAIT_TRIES

    let wait_top = a.at();
    // Look before sleeping. A waiter that slept first would sleep through a
    // wake that had already happened, which is the whole reason `FUTEX_WAIT`
    // takes the value it expects rather than just an address.
    a.raw(&[0x8B, 0x03]); // mov eax, [rbx]
    a.raw(&[0x85, 0xC0]); // test eax, eax
    a.raw(&[0x0F, 0x85]); // jnz -- out of the loop; patched below
    let to_woken = a.at();
    a.raw(&0u32.to_le_bytes());

    a.raw(&[0x48, 0x89, 0xDF]); // mov rdi, rbx     -- the futex
    a.mov_esi(FUTEX_WAIT);
    a.raw(&[0x48, 0x31, 0xD2]); // xor rdx, rdx     -- expecting zero
    a.raw(&[0x4C, 0x8D, 0x53, TIMEOUT_AT as u8]); // lea r10, [rbx+TIMEOUT_AT]
    a.mov_eax(202).syscall();

    // A wait that returned is permission to look again, not a promise. Round
    // the loop until the word changes or the tries run out -- and running out
    // is its own status, because "the child never woke me" and "the child woke
    // me with the wrong value" are different failures.
    a.raw(&[0x41, 0xFF, 0xCD]); // dec r13d
    let back = -((a.at() + 2 - wait_top) as i64);
    a.raw(&[
        0x75,
        i8::try_from(back).expect("the wait loop is short") as u8,
    ]); // jnz
    a.raw(&Assembler::stop(73));

    // Woken, and the word changed. Now the second question: did the child
    // really run, and was it really writing into the same memory?
    let woken = a.at();
    patch(&mut a, to_woken, woken);
    a.raw(&[0x48, 0x8B, 0x43, MARKER_AT as u8]); // mov rax, [rbx + MARKER_AT]
    a.raw(&[0x48, 0xB9]).raw(&MARKER.to_le_bytes()); // movabs rcx, MARKER
    a.raw(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
    a.unless(0x74, 74);

    // Both threads did what they were for. Say so, and end the whole program --
    // `exit_group` and not `exit`, because the child is gone and this is the
    // program finishing rather than a thread finishing.
    a.mov_edi(1)
        .mov_rsi_imm(message)
        .mov_edx(length)
        .mov_eax(1)
        .syscall();
    a.mov_eax(231).mov_edi(0).syscall();

    // ---- the child ------------------------------------------------------
    let child = a.at();
    patch(&mut a, to_child, child);

    // A system call of its own, first. A thread entered with a broken register
    // frame can still execute instructions and store to memory; whether it can
    // reach the kernel is a different question, and this is the one that asks
    // it. Its failure is the child's, so it ends the whole program.
    page(&mut a, 75);
    // Given straight back: what was being tested is that the call worked.
    a.raw(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    a.mov_esi(4096).mov_eax(11).syscall(); // munmap

    // The marker first and the word second, in that order. The parent wakes on
    // the word and then reads the marker, so a child that set them the other
    // way round would be a child racing its own signal.
    a.raw(&[0x48, 0xB8]).raw(&MARKER.to_le_bytes()); // movabs rax, MARKER
    a.raw(&[0x48, 0x89, 0x43, MARKER_AT as u8]); // mov [rbx + MARKER_AT], rax
    a.raw(&[0xC7, 0x03]).raw(&1u32.to_le_bytes()); // mov dword [rbx], 1

    a.raw(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
    a.mov_esi(FUTEX_WAKE);
    a.mov_edx(1) // wake one, which is all there is
        .mov_eax(202)
        .syscall();

    // `exit` and not `exit_group`: this thread is finishing and the program is
    // not. A kernel that treated the two as one call would take the parent down
    // here, and the parent is the thread with the message.
    a.mov_eax(60).mov_edi(0).syscall();
    // Unreachable, and worth being an instruction that stops rather than
    // whatever follows in memory.
    a.raw(&[0xF4]); // hlt

    a.code
}

/// `mmap` one anonymous read-write page, or stop with `code`.
fn page(a: &mut Assembler, code: u32) {
    a.mov_edi(0)
        .mov_esi(4096)
        .mov_edx(3) // PROT_READ | PROT_WRITE
        .mov_r10d(0x22) // MAP_PRIVATE | MAP_ANONYMOUS
        .mov_r8d(u32::MAX) // no descriptor
        .mov_r9d(0)
        .mov_eax(9)
        .syscall();
    a.expect_not_negative(code);
}

/// Fill in a `rel32` that was emitted before its target was known.
///
/// The displacement is from the end of the instruction, which is the four bytes
/// of the placeholder itself -- so `at + 4` and not `at`.
fn patch(a: &mut Assembler, at: usize, target: usize) {
    let displacement = (target as i64) - (at as i64 + 4);
    let bytes = i32::try_from(displacement)
        .expect("the program is smaller than two gigabytes")
        .to_le_bytes();
    a.code[at..at + 4].copy_from_slice(&bytes);
}
