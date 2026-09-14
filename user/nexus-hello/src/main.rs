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
        failed("hello: FAILED: could not reach whoever started me");
        finish();
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
            failed("hello: FAILED: no answer came back");
            finish();
        }
    }

    share_memory();
    finish()
}

/// Where this program maps memory someone shares with it.
///
/// A different address from the one the other process chose, on purpose: two
/// processes sharing memory do not have to agree on where it goes, and a test
/// that used the same address on both sides would not show that.
const SHARED_AT: usize = 0x0000_0000_0300_0000;

/// What the other process writes, and what this one writes back.
const THEIRS: u64 = 0x1111_1111_1111_1111;
const OURS: u64 = 0x2222_2222_2222_2222;

/// Take a memory handle and write into the page it names.
fn share_memory() {
    let mut buffer = [0u8; 64];
    let mut handles = [Handle(0); 1];

    let received = match nexus_user::receive(PARENT, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("hello: FAILED: nothing more came");
            return;
        }
    };
    if received.handles != 1 {
        failed("hello: FAILED: no memory handle came with it");
        return;
    }

    let memory = handles[0];
    let Ok(size) = nexus_user::memory_size(memory) else {
        failed("hello: FAILED: could not ask how large the memory is");
        return;
    };
    if nexus_user::memory_map(memory, SHARED_AT, true) != Ok(size) {
        failed("hello: FAILED: could not map the shared memory");
        return;
    }

    // SAFETY: the kernel mapped the page here, writable, for this process.
    let seen = unsafe { core::ptr::read_volatile(SHARED_AT as *const u64) };
    if seen != THEIRS {
        failed("hello: FAILED: the shared page does not hold what was written");
        return;
    }

    // SAFETY: as above. The other process sees this through its own mapping of
    // the same frames, with nothing copied either way.
    unsafe {
        core::ptr::write_volatile(SHARED_AT as *mut u64, OURS);
    }

    nexus_user::log("hello: read the shared page and wrote back into it").ok();
    nexus_user::send(PARENT, b"written", &[]).ok();
}

/// Whether anything has gone wrong, for the status this program exits with.
///
/// A program that logged a failure and then exited saying it worked would be a
/// program whose parent has no way to find out. Every `FAILED` path below sets
/// this, and `finish` turns it into the number the waiter reads.
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
    nexus_user::log("hello: PANIC").ok();
    // Straight to the status: a panicking program has no state left worth
    // consulting, and its waiter is owed a number that says so.
    nexus_user::exit_with(2)
}
