//! A program NexusOS runs because another program asked for it.
//!
//! It is started by the spawn service, not by the kernel deciding to, and it is
//! handed one thing: a channel to whoever asked. It says hello down that
//! channel, waits for a reply, and exits.
//!
//! There is nothing else it can reach. It has no name for the process that
//! started it, no way to find any other, and no call that would let it ask. The
//! handle is the whole of its authority, which is the point.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel to whoever asked for this program, as the kernel hands it over:
/// the first entry in an otherwise empty table.
const PARENT: Handle = Handle(1);

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
    nexus_user::log("hello: started because another program asked for me").ok();

    const GREETING: &[u8] = b"hello, from the program you asked for";
    if nexus_user::send(PARENT, GREETING, &[]).is_err() {
        nexus_user::log("hello: FAILED: could not reach whoever started me").ok();
        nexus_user::exit();
    }

    let mut buffer = [0u8; 64];
    let mut handles = [Handle(0); 1];
    match nexus_user::receive(PARENT, &mut buffer, &mut handles) {
        Ok(received) if received.bytes > 0 => {
            // Logged back rather than merely counted, so the boot log shows
            // what crossed rather than that something did.
            let text = core::str::from_utf8(&buffer[..received.bytes]).unwrap_or("<not text>");
            nexus_user::log(text).ok();
        }
        _ => {
            nexus_user::log("hello: FAILED: no answer came back").ok();
            nexus_user::exit();
        }
    }

    nexus_user::exit()
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("hello: PANIC").ok();
    nexus_user::exit()
}
