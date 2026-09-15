//! The interface a NexusOS program uses to reach the rest of the system.
//!
//! `nexus-user` is the *system call* layer: one function per call the kernel
//! offers, each a thin wrapper over the instruction. This is the layer above
//! it — the operations a program actually performs, which are almost never one
//! call.
//!
//! Starting a program is the clearest example. It is a path and a zero byte and
//! the arguments in one message, two handles attached in a fixed order, a reply
//! of exactly two handles, a wait, and an ending to interpret. That is about a
//! hundred lines, and it was written five times: in the terminal, the
//! compositor, the launcher, the assistant and `init`. Five copies of a wire
//! format is five places for it to drift.
//!
//! # What this is not
//!
//! Not a new kind of authority. Every function here is built out of handles the
//! caller was already lent, and a program that was lent nothing can do nothing
//! with any of it. [`Spawn`] needs a spawn service handle; [`path`] needs a
//! directory handle; [`machine`] needs the machine service's. None of them can
//! be conjured, and there is no call here that reaches something the caller
//! could not already have reached by hand.
//!
//! That is the difference between this and the interface it is named after. The
//! Windows API is the boundary of the system: `CreateProcess` is a thing any
//! process may call because it is a process. Here, starting a program is
//! something you can do because somebody handed you the means, and this library
//! is a convenience over the handing rather than a way around it.
//!
//! # What is here
//!
//! | [`process`] | starting a program, feeding it, waiting for it, killing it |
//! | [`path`] | walking a path of several components from a directory handle |
//! | [`machine`] | what the machine is doing, from the kernel's own snapshot |
//!
//! Text in and out is [`nexus_user::print`], [`nexus_user::println`] and
//! [`nexus_user::read_input`], which are already the right shape and are
//! re-exported here so that a program has one thing to import.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub use nexus_user::{print, println, read_input, Error, Handle, Kind};

pub mod machine;
pub mod path;
pub mod process;

/// The handles every program is started with, by number.
///
/// Handle one is put there by the kernel's spawn service before anything else,
/// so it means the same thing in every program on this machine. Two and three
/// are whatever the caller attached first and second, which by convention are
/// somewhere to write and somewhere to read; four onwards is between the two
/// programs and is written down by whichever of them documents it.
pub mod given {
    use nexus_user::Handle;

    /// The channel back to whoever started this program. Its arguments are the
    /// first message on it.
    pub const PARENT: Handle = nexus_user::PARENT;
    /// Standard output, when one was attached.
    pub const OUTPUT: Handle = nexus_user::OUTPUT;
    /// Standard input, when one was attached.
    pub const INPUT: Handle = nexus_user::INPUT;
    /// The first thing beyond those three: what this program was lent to work
    /// on, by arrangement with whoever started it.
    pub const LENT: Handle = Handle(4);
}

/// This program's arguments, as the first message on the parent channel.
///
/// Read once and early. The kernel sends this message before handing the
/// channel over, so it is waiting when the program makes its first read — and
/// leaving it there means whatever reads that channel next finds the arguments
/// instead of what it expected.
///
/// Empty when there are none, which is not an error.
#[must_use]
pub fn arguments() -> alloc::string::String {
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    match nexus_user::receive(given::PARENT, &mut buffer, &mut none) {
        Ok(received) => alloc::string::String::from_utf8_lossy(&buffer[..received.bytes])
            .trim()
            .into(),
        Err(_) => alloc::string::String::new(),
    }
}
