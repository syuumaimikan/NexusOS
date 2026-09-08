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
    use_the_filesystem();
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

    share_memory_with(child);
}

/// Where this program maps memory it shares.
///
/// Well above its own image and stack, and a different address from the one the
/// other program will choose — which is the point of mapping being separate
/// from creating. Two processes sharing memory do not have to agree on where.
const SHARED_AT: usize = 0x0000_0000_0200_0000;

/// What this program writes into the shared page, and what it expects back.
const OURS: u64 = 0x1111_1111_1111_1111;
const THEIRS: u64 = 0x2222_2222_2222_2222;

/// Share a page with the program on the other end of `child`.
///
/// A channel copies its message twice, which is right for a request and wrong
/// for anything large. This is the other arrangement: one page of memory, two
/// processes, no copy — and the handle is what crosses the channel rather than
/// the contents.
fn share_memory_with(child: nexus_user::Handle) {
    let Ok(memory) = nexus_user::memory_create(4096) else {
        nexus_user::log("init: FAILED: could not create shared memory").ok();
        return;
    };
    if nexus_user::memory_map(memory, SHARED_AT, true) != Ok(4096) {
        nexus_user::log("init: FAILED: could not map shared memory").ok();
        return;
    }

    // SAFETY: the kernel mapped a page here, writable, for this process.
    unsafe {
        core::ptr::write_volatile(SHARED_AT as *mut u64, OURS);
    }

    // The handle crosses, not the page. The other process maps the same frames
    // wherever it likes.
    if nexus_user::send(child, b"shared memory", &[memory]).is_err() {
        nexus_user::log("init: FAILED: could not pass the memory handle").ok();
        return;
    }

    // Wait for it to say it has written its own value in.
    let mut buffer = [0u8; 64];
    let mut none = [nexus_user::Handle(0); 1];
    if nexus_user::receive(child, &mut buffer, &mut none).is_err() {
        nexus_user::log("init: FAILED: no answer about the shared page").ok();
        return;
    }

    // SAFETY: still mapped; the other process wrote through its own mapping of
    // the same frames.
    let seen = unsafe { core::ptr::read_volatile(SHARED_AT as *const u64) };
    if seen == THEIRS {
        nexus_user::log("init: the other process wrote into memory we both map").ok();
    } else if seen == OURS {
        nexus_user::log("init: FAILED: the shared page still holds only our own value").ok();
    } else {
        nexus_user::log("init: FAILED: the shared page holds something neither wrote").ok();
    }
}

/// The root of the system's filesystem, as the kernel hands it over: the second
/// entry, after the spawner.
///
/// A directory handle is the whole of what this program can reach. There is no
/// call that takes a path, so a filesystem it was not handed is one it cannot
/// name -- the same argument as for the spawner, applied to files.
const ROOT: nexus_user::Handle = nexus_user::Handle(2);

/// The directory this program keeps its own things in.
const OUR_DIRECTORY: &str = "init";
/// What it writes there, and reads back on the next boot.
const NOTE: &str = "started.txt";

