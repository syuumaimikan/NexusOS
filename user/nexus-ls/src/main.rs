//! `ls`: lists a directory, and writes the listing to standard output.
//!
//! This is a small program and it is the first one on this machine that did not
//! have to be built into the terminal. Until the spawn service could lend a new
//! program a channel, there was nowhere for a program to put text a person was
//! meant to read -- so `ls` was a method on the shell, and so was everything
//! else a shell does. The roadmap called that out for a long time: "the thing
//! that would let `ls` stop being built into the terminal".
//!
//! # What it is given
//!
//! | 1 | the channel back to whoever started it; the arguments are its first message |
//! | 2 | standard output, which the terminal reads |
//! | 3 | standard input, which this does not use |
//! | 4 | the directory to list |
//!
//! Nothing else. It cannot open the disk, reach the network, or name another
//! process. If the caller lends it a subdirectory it lists that; there is no
//! path by which it could list anything above what it was handed, because `open`
//! refuses a name with a separator in it and this program never asks for one.
//!
//! # What it does not do
//!
//! No columns, no sizes, no sorting options, no recursion, no colours. One name
//! a line, directories marked with a trailing slash, sorted. A listing that goes
//! down a pipe wants one thing per line far more than it wants columns, and the
//! terminal is free to lay the lines out however it likes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_user::{Handle, Kind};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// How much of one. A listing is names and nothing else, so this is generous
/// for what it does and small enough that starting the program is cheap --
/// which matters for something a person runs in a loop.
const HEAP: usize = 256 * 1024;

/// The directory this was asked to list.
///
/// Four, because two and three are standard output and standard input. A caller
/// that lends nothing here gets the refusal below rather than a listing of
/// something else.
const DIRECTORY: Handle = Handle(4);

/// The most a directory's packed entries may come to.
///
/// One read, because `list` fills a buffer and says how much it used; a
/// directory larger than this is reported as such rather than silently cut, so
/// nobody is shown half a directory and told it is all of it.
const PACKED: usize = 8192;

/// The entry point the kernel jumps to.
///
/// See `nexus-init` for why this is naked: there is no C runtime to set
/// anything up, and no return address on the stack to return to.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        nexus_user::log("ls: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    // The arguments, which arrive as the first message on the parent channel.
    // Read before anything else is done, because they may name a subdirectory
    // and listing the wrong thing first would be work thrown away.
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    let argument = match nexus_user::receive(nexus_user::PARENT, &mut buffer, &mut none) {
        Ok(received) => core::str::from_utf8(&buffer[..received.bytes])
            .unwrap_or("")
            .trim()
            .to_string(),
        // No arguments is not an error: `ls` with nothing after it lists where
        // it was pointed.
        Err(_) => String::new(),
    };

    let status = run(&argument);
    nexus_user::exit_with(status);
}

/// List it, and say whether that worked.
///
/// Returns what the process exits with, which is what the terminal reports: nought
/// for a listing, one for a directory that could not be read, two for a name
/// that is not there.
fn run(argument: &str) -> u32 {
    // A name opens that directory and lists it; no name lists the one this was
    // lent. Either way nothing above the lent directory is reachable, because
    // `open` takes one component and refuses a separator.
    let (target, borrowed) = if argument.is_empty() || argument == "." {
        (DIRECTORY, false)
    } else {
        match nexus_user::open(DIRECTORY, argument) {
            Ok(handle) => (handle, true),
            Err(error) => {
                say(&alloc::format!("ls: {argument}: {error}"));
                return 2;
            }
        }
    };

    let mut packed = [0u8; PACKED];
    let read = nexus_user::list(target, &mut packed);
    if borrowed {
        nexus_user::close(target).ok();
    }
    let Ok(length) = read else {
        say(&alloc::format!("ls: {argument}: not a directory"));
        return 1;
    };
    // A full buffer means there may have been more. Said, rather than left for
    // somebody to notice that a directory stopped growing at a round number.
    if length == PACKED {
        say("ls: this directory is larger than one listing holds");
    }

    let mut names: Vec<String> = Vec::new();
    for entry in nexus_user::entries(&packed[..length]) {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        names.push(match entry.kind {
            // The slash is the only marking here. It is not decoration: a
            // listing that goes into another program has to carry the one
            // distinction that changes what can be done with a name.
            Kind::Directory => alloc::format!("{}/", entry.name),
            Kind::File => entry.name.to_string(),
        });
    }

    names.sort();
    for name in &names {
        say(name);
    }
    0
}

/// Write one line to standard output, or to the log when there is none.
///
/// Both, rather than one: a program run from the terminal has somewhere to
/// write, and the same program started by `init` at boot has not. Falling back
/// to the log means the second case still leaves a record, and it means this
/// program is testable without a terminal.
fn say(line: &str) {
    if nexus_user::println(line).is_err() {
        nexus_user::log(&alloc::format!("ls: {line}")).ok();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("ls: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
