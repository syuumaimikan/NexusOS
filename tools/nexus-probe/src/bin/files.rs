//! A Linux program that renames a file and checks where it went.
//!
//! # Why this is separate from the probe
//!
//! `nexus-probe` promises to change nothing, and that promise is what makes it
//! safe to run at any time. This program creates a file, writes to it, moves it
//! and deletes it, so it cannot live in there.
//!
//! # What it checks, and why each
//!
//! A rename is two directory writes, and every way of getting it wrong loses
//! something:
//!
//! - **The file is at the new name** -- or the move went nowhere.
//! - **With the same contents** -- or the move copied badly, or moved a
//!   different file.
//! - **The old name is gone** -- or the file is now named twice, and deleting
//!   one of them takes the other's blocks.
//!
//! The third is the one a careless implementation passes by accident, because
//! it is the only one that fails silently: two names for one inode looks
//! exactly right until something is deleted.

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

const AT_FDCWD: u64 = (-100i64) as u64;
const O_WRONLY: u64 = 1;
const O_RDONLY: u64 = 0;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;

/// SAFETY: every call below is made with arguments this file owns.
unsafe fn call(number: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let out: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as i64 => out,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    out
}

fn write_out(text: &[u8]) {
    // SAFETY: standard output, a real pointer and its real length.
    unsafe {
        call(1, 1, text.as_ptr() as u64, text.len() as u64, 0);
    }
}

fn say(parts: &[&str]) {
    let mut line = [0u8; 160];
    let mut at = 0;
    for part in parts {
        for byte in part.as_bytes() {
            if at < line.len() - 1 {
                line[at] = *byte;
                at += 1;
            }
        }
    }
    line[at] = b'\n';
    write_out(&line[..at + 1]);
}

fn exit(code: i32) -> ! {
    // SAFETY: exit_group takes a status and does not return.
    unsafe {
        call(231, code as u64, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    say(&["files: PANIC"]);
    exit(101)
}

/// The two names. In the Linux root, which is the only place a translated
/// program may write.
const FIRST: &[u8] = b"/tmp-rename-before\0";
const SECOND: &[u8] = b"/tmp-rename-after\0";
const CONTENTS: &[u8] = b"a file that was moved";

fn openat(path: &[u8], flags: u64) -> i64 {
    // SAFETY: a null-terminated path this file owns.
    unsafe { call(257, AT_FDCWD, path.as_ptr() as u64, flags, 0o644) }
}

fn close(fd: i64) {
    // SAFETY: a descriptor this file opened.
    unsafe {
        call(3, fd as u64, 0, 0, 0);
    }
}

#[unsafe(naked)]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // The stack is sixteen-byte aligned at entry with no return address on it,
    // and the compiler assumes the opposite. See `nexus-probe`'s `_start`.
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "and rsp, -16",
        "call {main}",
        "ud2",
        main = sym run,
    )
}

extern "C" fn run() -> ! {
    say(&["files: moving a file, and looking at where it went"]);
    let mut wrong = 0;

    // Make it.
    let fd = openat(FIRST, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        say(&["files: FAILED: could not create the first name"]);
        exit(1);
    }
    // SAFETY: a descriptor just opened for writing, and a buffer this file owns.
    let written = unsafe {
        call(
            1,
            fd as u64,
            CONTENTS.as_ptr() as u64,
            CONTENTS.len() as u64,
            0,
        )
    };
    close(fd);
    if written != CONTENTS.len() as i64 {
        say(&["files: FAILED: the first write was short"]);
        exit(1);
    }

    // Move it.
    // SAFETY: two null-terminated paths this file owns.
    let moved = unsafe { call(82, FIRST.as_ptr() as u64, SECOND.as_ptr() as u64, 0, 0) };
    if moved != 0 {
        say(&["files: FAILED: rename refused"]);
        exit(1);
    }

    // It is at the new name, with what was written into it.
    let fd = openat(SECOND, O_RDONLY);
    if fd < 0 {
        say(&["files: FAILED: nothing at the new name"]);
        exit(1);
    }
    let mut read_back = [0u8; 64];
    // SAFETY: a descriptor just opened for reading, and a buffer this file owns.
    let got = unsafe {
        call(
            0,
            fd as u64,
            read_back.as_mut_ptr() as u64,
            read_back.len() as u64,
            0,
        )
    };
    close(fd);
    if got != CONTENTS.len() as i64 || &read_back[..CONTENTS.len()] != CONTENTS {
        say(&["files: FAILED: the new name holds something else"]);
        wrong += 1;
    } else {
        say(&["files: the file is at the new name, with the bytes it had"]);
    }

    // And the old name is gone. The check a careless rename passes by accident,
    // because two names for one file look right until one of them is deleted.
    let fd = openat(FIRST, O_RDONLY);
    if fd >= 0 {
        close(fd);
        say(&["files: FAILED: the old name still opens, so the file is named twice"]);
        wrong += 1;
    } else {
        say(&["files: the old name is gone"]);
    }

    // Tidy up, so a second run starts where the first did.
    // SAFETY: a null-terminated path this file owns.
    unsafe {
        call(263, AT_FDCWD, SECOND.as_ptr() as u64, 0, 0);
    }

    if wrong == 0 {
        say(&["files: rename moved the file and left nothing behind"]);
        exit(0)
    }
    exit(1)
}
