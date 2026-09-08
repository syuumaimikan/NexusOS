//! `init`: the first program NexusOS loads from a disk.
//!
//! Everything that ran in ring 3 before this was assembled into the kernel and
//! copied into a page. This one is an ELF file on a filesystem, built as its
//! own binary, read off the disk by the kernel and loaded into an address space
//! of its own. That is the difference between a system that can run user code
//! and a system that can run *programs*.
//!
//! What it does is deliberately small. It says who it is, checks that the
//! system calls it can reach behave, and exits. It is the thing every later
//! program starts from, so what matters is that the path from a file on disk to
//! a running process is real, not that this particular process is interesting.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// The entry point the kernel jumps to.
///
/// `naked` because there is no C runtime to set anything up and nothing to
/// return to: the kernel enters here with a fresh stack and no return address
/// on it, so an ordinary function's epilogue would return into nothing. The
/// stack is already aligned the way the ABI wants at a call boundary, so the
/// body is reached with a plain `call`.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // Terminate the frame chain, so anything walking it stops here rather
        // than following whatever the stack happened to contain.
        "xor rbp, rbp",
        "call {main}",
        // `main` does not return; this is what happens if it ever does.
        "ud2",
        main = sym main,
    )
}

/// What the program actually does.
extern "C" fn main() -> ! {
    nexus_user::log("init: loaded from disk and running in ring 3").ok();

    // The uptime, twice, with a yield between. Two different answers say the
    // clock is running and that this process is one among others rather than
    // the only thing on the machine.
    let first = nexus_user::uptime();
    for _ in 0..64 {
        nexus_user::yield_now();
    }
    let second = nexus_user::uptime();

    if second < first {
        nexus_user::log("init: FAILED: the clock went backwards").ok();
        nexus_user::exit();
    }

    // A channel to itself, which is the whole of the IPC interface exercised
    // from a program that was not compiled into the kernel.
    let Ok((writer, reader)) = nexus_user::channel() else {
        nexus_user::log("init: FAILED: could not create a channel").ok();
        nexus_user::exit();
    };

    const GREETING: &[u8] = b"a message from a program on disk";
    if nexus_user::send(writer, GREETING, &[]) != Ok(GREETING.len()) {
        nexus_user::log("init: FAILED: could not send on its own channel").ok();
        nexus_user::exit();
    }

    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 1];
    match nexus_user::receive(reader, &mut buffer, &mut handles) {
        Ok(received) if received.bytes == GREETING.len() && received.handles == 0 => {
            if &buffer[..received.bytes] != GREETING {
                nexus_user::log("init: FAILED: the message came back changed").ok();
                nexus_user::exit();
            }
        }
        _ => {
            nexus_user::log("init: FAILED: could not read its own message").ok();
            nexus_user::exit();
        }
    }

    // A handle it was never given has to be refused rather than answered.
    if nexus_user::rights(nexus_user::Handle(9999)) != Err(nexus_user::Error::BadHandle) {
        nexus_user::log("init: FAILED: a handle it never had was accepted").ok();
        nexus_user::exit();
    }

    nexus_user::log("init: clock, channel and handle checks all passed").ok();

    ask_for_a_program();
    nexus_user::exit()
}

/// The channel to the spawn service, as the kernel hands it over: the first
/// entry in an otherwise empty table.
const SPAWNER: nexus_user::Handle = nexus_user::Handle(1);

/// Ask for another program to be started, and then talk to it.
///
/// There is no system call that creates a process. There is a channel, and
/// holding one end of it is the authority to ask; a program that was never
/// given this handle cannot ask, and there is no name it could use instead.
/// What comes back is a handle to the thing that started, so the answer is an
/// introduction rather than a notification.
fn ask_for_a_program() {
    const PROGRAM: &[u8] = b"BIN/HELLO.ELF";

    if nexus_user::send(SPAWNER, PROGRAM, &[]).is_err() {
        nexus_user::log("init: FAILED: could not reach the spawn service").ok();
        return;
    }

    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 1];
    let received = match nexus_user::receive(SPAWNER, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            nexus_user::log("init: FAILED: the spawn service did not answer").ok();
            return;
        }
    };

    if received.handles != 1 {
        // The reply text says why, and is worth showing: a refusal is as
        // interesting as a success and reads the same way in a boot log.
        let text = core::str::from_utf8(&buffer[..received.bytes]).unwrap_or("<not text>");
        nexus_user::log(text).ok();
        nexus_user::log("init: FAILED: no channel to the new program came back").ok();
        return;
    }

    let child = handles[0];

    // Whatever it says first. It was started by the kernel on this program's
    // behalf and neither of them can name the other, so this channel is the
    // only thing connecting them.
    let mut incoming = [0u8; 64];
    let mut none = [nexus_user::Handle(0); 1];
    match nexus_user::receive(child, &mut incoming, &mut none) {
        Ok(reply) if reply.bytes > 0 => {
            let text = core::str::from_utf8(&incoming[..reply.bytes]).unwrap_or("<not text>");
            nexus_user::log(text).ok();
        }
        _ => {
            nexus_user::log("init: FAILED: the new program said nothing").ok();
            return;
        }
    }

    if nexus_user::send(child, b"init: heard you", &[]).is_err() {
        nexus_user::log("init: FAILED: could not answer the new program").ok();
        return;
    }

    nexus_user::log("init: asked for a program, got a channel, and used it").ok();
}

/// Nothing catches a panic here, so it is reported and the thread stops.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("init: PANIC").ok();
    nexus_user::exit()
}
