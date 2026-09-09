//! `find`: reads a directory, indexes what is in it, and answers a question.
//!
//! The smallest thing that is honestly an *agent*: it is given a job and a
//! place to do it in, it decides for itself which files to read, and it comes
//! back with an answer nobody told it.
//!
//! # What it is allowed to do
//!
//! Exactly what its handle says. It is handed one directory, with read and
//! transfer rights and *not* write — so it can open and read every file it
//! finds, and a write is refused by the kernel rather than by this program's
//! good behaviour. That is the whole of the non-negotiable rule this system
//! sets for anything that acts on its own: explicit capabilities, never ambient
//! authority.
//!
//! It proves it, too. Before it does anything useful it tries to create a file
//! in the directory it was given, and reports being refused. A sandbox nobody
//! has pushed against is a sandbox nobody has tested.
//!
//! # What "understands" means here
//!
//! Nothing is trained and there is no model. [`nexus_index`] hashes character
//! n-grams into buckets and compares documents by the angle between their count
//! vectors; a query and a file that share phrases rank highly. That finds a file
//! again, which is what this does. It is worth being plain about because the
//! word for it invites the reader to assume a great deal more.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::panic::PanicInfo;

use nexus_index::Index;
use nexus_user::{Handle, Kind};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

/// How much heap: every file it indexes is read whole.
const HEAP: usize = 512 * 1024;

/// The most bytes it will read out of one file.
///
/// An index of the first few kilobytes of a file is an index of what the file
/// is about, and reading a megabyte to find that out is a program that stops
/// the machine to answer a question.
const MAX_FILE: usize = 4096;

/// The most files it will look at.
const MAX_FILES: usize = 64;

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
        failed("find: FAILED: could not get a heap");
        finish();
    }

    // One message: what to look for, and the one place it may look.
    let mut buffer = [0u8; 256];
    let mut handles = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(PARENT, &mut buffer, &mut handles) else {
        failed("find: FAILED: nothing arrived to look for");
        finish();
    };
    if received.handles != 1 {
        failed("find: FAILED: no directory came with the question");
        finish();
    }
    let directory = handles[0];
    let Ok(query) = core::str::from_utf8(&buffer[..received.bytes]) else {
        failed("find: FAILED: the question is not text");
        finish();
    };

    // What it may do, before it does anything. The rights are on the handle and
    // the kernel enforces them; asking is how this program finds out what it
    // was trusted with rather than assuming.
    let rights = nexus_user::rights(directory).unwrap_or(0);
    nexus_user::log(&format!(
        "find: given one directory with {}{}{}{}",
        if rights & nexus_user::rights::READ != 0 {
            "read "
        } else {
            ""
        },
        if rights & nexus_user::rights::WRITE != 0 {
            "write "
        } else {
            ""
        },
        if rights & nexus_user::rights::CLOSE != 0 {
            "close "
        } else {
            ""
        },
        if rights & nexus_user::rights::TRANSFER != 0 {
            "transfer"
        } else {
            ""
        },
    ))
    .ok();

    // And the proof. A program that merely *does not* write is not a program
    // that *cannot*; the difference is the whole point of a capability, and the
    // only way to see it is to try.
    match nexus_user::create(directory, "agent-was-here.txt", Kind::File) {
        Err(nexus_user::Error::Denied) => {
            nexus_user::log("find: tried to write where it was reading, and was refused").ok();
        }
        Err(other) => {
            failed(&format!(
                "find: FAILED: the write failed for the wrong reason: {other:?}"
            ));
            finish();
        }
        Ok(_) => {
            // Worse than a bug: the sandbox is not one.
            failed("find: FAILED: it could write to a directory it was given for reading");
            finish();
        }
    }

    match search(directory, query) {
        Some((name, dot, norm)) => {
            nexus_user::log(&format!(
                "find: \"{query}\" is closest to {name} (overlap {dot}, size {norm})"
            ))
            .ok();
        }
        None => {
            failed("find: FAILED: nothing in the directory matched at all");
        }
    }
    finish()
}

/// Read everything in the directory and find what best answers the question.
fn search(directory: Handle, query: &str) -> Option<(String, u64, u64)> {
    let mut listing = [0u8; 2048];
    let length = nexus_user::list(directory, &mut listing).ok()?;

    let mut index: Index<String> = Index::new();
    let mut looked = 0usize;
    for entry in nexus_user::entries(&listing[..length]) {
        if looked >= MAX_FILES {
            break;
        }
        if entry.kind != Kind::File {
            continue;
        }
        let Some(text) = read(directory, entry.name) else {
            continue;
        };
        looked += 1;
        index.add(String::from(entry.name), &text);
    }

    nexus_user::log(&format!("find: indexed {looked} files it was able to read")).ok();
    let (name, dot, norm) = index.best(query)?;
    Some((name.clone(), dot, norm))
}

/// Read the beginning of a file, as text.
///
/// Anything that is not valid UTF-8 is not indexed rather than being forced:
/// a program image hashed as though it were prose would put an executable at
/// the top of a search for a sentence.
fn read(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
    let mut bytes = vec![0u8; size];
    let mut read = 0;
    while read < size {
        match nexus_user::read_at(file, read as u64, &mut bytes[read..]) {
            Ok(0) | Err(_) => break,
            Ok(count) => read += count,
        }
    }
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
}

/// Whether anything has gone wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit()
    }
}

/// Log a failure and remember it.
fn failed(what: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(what).ok();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("find: PANIC").ok();
    nexus_user::exit_with(2)
}
