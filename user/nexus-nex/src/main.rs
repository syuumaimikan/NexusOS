//! `nex`: runs a Nex program.
//!
//! With a name it reads that file out of the directory the shell lent it; with
//! none it reads its standard input, so `cat program.nex | nex` works and so
//! does writing a program in the editor and running it without leaving the
//! machine.
//!
//! # What it is given
//!
//! | 1 | the channel back to whoever started it; the arguments are its first message |
//! | 2 | standard output |
//! | 3 | standard input |
//! | 4 | the directory the program may be read from |
//!
//! Nothing else. A Nex program cannot open a file, reach the network or start
//! another program, because this interpreter has no built-in that does any of
//! those -- and so the language needs no permission model of its own. What a
//! program can do is decided entirely by what this process was lent, which is
//! the same rule everything else on this machine follows.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::panic::PanicInfo;

use nexus_user::Handle;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// Enough for a program, its tree, and whatever it prints.
const HEAP: usize = 8 * 1024 * 1024;

/// The directory a program may be read from.
const DIRECTORY: Handle = Handle(4);

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main)
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        nexus_user::log("nex: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    let name = nexus_api::arguments();
    let source = if name.is_empty() {
        match read_input() {
            Some(text) => text,
            None => {
                say("nex: give it the name of a program, or something to read");
                nexus_user::exit_with(2);
            }
        }
    } else {
        match nexus_api::path::read(DIRECTORY, &name) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    say(&alloc::format!("nex: {name} is not text"));
                    nexus_user::exit_with(2);
                }
            },
            Err(error) => {
                say(&alloc::format!("nex: {name}: {error}"));
                nexus_user::exit_with(1);
            }
        }
    };

    match nexus_lang::run(&source) {
        Ok(printed) => {
            for line in printed {
                say(&line);
            }
            nexus_user::exit_with(0)
        }
        // The line and the reason, as the language reports them. To standard
        // output *and* to the log: the person who wrote the program needs to
        // read it in the window, and the log is the only way anything outside
        // the machine can tell a program that failed from one that printed
        // nothing. `say` alone would do only the first, because it falls back
        // to the log rather than doing both.
        Err(trouble) => {
            let text = alloc::format!("nex: {trouble}");
            nexus_user::log(&text).ok();
            nexus_user::println(&text).ok();
            nexus_user::exit_with(1)
        }
    }
}

/// Everything on standard input, as text.
///
/// `None` when there is no input at all, which is what running `nex` on its own
/// does -- a different thing from an input that was empty.
fn read_input() -> Option<String> {
    let mut text = String::new();
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut anything = false;
    loop {
        match nexus_user::read_input(&mut buffer) {
            Ok(None) => break,
            Ok(Some(got)) => {
                anything = true;
                text.push_str(&alloc::string::String::from_utf8_lossy(&buffer[..got]));
            }
            Err(_) => return None,
        }
    }
    if anything { Some(text) } else { None }
}

/// Write one line to standard output, or to the log when there is none.
fn say(line: &str) {
    if nexus_user::println(line).is_err() {
        nexus_user::log(&alloc::format!("nex: {line}")).ok();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("nex: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
