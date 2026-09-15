//! `count`: reads standard input to its end and says how much there was.
//!
//! The other half of a pipe. `ls` proved a program can be given somewhere to
//! write; this proves a program can be given somewhere to read, and `ls | count`
//! proves the two are the same channel with each end handed to a different
//! program.
//!
//! # What it is given
//!
//! | 1 | the channel back to whoever started it; the arguments are its first message |
//! | 2 | standard output |
//! | 3 | standard input |
//!
//! No directory. It never opens a file, and a program that cannot open a file
//! should not be handed the disk.
//!
//! # Why counting
//!
//! Because it is the smallest thing that cannot be faked. A program that
//! claimed a number without reading would get it wrong the moment the input
//! changed, and the count of *lines* is a different number from the count of
//! *bytes* and from the count of *messages* -- so all three are reported, and
//! all three have to agree with what was actually sent.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// How much of one. It holds no input: the counting is done as the bytes
/// arrive, so a gigabyte through this program costs the same as a line.
const HEAP: usize = 64 * 1024;

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
        nexus_user::log("count: FAILED: could not get a heap").ok();
        nexus_user::exit_with(101);
    }

    // The arguments, read and ignored. Read anyway, because they are the first
    // message on the parent channel and leaving them there would mean anything
    // that later reads that channel finds them instead of what it expected.
    let mut ignored = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    nexus_user::receive(nexus_user::PARENT, &mut ignored, &mut none).ok();

    let mut bytes = 0usize;
    let mut lines = 0usize;
    let mut messages = 0usize;
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];

    loop {
        match nexus_user::read_input(&mut buffer) {
            // The end of the input, which is whoever was writing having closed
            // their end. Not an error: it is how every pipe ends.
            Ok(None) => break,
            Ok(Some(got)) => {
                messages += 1;
                bytes += got;
                lines += buffer[..got].iter().filter(|byte| **byte == b'\n').count();
            }
            // No input at all, which is what running `count` on its own does.
            // Said rather than counted as zero, because zero is also what an
            // empty input gives and the two are not the same thing.
            Err(_) => {
                say("count: nothing was connected to this program's input");
                nexus_user::exit_with(1);
            }
        }
    }

    // A last line without a line ending still counts as a line, which is what
    // anybody looking at the output means by one.
    if bytes > 0 && lines == 0 {
        lines = 1;
    }

    say(&alloc::format!(
        "{lines} lines, {bytes} bytes, {messages} messages"
    ));
    nexus_user::exit_with(0)
}

/// Write one line to standard output, or to the log when there is none.
fn say(line: &str) {
    if nexus_user::println(line).is_err() {
        nexus_user::log(&alloc::format!("count: {line}")).ok();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&alloc::format!("count: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
