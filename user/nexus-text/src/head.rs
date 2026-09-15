//! `head`: writes the first few lines of its input and stops.
//!
//! Stopping is the point, and it is why this reads a line at a time rather than
//! collecting the input first: `ls | head 3` should not wait for a listing of
//! ten thousand files to finish before showing three of them.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

const HEAP: usize = 256 * 1024;

/// How many lines, when nobody says.
const DEFAULT: usize = 10;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main)
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        nexus_user::log("head: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    let argument = nexus_text::arguments();
    let wanted = if argument.is_empty() {
        DEFAULT
    } else {
        match argument.parse::<usize>() {
            Ok(number) => number,
            // Refused rather than treated as the default. Somebody who typed
            // `head two` meant something, and quietly doing ten is not it.
            Err(_) => {
                nexus_text::say("head", &alloc::format!("head: {argument} is not a number"));
                nexus_user::exit_with(2);
            }
        }
    };

    let mut shown = 0usize;
    let had = nexus_text::for_each_line(|line| {
        if shown >= wanted {
            return false;
        }
        nexus_text::say("head", line);
        shown += 1;
        shown < wanted
    });

    if !had {
        nexus_text::say("head", "head: nothing was connected to this program's input");
        nexus_user::exit_with(1);
    }
    nexus_user::exit_with(0)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("head: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
