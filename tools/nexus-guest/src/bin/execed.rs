//! The program the other one becomes.
//!
//! Started only by `execve`, and its whole job is to prove three things about
//! what happened: that it is running at all, that it got the arguments the
//! caller passed rather than a default, and that a descriptor the previous
//! program opened is still open.
//!
//! The last one is what makes `execve` useful rather than merely possible. A
//! launcher works out what to run and then becomes it, handing over whatever it
//! had already opened -- a connection, a log, a pipe to its parent. A kernel
//! that closed everything would leave the new program with nothing.
//!
//! | 170 | the wrong number of arguments arrived |
//! | 171 | an argument is not the one that was passed |
//! | 172 | the descriptor the previous program left open is not readable |
//! | 173 | what came out of it is not what went in |

#![no_std]
#![no_main]

use nexus_guest::{call, exit_group, expect, say, syscall3};

nexus_guest::guest_main!(run);

/// What the program that called `execve` passed, and wrote into the pipe.
const FIRST: &[u8] = b"/usr/bin/execed";
const SECOND: &[u8] = b"and-a-second-argument";
const THROUGH: &[u8] = b"across an execve";

/// The descriptor the previous program left open. Not a guess: it wrote the
/// number into its own argument list, and this reads it back.
const PASSED: &[u8] = b"fd=";

fn run() -> ! {
    // The stack the kernel left: `argc`, then the argument pointers.
    let stack = nexus_guest::stack();
    // SAFETY: the kernel puts `argc` at the stack pointer a program is entered
    // with, which is what `_start` recorded.
    let count = unsafe { core::ptr::read(stack) };
    expect(count == 3, 170);

    // SAFETY: `argc` pointers follow it, as the ABI says.
    let first = unsafe { core::ptr::read(stack.add(1)) } as *const u8;
    let second = unsafe { core::ptr::read(stack.add(2)) } as *const u8;
    let third = unsafe { core::ptr::read(stack.add(3)) } as *const u8;
    expect(same(first, FIRST), 171);
    expect(same(second, SECOND), 171);
    expect(starts_with(third, PASSED), 171);

    // The number after `fd=`, as the previous program wrote it.
    let mut descriptor: u64 = 0;
    let mut at = PASSED.len();
    loop {
        // SAFETY: a NUL-terminated string the kernel copied onto this stack.
        let byte = unsafe { *third.add(at) };
        if !byte.is_ascii_digit() {
            break;
        }
        descriptor = descriptor * 10 + u64::from(byte - b'0');
        at += 1;
    }

    let mut buffer = [0u8; 64];
    let got = syscall3(
        call::READ,
        descriptor,
        buffer.as_mut_ptr() as u64,
        buffer.len() as u64,
    );
    expect(got > 0, 172);
    expect(&buffer[..got as usize] == THROUGH, 173);

    say("guest: a program became another one, with its arguments and its descriptors");
    exit_group(0)
}

/// Whether a NUL-terminated string is exactly `wanted`.
fn same(text: *const u8, wanted: &[u8]) -> bool {
    let mut index = 0;
    while index < wanted.len() {
        // SAFETY: a NUL-terminated string the kernel copied onto this stack;
        // the loop stops at the first mismatch, so it never reads past the NUL.
        if unsafe { *text.add(index) } != wanted[index] {
            return false;
        }
        index += 1;
    }
    // SAFETY: as above -- this is the byte after the last one compared.
    unsafe { *text.add(wanted.len()) == 0 }
}

/// Whether it begins with `prefix`.
fn starts_with(text: *const u8, prefix: &[u8]) -> bool {
    let mut index = 0;
    while index < prefix.len() {
        // SAFETY: as in `same`.
        if unsafe { *text.add(index) } != prefix[index] {
            return false;
        }
        index += 1;
    }
    true
}
