//! `grep`: writes the lines of its input that contain some text.
//!
//! A plain substring and not a regular expression. Saying so matters: somebody
//! who types `grep "a.c"` on another system means a pattern, and here it means
//! three characters. A program that silently treated one as the other would
//! match the wrong lines and look right doing it.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const HEAP: usize = 256 * 1024;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main)
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        nexus_user::log("grep: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    let wanted = nexus_text::arguments();
    if wanted.is_empty() {
        nexus_text::say("grep", "grep: say what to look for");
        nexus_user::exit_with(2);
    }

    let mut found = 0usize;
    let had = nexus_text::for_each_line(|line| {
        if line.contains(wanted.as_str()) {
            nexus_text::say("grep", line);
            found += 1;
        }
        true
    });

    if !had {
        nexus_text::say("grep", "grep: nothing was connected to this program's input");
        nexus_user::exit_with(2);
    }
    // One when nothing matched, which is what every other `grep` does and is
    // what lets a shell ask "was it there" rather than read the output.
    nexus_user::exit_with(u32::from(found == 0))
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("grep: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
