//! What `cat`, `head` and `grep` all have to do.
//!
//! Each of the three is a few lines of its own on top of this: read the
//! arguments, read the input a line at a time, decide what to write. The
//! deciding is the program; everything else is here.
//!
//! # What every one of them is given
//!
//! | 1 | the channel back to whoever started it; the arguments are its first message |
//! | 2 | standard output |
//! | 3 | standard input |
//! | 4 | the directory it may read, when it reads one at all |
//!
//! `head` and `grep` never open a file. They are handed a directory they do not
//! use, because the shell lends the same three things to everything it starts
//! and a program that refused what it was given would be harder to start than
//! one that ignores it.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use nexus_user::Handle;

/// The directory the shell lends, for the one of these that opens a file.
pub const DIRECTORY: Handle = Handle(4);

/// The most one of these will hold of a file.
///
/// A megabyte. Larger than anything on this machine and small enough that a
/// program which is handed something enormous says so rather than asking for
/// memory it will not get.
pub const MAX_FILE: usize = 1024 * 1024;

/// Read the arguments, which arrive as the first message on the parent channel.
///
/// Empty when there are none, which is not an error: a program whose arguments
/// are optional still reads them, because the kernel sends the message either
/// way and leaving it there would mean the next read finds it.
#[must_use]
pub fn arguments() -> String {
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    match nexus_user::receive(nexus_user::PARENT, &mut buffer, &mut none) {
        Ok(received) => String::from_utf8_lossy(&buffer[..received.bytes])
            .trim()
            .into(),
        Err(_) => String::new(),
    }
}

/// Write one line to standard output, or to the log when there is none.
///
/// Both, rather than one: a program run from the terminal has somewhere to
/// write, and the same program started by something else at boot has not.
pub fn say(who: &str, line: &str) {
    if nexus_user::println(line).is_err() {
        nexus_user::log(&alloc::format!("{who}: {line}")).ok();
    }
}

/// Read standard input to its end, a line at a time, giving each to `each`.
///
/// Returns whether there was an input at all. `false` means this program was
/// started with nothing connected -- which is what running `head` on its own
/// does, and is a different thing from an input that was empty.
///
/// # Why lines and not messages
///
/// Because a message boundary is not a line boundary and nothing promises it
/// will be. The program on the other end writes when it has something to say;
/// where that lands in the stream is its business. A `grep` that searched
/// messages would match or miss depending on how the sender happened to buffer,
/// which is the kind of bug that is never reproducible.
pub fn for_each_line(mut each: impl FnMut(&str) -> bool) -> bool {
    let mut held = String::new();
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut anything = false;

    loop {
        match nexus_user::read_input(&mut buffer) {
            Ok(None) => break,
            Ok(Some(got)) => {
                anything = true;
                held.push_str(&alloc::string::String::from_utf8_lossy(&buffer[..got]));
                while let Some(at) = held.find('\n') {
                    let line: String = held.drain(..=at).collect();
                    if !each(line.trim_end_matches('\n')) {
                        return true;
                    }
                }
            }
            Err(_) => return false,
        }
    }

    // A last line with no ending on it is still a line.
    if !held.is_empty() {
        each(&held);
    }
    anything
}

/// Read a whole file out of the lent directory.
///
/// # Errors
///
/// The name as the system reported it, for saying which file and why.
pub fn read_file(name: &str) -> Result<Vec<u8>, nexus_user::Error> {
    let file = nexus_user::open(DIRECTORY, name)?;
    let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    Ok(bytes)
}
