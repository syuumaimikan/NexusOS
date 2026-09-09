//! `idle`: a program that waits for something that never comes.
//!
//! It exists to be stopped. Every other program here ends because it has
//! finished, which says nothing about whether a program can be *made* to end --
//! and a program that finishes on its own would end at about the right moment
//! whether or not the kill worked.
//!
//! It waits in one of two ways, because there are two ways a program can be
//! impossible to stop and only one of them is easy.
//!
//! Told nothing, it blocks on its channel. Nothing will ever send to it, so the
//! only way it ends is if someone ends it -- and ending it means reaching a
//! thread that is off every run queue and waking it before it can notice.
//!
//! Told `spin`, it loops on a number and makes no system calls at all. That one
//! cannot be stopped at the system-call boundary because it never reaches one;
//! the only thing that ever interrupts it is the timer, and being stopped there
//! is the claim worth testing. A kernel that only checked at the boundary would
//! run this loop until the machine was turned off.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

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
    nexus_user::log("idle: waiting for something that will never arrive").ok();

    // The blocking case. This thread is off every run queue; something has to
    // reach in and wake it before it can notice it has been asked to leave.
    let mut buffer = [0u8; 16];
    let mut handles = [Handle(0); 1];
    let received = match nexus_user::receive(PARENT, &mut buffer, &mut handles) {
        Ok(received) => received,
        // The wait ended without a message, which here means the kill -- and
        // the log line below never appears, because the system-call boundary
        // stops this thread on the way out of `receive`.
        Err(_) => {
            nexus_user::log("idle: FAILED: the wait ended but the program did not").ok();
            nexus_user::exit_with(1)
        }
    };

    if &buffer[..received.bytes] == b"spin" {
        nexus_user::log("idle: spinning, and asking the kernel for nothing at all").ok();
        spin();
    }

    nexus_user::log("idle: FAILED: something ended the wait but not the program").ok();
    nexus_user::exit_with(1)
}

/// Loop forever without touching the kernel.
///
/// No system calls, no waiting, nothing to block on: from the kernel's side
/// this thread is simply always runnable. `write_volatile` is what keeps the
/// loop from being optimised into nothing, which would turn this into a test of
/// the compiler rather than of the kernel.
fn spin() -> ! {
    let mut counter: u64 = 0;
    loop {
        // SAFETY: a local, written through a pointer to itself.
        unsafe {
            core::ptr::write_volatile(&raw mut counter, counter.wrapping_add(1));
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("idle: PANIC").ok();
    nexus_user::exit_with(2)
}
