//! `idle`: a program that waits for something that never comes.
//!
//! It exists to be stopped. Every other program here ends because it has
//! finished, which says nothing about whether a program can be *made* to end --
//! and a program that finishes on its own would end at about the right moment
//! whether or not the kill worked.
//!
//! So this one blocks on its channel and never leaves. Nothing will ever send
//! to it: whoever started it holds the other end and says nothing. The only way
//! this process ends is if someone ends it.

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

    // Blocking, not spinning. A spin would be stopped too -- the check happens
    // at the system-call boundary and this would reach one eventually -- but
    // blocking is the harder case and the one worth showing: this thread is off
    // every run queue, and something has to reach in and wake it before it can
    // notice it has been asked to leave.
    let mut buffer = [0u8; 16];
    let mut handles = [Handle(0); 1];
    let _ = nexus_user::receive(PARENT, &mut buffer, &mut handles);

    // Reached only when the wait was ended by something other than a message,
    // which here means the kill. The exit below never runs: the system-call
    // boundary stops this thread on the way out of `receive`.
    nexus_user::log("idle: FAILED: something ended the wait but not the program").ok();
    nexus_user::exit_with(1)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("idle: PANIC").ok();
    nexus_user::exit_with(2)
}
