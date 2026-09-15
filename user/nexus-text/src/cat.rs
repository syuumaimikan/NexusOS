//! `cat`: writes a file, or its input, to standard output.
//!
//! With a name it reads that file out of the directory the shell lent it; with
//! none it copies its input, which is what makes `ls | cat` work and is the
//! behaviour the name comes from.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// Enough for a megabyte of file and the copy of it a read makes.
const HEAP: usize = 4 * 1024 * 1024;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main)
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        nexus_user::log("cat: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    let name = nexus_text::arguments();
    if name.is_empty() {
        // No name: copy the input. A line at a time, so that what comes out is
        // lines whatever the sender's message boundaries were.
        let had = nexus_text::for_each_line(|line| {
            nexus_text::say("cat", line);
            true
        });
        if !had {
            nexus_text::say("cat", "cat: give it a name, or something to read");
            nexus_user::exit_with(1);
        }
        nexus_user::exit_with(0);
    }

    match nexus_text::read_file(&name) {
        Ok(bytes) => {
            match core::str::from_utf8(&bytes) {
                Ok(text) => {
                    // Split here rather than sending the whole file as one
                    // message: a message is bounded, and a file is not.
                    for line in text.split('\n') {
                        nexus_text::say("cat", line);
                    }
                }
                // Said rather than written. A terminal handed the bytes of a
                // program would draw whatever they happen to be, and a pipe
                // would carry them into something expecting text.
                Err(_) => {
                    nexus_text::say(
                        "cat",
                        &alloc::format!("cat: {name} is not text ({} bytes)", bytes.len()),
                    );
                    nexus_user::exit_with(2);
                }
            }
            nexus_user::exit_with(0)
        }
        Err(error) => {
            nexus_text::say("cat", &alloc::format!("cat: {name}: {error}"));
            nexus_user::exit_with(1)
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("cat: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
