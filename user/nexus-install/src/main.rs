//! `install`: puts a package on the filesystem, or puts nothing on it.
//!
//! An ordinary program. It has no privilege of any kind: it cannot reach the
//! filesystem at all until somebody *hands* it a directory, and what it can do
//! is bounded by the rights that handle carries. A package manager that had to
//! run as the system in order to install a file would be a package manager that
//! can install a file anywhere.
//!
//! What installing actually means -- verify, write, verify again, and roll back
//! if any of it fails -- lives in this crate's library, because `update` does
//! the same thing for a different reason. This is the program around it: one
//! message in, one package, one line of report.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::panic::PanicInfo;

use nexus_install::{install, Trouble};
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

/// How much heap: a package, its files, and the copies kept for a rollback.
const HEAP: usize = 512 * 1024;

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
        failed("install: FAILED: could not get a heap");
        finish();
    }

    // One message: where the package is, and a directory to work in. The
    // directory is the authority -- without it this program cannot open a file,
    // and with it it can do exactly what that handle allows and nothing
    // elsewhere.
    let mut buffer = [0u8; 128];
    let mut handles = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(PARENT, &mut buffer, &mut handles) else {
        failed("install: FAILED: nothing arrived to install");
        finish();
    };
    if received.handles != 1 {
        failed("install: FAILED: no directory came with the message");
        finish();
    }
    let root = handles[0];
    let Ok(path) = core::str::from_utf8(&buffer[..received.bytes]) else {
        failed("install: FAILED: the path is not text");
        finish();
    };

    match install(root, path) {
        Ok(report) => {
            nexus_user::log(&report).ok();
            nexus_user::log("install: the package is on the filesystem").ok();
        }
        // Refusing a package is this program working, not this program
        // failing, and it says so in different words. A log line that read the
        // same either way would make a machine that correctly rejected a forged
        // package indistinguishable from one that had broken.
        Err(Trouble::Refused(why)) => {
            REFUSED.store(true, core::sync::atomic::Ordering::Relaxed);
            nexus_user::log(&format!("install: refused {path}: {why}")).ok();
        }
        Err(Trouble::Broken(why)) => {
            failed(&format!("install: FAILED: {why}"));
        }
    }
    finish()
}

/// Whether something broke, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Whether a package was refused, which is a different answer.
static REFUSED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
///
/// Three answers rather than two: it worked, it refused, or it broke. Whoever
/// asked for the install has to be able to tell the last two apart -- a
/// refusal is an answer about the package and a fault is an answer about the
/// machine.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(2)
    } else if REFUSED.load(core::sync::atomic::Ordering::Relaxed) {
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
    nexus_user::log("install: PANIC").ok();
    nexus_user::exit_with(2)
}