/// Use the filesystem, from ring 3, through a handle.
///
/// Reads what a previous boot left, writes something for the next one, and
/// checks that the operations that must be refused are refused. Every one of
/// these goes through a real disk: there is no cache between this and the
/// platter, so a file that reads back is a file that was written.
fn use_the_filesystem() {
    let mut buffer = [0u8; 512];

    // The root, as it stands. A listing before anything is created, so what
    // this program adds is visibly its own.
    let Ok(length) = nexus_user::list(ROOT, &mut buffer) else {
        nexus_user::log("init: FAILED: could not read the root directory").ok();
        return;
    };
    let mut found = 0;
    for entry in nexus_user::entries(&buffer[..length]) {
        found += 1;
        let _ = entry;
    }
    if found == 0 {
        nexus_user::log("init: FAILED: the root directory is empty").ok();
        return;
    }

    // Its own directory, made once and found thereafter. Both outcomes are
    // ordinary, and telling them apart is the point of having two error codes
    // rather than one.
    let ours = match nexus_user::create(ROOT, OUR_DIRECTORY, nexus_user::Kind::Directory) {
        Ok(handle) => handle,
        Err(nexus_user::Error::Exists) => match nexus_user::open(ROOT, OUR_DIRECTORY) {
            Ok(handle) => handle,
            Err(_) => {
                nexus_user::log("init: FAILED: its directory exists and will not open").ok();
                return;
            }
        },
        Err(_) => {
            nexus_user::log("init: FAILED: could not make a directory").ok();
            return;
        }
    };

    // What the last boot left, if there was one.
    let previous = match nexus_user::open(ours, NOTE) {
        Ok(note) => {
            let mut text = [0u8; 256];
            let length = nexus_user::read(note, &mut text).unwrap_or(0);
            let kept = core::str::from_utf8(&text[..length]).unwrap_or("");
            let count = kept.len();
            nexus_user::close(note).ok();
            count
        }
        Err(nexus_user::Error::NotFound) => 0,
        Err(_) => {
            nexus_user::log("init: FAILED: its own note would not open").ok();
            return;
        }
    };

    // And something for the next one. Written whole, because that is what the
    // filesystem underneath offers.
    let note = match nexus_user::open(ours, NOTE) {
        Ok(handle) => handle,
        Err(nexus_user::Error::NotFound) => {
            match nexus_user::create(ours, NOTE, nexus_user::Kind::File) {
                Ok(handle) => handle,
                Err(_) => {
                    nexus_user::log("init: FAILED: could not make a file").ok();
                    return;
                }
            }
        }
        Err(_) => {
            nexus_user::log("init: FAILED: could not open its own file").ok();
            return;
        }
    };

    const WRITTEN: &[u8] = b"init was here, and wrote this from ring 3\n";
    if nexus_user::write(note, WRITTEN) != Ok(WRITTEN.len()) {
        nexus_user::log("init: FAILED: could not write to its own file").ok();
        return;
    }

    // Read back through the same handle, off the same disk.
    let mut back = [0u8; 128];
    match nexus_user::read(note, &mut back) {
        Ok(length) if length == WRITTEN.len() && &back[..length] == WRITTEN => {}
        _ => {
            nexus_user::log("init: FAILED: what was written did not read back").ok();
            return;
        }
    }
    if nexus_user::size(note) != Ok(WRITTEN.len()) {
        nexus_user::log("init: FAILED: the file does not know its own length").ok();
        return;
    }

    // A buffer too small has to be an error rather than a prefix. Half a file
    // that reports its own length looks exactly like a whole one.
    let mut tiny = [0u8; 4];
    if nexus_user::read(note, &mut tiny) != Err(nexus_user::Error::TooBig) {
        nexus_user::log("init: FAILED: a short buffer was filled with a fragment").ok();
        return;
    }

    // A name with a separator in it must be refused, not walked. If it were
    // walked, a directory handle would stop meaning "this subtree".
    if nexus_user::open(ROOT, "../system").is_ok() {
        nexus_user::log("init: FAILED: a path escaped the directory it started in").ok();
        return;
    }
    // And a name that is not there.
    if nexus_user::open(ours, "nosuch") != Err(nexus_user::Error::NotFound) {
        nexus_user::log("init: FAILED: a name that is not there was opened").ok();
        return;
    }
    // And removing something that is still open.
    if nexus_user::remove(ours, NOTE) != Err(nexus_user::Error::Filesystem) {
        nexus_user::log("init: FAILED: a file was removed while it was still open").ok();
        return;
    }

    // The directory now has the file in it, and says so.
    let Ok(length) = nexus_user::list(ours, &mut buffer) else {
        nexus_user::log("init: FAILED: could not read its own directory").ok();
        return;
    };
    let mut names = 0;
    let mut correct = false;
    for entry in nexus_user::entries(&buffer[..length]) {
        names += 1;
        if entry.name == NOTE && entry.kind == nexus_user::Kind::File {
            correct = true;
        }
    }
    if names != 1 || !correct {
        nexus_user::log("init: FAILED: its directory does not hold what it wrote").ok();
        return;
    }

    nexus_user::close(note).ok();
    nexus_user::close(ours).ok();

    if previous == 0 {
        nexus_user::log("init: made a directory and a file in the system's filesystem").ok();
    } else {
        nexus_user::log("init: read what a previous boot wrote, and wrote again").ok();
    }
}

/// Nothing catches a panic here, so it is reported and the thread stops.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("init: PANIC").ok();
    nexus_user::exit()
}
