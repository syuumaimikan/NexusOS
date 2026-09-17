//! A program that becomes another one.
//!
//! `execve` with no `fork` in front of it, which is what a bootstrapper is: a
//! program that works out what to run and then *is* it. Nearly every installer
//! and launcher has this shape, Steam's included.
//!
//! What it hands over is the part that matters. It opens a pipe, writes a word
//! into it, and passes the reading end's number to the new program in its own
//! argument list -- so the new program can only pass its checks if the
//! descriptor survived the replacement.
//!
//! | 160 | `pipe2` |
//! | 161 | writing into the pipe |
//! | 162 | `execve` returned, which means it failed |

#![no_std]
#![no_main]

use nexus_guest::{call, expect, syscall2, syscall3};

nexus_guest::guest_main!(run);

/// What goes into the pipe, for the other side of the `execve` to read.
const THROUGH: &[u8] = b"across an execve";
/// The program to become, and the argument it is given.
const BECOME: &[u8] = b"/usr/bin/execed\0";
const SECOND: &[u8] = b"and-a-second-argument\0";

fn run() -> ! {
    let mut ends = [0i32; 2];
    expect(syscall2(call::PIPE2, ends.as_mut_ptr() as u64, 0) == 0, 160);
    expect(
        syscall3(
            call::WRITE,
            ends[1] as u64,
            THROUGH.as_ptr() as u64,
            THROUGH.len() as u64,
        ) == THROUGH.len() as i64,
        161,
    );

    // The reading end's number, written into an argument. A program that is
    // about to stop existing cannot tell the next one anything except through
    // what survives, and its arguments are the simplest of those.
    let mut third = [0u8; 16];
    third[..3].copy_from_slice(b"fd=");
    let mut value = ends[0] as u64;
    let mut digits = [0u8; 8];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
        if value == 0 {
            break;
        }
    }
    let mut at = 3;
    while count > 0 {
        count -= 1;
        third[at] = digits[count];
        at += 1;
    }
    third[at] = 0;

    // `argv` and `envp`, each a null-terminated array of pointers.
    let argv = [
        BECOME.as_ptr() as u64,
        SECOND.as_ptr() as u64,
        third.as_ptr() as u64,
        0,
    ];
    let envp = [0u64];

    let failed = syscall3(
        call::EXECVE,
        BECOME.as_ptr() as u64,
        argv.as_ptr() as u64,
        envp.as_ptr() as u64,
    );
    // `execve` does not return when it works. Reaching this line at all is the
    // failure, and the value it came back with says why.
    nexus_guest::fmt::say_with("guest: execve came back: ", failed);
    nexus_guest::fail(162)
}
