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

extern crate alloc;

use alloc::vec::Vec;
use core::panic::PanicInfo;

/// Where this program's allocations come from.
///
/// A memory object like any other, taken from the kernel and handed to the same
/// allocator the kernel uses. Nothing about it is special except what the
/// allocator does with it.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

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

    // Before anything allocates. A program that allocated first would get a
    // null pointer and Rust's panic for one, which says less than this does.
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("init: FAILED: could not get a heap");
        finish();
    }
    use_the_heap();

    // The uptime, twice, with a yield between. Two different answers say the
    // clock is running and that this process is one among others rather than
    // the only thing on the machine.
    let first = nexus_user::uptime();
    for _ in 0..64 {
        nexus_user::yield_now();
    }
    let second = nexus_user::uptime();

    if second < first {
        failed("init: FAILED: the clock went backwards");
        finish();
    }

    // A channel to itself, which is the whole of the IPC interface exercised
    // from a program that was not compiled into the kernel.
    let Ok((writer, reader)) = nexus_user::channel() else {
        failed("init: FAILED: could not create a channel");
        finish();
    };

    const GREETING: &[u8] = b"a message from a program on disk";
    if nexus_user::send(writer, GREETING, &[]) != Ok(GREETING.len()) {
        failed("init: FAILED: could not send on its own channel");
        finish();
    }

    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 1];
    match nexus_user::receive(reader, &mut buffer, &mut handles) {
        Ok(received) if received.bytes == GREETING.len() && received.handles == 0 => {
            if &buffer[..received.bytes] != GREETING {
                failed("init: FAILED: the message came back changed");
                finish();
            }
        }
        _ => {
            failed("init: FAILED: could not read its own message");
            finish();
        }
    }

    // A handle it was never given has to be refused rather than answered.
    if nexus_user::rights(nexus_user::Handle(9999)) != Err(nexus_user::Error::BadHandle) {
        failed("init: FAILED: a handle it never had was accepted");
        finish();
    }

    nexus_user::log("init: clock, channel and handle checks all passed").ok();

    ask_for_a_program();
    use_the_filesystem();

    // And a package, installed by a program that had to be handed a directory
    // before it could touch anything at all.
    install_a_package();
    finish()
}

/// Allocate, grow, free, and check the accounting.
///
/// A program could not do this at all until now: it was a fixed set of arrays,
/// which is enough to draw a gradient and not enough to do anything with what
/// it is told. Everything above this in the system -- a toolkit that lays out
/// text, a program that reads a directory -- needs somewhere to put the answer.
fn use_the_heap() {
    let (used_before, total) = nexus_user::heap::used();
    if total == 0 {
        failed("init: FAILED: the heap reports no memory");
        return;
    }

    // Grown well past its first allocation, so the block is moved at least once
    // and the old one handed back. A heap that never freed would pass a test
    // that only ever allocated.
    let mut numbers: Vec<u64> = Vec::new();
    for index in 0..4096u64 {
        numbers.push(index * 3);
    }
    if numbers.len() != 4096 || numbers[4095] != 4095 * 3 {
        failed("init: FAILED: what went into the heap did not come back");
        return;
    }

    let (used_during, _) = nexus_user::heap::used();
    if used_during <= used_before {
        failed("init: FAILED: allocating did not use any of the heap");
        return;
    }

    drop(numbers);
    let (used_after, _) = nexus_user::heap::used();
    if used_after != used_before {
        failed("init: FAILED: the heap did not take the memory back");
        return;
    }
    if nexus_user::heap::refused() != 0 {
        failed("init: FAILED: the heap refused an allocation it should have met");
        return;
    }

    nexus_user::log("init: allocated, grew, and gave it all back").ok();
}

/// The channel to the spawn service, as the kernel hands it over: the first
/// entry in an otherwise empty table.
const SPAWNER: nexus_user::Handle = nexus_user::Handle(1);

/// Ask for another program to be started, and then talk to it.
///
/// There is no system call that creates a process. There is a channel, and
/// holding one end of it is the authority to ask; a program that was never
/// given this handle cannot ask, and there is no name it could use instead.
/// What comes back is two handles: a channel to the thing that started, so the
/// answer is an introduction rather than a notification, and the process
/// itself, which is what makes "and tell me when it is done" something this
/// program can ask rather than something it has to guess at.
fn ask_for_a_program() {
    const PROGRAM: &[u8] = b"BIN/HELLO.ELF";

    if nexus_user::send(SPAWNER, PROGRAM, &[]).is_err() {
        failed("init: FAILED: could not reach the spawn service");
        return;
    }

    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 2];
    let received = match nexus_user::receive(SPAWNER, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("init: FAILED: the spawn service did not answer");
            return;
        }
    };

    if received.handles != 2 {
        // The reply text says why, and is worth showing: a refusal is as
        // interesting as a success and reads the same way in a boot log.
        let text = core::str::from_utf8(&buffer[..received.bytes]).unwrap_or("<not text>");
        nexus_user::log(text).ok();
        failed("init: FAILED: the spawn service did not send both handles");
        return;
    }

    // In the order the service sends them: talk to it, then wait for it.
    let child = handles[0];
    let process = handles[1];

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
            failed("init: FAILED: the new program said nothing");
            return;
        }
    }

    if nexus_user::send(child, b"init: heard you", &[]).is_err() {
        failed("init: FAILED: could not answer the new program");
        return;
    }

    nexus_user::log("init: asked for a program, got a channel, and used it").ok();

    share_memory_with(child);
    wait_on_both(child, process);
    wait_for(process);
    stop_a_program();
}

/// Start a program that will never finish, and stop it.
///
/// Every other program here ends because it has finished, which says nothing
/// about whether one can be *made* to end -- a program that was going to exit
/// anyway would exit at about the right moment whether or not the kill worked.
/// So this one starts something that blocks on a channel nobody will ever send
/// to, stops it, and waits to see what it did.
///
/// The handle is the authority. There is no call that stops a process by
/// number, and this program can only stop what it was handed.
fn stop_a_program() {
    const PROGRAM: &[u8] = b"BIN/IDLE.ELF";

    if nexus_user::send(SPAWNER, PROGRAM, &[]).is_err() {
        failed("init: FAILED: could not ask for a program to stop");
        return;
    }
    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 2];
    let Ok(received) = nexus_user::receive(SPAWNER, &mut buffer, &mut handles) else {
        failed("init: FAILED: the spawn service did not answer the second time");
        return;
    };
    if received.handles != 2 {
        failed("init: FAILED: no program came back to stop");
        return;
    }
    let channel = handles[0];
    let process = handles[1];

    // A moment, so it reaches its wait. Stopping it before it blocks would
    // still work -- the check at the system-call boundary catches it on the way
    // in -- but then this would be testing the easy half.
    nexus_user::sleep(200).ok();

    if nexus_user::kill(process).is_err() {
        failed("init: FAILED: could not stop the program");
        return;
    }

    match nexus_user::wait(process) {
        Ok(nexus_user::Ending::Stopped) => {
            nexus_user::log("init: stopped a program that was waiting forever").ok();
        }
        Ok(nexus_user::Ending::Exited(_)) => {
            // It got out of its wait and exited on its own, which means the
            // kill did not do it and something else did.
            failed("init: FAILED: the program it stopped exited by itself instead");
        }
        Err(_) => failed("init: FAILED: could not wait for the program it stopped"),
    }

    nexus_user::close(channel).ok();
    nexus_user::close(process).ok();

    stop_a_spinning_program();
}

/// And the harder one: a program that asks the kernel for nothing.
///
/// The one above blocks, so stopping it is a matter of waking it. This one
/// loops on a number and makes no system calls at all, so it never reaches the
/// boundary where a thread is asked to leave. The only thing that interrupts it
/// is the timer, and being stopped there is the whole claim: without it a
/// kernel can be asked to stop a program and simply fail to, forever.
fn stop_a_spinning_program() {
    // The path, a zero byte, and what the program should be told. Arguments are
    // not a separate mechanism: they are the first message on the channel the
    // program is given, sent by the kernel on this program's behalf -- because
    // this program does not have that channel until the reply comes back, and a
    // program that had to wait for its arguments until after it had started
    // would have started without them.
    const PROGRAM: &[u8] = b"BIN/IDLE.ELF spin";

    if nexus_user::send(SPAWNER, PROGRAM, &[]).is_err() {
        failed("init: FAILED: could not ask for a program to spin");
        return;
    }
    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 2];
    let Ok(received) = nexus_user::receive(SPAWNER, &mut buffer, &mut handles) else {
        failed("init: FAILED: the spawn service did not answer the third time");
        return;
    };
    if received.handles != 2 {
        failed("init: FAILED: no program came back to spin");
        return;
    }
    let channel = handles[0];
    let process = handles[1];

    // Long enough that it is certainly in its loop. Stopping it before it got
    // there would be testing the boundary again rather than the timer.
    nexus_user::sleep(300).ok();

    if nexus_user::kill(process).is_err() {
        failed("init: FAILED: could not stop the spinning program");
        return;
    }

    match nexus_user::wait(process) {
        Ok(nexus_user::Ending::Stopped) => {
            nexus_user::log("init: stopped a program that was asking the kernel for nothing").ok();
        }
        Ok(nexus_user::Ending::Exited(_)) => {
            failed("init: FAILED: the spinning program exited by itself instead");
        }
        Err(_) => failed("init: FAILED: could not wait for the spinning program"),
    }

    nexus_user::close(channel).ok();
    nexus_user::close(process).ok();
}

/// The keys this program gives the two things it watches.
///
/// Its own names for them, not handle numbers. That is the point of a key: it
/// comes back unchanged, so a program recognises what became ready instead of
/// looking it up.
const KEY_CHILD: u64 = 100;
const KEY_PROCESS: u64 = 200;

/// Wait on a channel and a process at the same time.
///
/// This is the shape every server has and no program here could write until
/// now. A blocking `receive` on one client cannot be interrupted by a second
/// client having something to say, and a `wait` for one process cannot be
/// interrupted by a message. One wait over both is the difference between a
/// program that serves one thing and a program that serves.
///
/// A wait returns as soon as *anything* is ready, not when everything is, so
/// this loops the way a server loops: take what is ready, deal with it, stop
/// watching it, wait again. Dealing with it is what a real one would do with
/// the message; here it is enough to have been told, and stopping watching is
/// what keeps a level-triggered set from reporting the same thing forever.
///
/// The loop ends when the set is empty, which the kernel reports as zero keys
/// rather than by blocking on something that can never become ready.
fn wait_on_both(child: nexus_user::Handle, process: nexus_user::Handle) {
    let Ok(set) = nexus_user::wait_set() else {
        failed("init: FAILED: could not make a wait set");
        return;
    };
    if nexus_user::watch(set, child, KEY_CHILD).is_err()
        || nexus_user::watch(set, process, KEY_PROCESS).is_err()
    {
        failed("init: FAILED: could not watch a handle");
        return;
    }

    // The same key twice has to be refused: two members under one name would
    // make the answer ambiguous, which is worse than an error.
    if nexus_user::watch(set, process, KEY_CHILD) != Err(nexus_user::Error::Exists) {
        failed("init: FAILED: a wait set accepted the same key twice");
        return;
    }

    let mut saw_child = false;
    let mut saw_process = false;
    let mut waits = 0;

    // Bounded, because a loop that cannot end is the failure this is testing
    // for: a wait set that never reports the second member would otherwise
    // hang the boot rather than report anything.
    while waits < 8 {
        waits += 1;
        let mut keys = [0u64; 4];
        let Ok(count) = nexus_user::wait_any(set, &mut keys) else {
            failed("init: FAILED: could not wait on a set");
            return;
        };
        if count == 0 {
            // The set is empty: everything it watched has been dealt with.
            break;
        }

        for key in &keys[..count] {
            match *key {
                KEY_CHILD => saw_child = true,
                KEY_PROCESS => saw_process = true,
                _ => {
                    failed("init: FAILED: a wait set returned a key it was never given");
                    return;
                }
            }
            // Stop watching what has been dealt with. Readiness here is a
            // level and not an edge, so a member left in the set would be
            // reported again on the next wait, forever.
            if nexus_user::unwatch(set, *key).is_err() {
                failed("init: FAILED: could not stop watching a handle");
                return;
            }
        }
    }

    if !saw_child || !saw_process {
        failed("init: FAILED: a wait set did not report both of the things it watches");
        return;
    }

    // And a key that is no longer in the set has to be refused, so a program
    // that has stopped caring about a client cannot quietly go on being told
    // about it.
    if nexus_user::unwatch(set, KEY_CHILD) != Err(nexus_user::Error::NotFound) {
        failed("init: FAILED: removing a key twice was not refused");
        return;
    }

    nexus_user::close(set).ok();
    nexus_user::log("init: one wait covered a channel and a process, and reported both").ok();
}

/// Wait for the program that was started, and say how it went.
///
/// This is the last thing missing from "ask for a program": until now this
/// process could start one and talk to it, and had no way to learn that it had
/// finished or whether it had worked. The channel closing says the other end is
/// gone; it does not say what the program decided.
///
/// Blocking is the honest shape. The other program may already have exited --
/// it very likely has, since everything above this has been a round trip with
/// it -- and a wait that missed an ending that had already happened would be a
/// wait that never returned.
fn wait_for(process: nexus_user::Handle) {
    match nexus_user::wait(process) {
        Ok(nexus_user::Ending::Exited(0)) => {
            nexus_user::log("init: the program it asked for finished, and said it worked").ok();
        }
        // Reported rather than ignored. A non-zero status means the other
        // program decided something went wrong, and a parent that dropped that
        // would be the reason nobody ever found out.
        Ok(nexus_user::Ending::Exited(_)) => {
            failed("init: FAILED: the program it asked for reported a failure");
        }
        Ok(nexus_user::Ending::Stopped) => {
            failed("init: FAILED: the program it asked for was stopped by someone");
        }
        Err(_) => {
            failed("init: FAILED: could not wait for the program it asked for");
        }
    }

    // The handle outlives the process on purpose -- a completion is a name and
    // a number, not an address space -- so it is closed when there is nothing
    // more to ask it.
    nexus_user::close(process).ok();
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
        failed("init: FAILED: could not create shared memory");
        return;
    };
    if nexus_user::memory_map(memory, SHARED_AT, true) != Ok(4096) {
        failed("init: FAILED: could not map shared memory");
        return;
    }

    // SAFETY: the kernel mapped a page here, writable, for this process.
    unsafe {
        core::ptr::write_volatile(SHARED_AT as *mut u64, OURS);
    }

    // The handle crosses, not the page. The other process maps the same frames
    // wherever it likes.
    if nexus_user::send(child, b"shared memory", &[memory]).is_err() {
        failed("init: FAILED: could not pass the memory handle");
        return;
    }

    // Wait for it to say it has written its own value in.
    let mut buffer = [0u8; 64];
    let mut none = [nexus_user::Handle(0); 1];
    if nexus_user::receive(child, &mut buffer, &mut none).is_err() {
        failed("init: FAILED: no answer about the shared page");
        return;
    }

    // SAFETY: still mapped; the other process wrote through its own mapping of
    // the same frames.
    let seen = unsafe { core::ptr::read_volatile(SHARED_AT as *const u64) };
    if seen == THEIRS {
        nexus_user::log("init: the other process wrote into memory we both map").ok();
    } else if seen == OURS {
        failed("init: FAILED: the shared page still holds only our own value");
    } else {
        failed("init: FAILED: the shared page holds something neither wrote");
    }
}

/// The root of the system's filesystem, as the kernel hands it over: the second
/// entry, after the spawner.
///
/// A directory handle is the whole of what this program can reach. There is no
/// call that takes a path, so a filesystem it was not handed is one it cannot
/// name -- the same argument as for the spawner, applied to files.
const ROOT: nexus_user::Handle = nexus_user::Handle(2);

/// Ask for a package to be installed, and hand over what it needs.
///
/// The installer is an ordinary program with no privilege at all. It cannot
/// reach the filesystem until this one gives it a directory, and what it can do
/// there is bounded by the rights on the handle it is given -- which is the
/// same argument as for the spawner, applied to files. A package manager that
/// had to run as the system to install a file would be a package manager that
/// can install a file anywhere.
///
/// The handle is *duplicated* before it is sent, because handles move when they
/// cross a channel: sending the only one would leave this program with no
/// filesystem for the rest of the boot.
fn install_a_package() {
    const INSTALLER: &[u8] = b"BIN/INST.ELF";
    const PACKAGE: &[u8] = b"PKG/DEMO.NEX";

    if nexus_user::send(SPAWNER, INSTALLER, &[]).is_err() {
        failed("init: FAILED: could not reach the spawn service");
        return;
    }
    let mut buffer = [0u8; 64];
    let mut handles = [nexus_user::Handle(0); 2];
    let received = match nexus_user::receive(SPAWNER, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("init: FAILED: the spawn service did not answer");
            return;
        }
    };
    if received.handles != 2 {
        let text = core::str::from_utf8(&buffer[..received.bytes]).unwrap_or("<not text>");
        nexus_user::log(text).ok();
        failed("init: FAILED: the installer did not start");
        return;
    }
    let child = handles[0];
    let process = handles[1];

    // Read, write and transfer: it has to read the package, write what is in
    // it, and be able to be given the handle at all. Not close -- an installer
    // that could close this program's root directory would be an installer that
    // can take the filesystem away from whoever started it.
    let Ok(theirs) = nexus_user::duplicate(
        ROOT,
        nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
    ) else {
        failed("init: FAILED: could not duplicate the root directory");
        return;
    };

    if nexus_user::send(child, PACKAGE, &[theirs]).is_err() {
        failed("init: FAILED: could not tell the installer what to install");
        return;
    }

    match nexus_user::wait(process) {
        Ok(nexus_user::Ending::Exited(0)) => {
            nexus_user::log("init: the installer finished, and said it worked").ok();
        }
        Ok(_) => failed("init: FAILED: the installer reported a problem"),
        Err(_) => failed("init: FAILED: could not wait for the installer"),
    }

    nexus_user::close(child).ok();
    nexus_user::close(process).ok();

    // And read one of the installed files back, from this program's own handle
    // rather than the installer's. What that checks is not the installer's
    // report -- it is that the bytes are on the disk where a different process
    // with a different handle can find them.
    let Ok(directory) = nexus_user::open(ROOT, "demo") else {
        failed("init: FAILED: the package's directory is not there");
        return;
    };
    let Ok(file) = nexus_user::open(directory, "hello.txt") else {
        failed("init: FAILED: the package's file is not there");
        return;
    };
    let mut text = [0u8; 128];
    let length = nexus_user::read(file, &mut text).unwrap_or(0);
    nexus_user::close(file).ok();
    nexus_user::close(directory).ok();

    if core::str::from_utf8(&text[..length])
        .unwrap_or("")
        .starts_with("installed from a package")
    {
        nexus_user::log("init: read a file that arrived inside a package").ok();
    } else {
        failed("init: FAILED: the installed file does not say what it should");
    }
}

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
        failed("init: FAILED: could not read the root directory");
        return;
    };
    let mut found = 0;
    for entry in nexus_user::entries(&buffer[..length]) {
        found += 1;
        let _ = entry;
    }
    if found == 0 {
        failed("init: FAILED: the root directory is empty");
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
                failed("init: FAILED: its directory exists and will not open");
                return;
            }
        },
        Err(_) => {
            failed("init: FAILED: could not make a directory");
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
            failed("init: FAILED: its own note would not open");
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
                    failed("init: FAILED: could not make a file");
                    return;
                }
            }
        }
        Err(_) => {
            failed("init: FAILED: could not open its own file");
            return;
        }
    };

    const WRITTEN: &[u8] = b"init was here, and wrote this from ring 3\n";
    if nexus_user::write(note, WRITTEN) != Ok(WRITTEN.len()) {
        failed("init: FAILED: could not write to its own file");
        return;
    }

    // Read back through the same handle, off the same disk.
    let mut back = [0u8; 128];
    match nexus_user::read(note, &mut back) {
        Ok(length) if length == WRITTEN.len() && &back[..length] == WRITTEN => {}
        _ => {
            failed("init: FAILED: what was written did not read back");
            return;
        }
    }
    if nexus_user::size(note) != Ok(WRITTEN.len()) {
        failed("init: FAILED: the file does not know its own length");
        return;
    }

    // -- And the same file, a piece at a time --------------------------------

    // Appending. There is no cursor to seek: a file has no position, only the
    // offsets its holder chooses, which is the arrangement two programs sharing
    // a file can both be right about.
    const ADDED: &[u8] = b"and this was appended
";
    let end = WRITTEN.len() as u64;
    if nexus_user::write_at(note, end, ADDED) != Ok(ADDED.len()) {
        failed("init: FAILED: could not append to its own file");
        return;
    }
    if nexus_user::size(note) != Ok(WRITTEN.len() + ADDED.len()) {
        failed("init: FAILED: appending did not make the file longer");
        return;
    }

    // Read back only the part that was appended, from the offset it went to.
    // Reading the whole file and looking at the end would pass whether or not
    // the offset was honoured.
    let mut tail = [0u8; 64];
    match nexus_user::read_at(note, end, &mut tail) {
        Ok(read) if &tail[..read] == ADDED => {}
        _ => {
            failed("init: FAILED: what was appended did not read back from its offset");
            return;
        }
    }

    // Changing four bytes in the middle, which is the case a whole-file write
    // cannot express at all: everything around them has to be left alone.
    if nexus_user::write_at(note, 4, b"WAS!") != Ok(4) {
        failed("init: FAILED: could not change part of its own file");
        return;
    }
    // "init was here..." with four bytes replaced from offset four is
    // "initWAS!" -- the space is byte four and goes with them. Counted rather
    // than guessed, because a test that expected the wrong answer here would
    // have reported the kernel as broken when it was not.
    let mut middle = [0u8; 8];
    match nexus_user::read_at(note, 0, &mut middle) {
        Ok(8) if &middle[..8] == b"initWAS!" => {}
        _ => {
            failed("init: FAILED: changing part of a file changed the wrong part");
            return;
        }
    }

    // Past the end reads as nothing, rather than as an error or as whatever
    // happened to be in the last block.
    let mut past = [0u8; 8];
    if nexus_user::read_at(note, 10_000, &mut past) != Ok(0) {
        failed("init: FAILED: reading past the end of a file returned something");
        return;
    }

    nexus_user::log("init: appended to a file and changed four bytes in the middle").ok();

    // Put it back the way the next boot expects to find it.
    if nexus_user::write(note, WRITTEN) != Ok(WRITTEN.len()) {
        failed("init: FAILED: could not restore its own file");
        return;
    }

    // A buffer too small has to be an error rather than a prefix. Half a file
    // that reports its own length looks exactly like a whole one.
    let mut tiny = [0u8; 4];
    if nexus_user::read(note, &mut tiny) != Err(nexus_user::Error::TooBig) {
        failed("init: FAILED: a short buffer was filled with a fragment");
        return;
    }

    // A name with a separator in it must be refused, not walked. If it were
    // walked, a directory handle would stop meaning "this subtree".
    if nexus_user::open(ROOT, "../system").is_ok() {
        failed("init: FAILED: a path escaped the directory it started in");
        return;
    }
    // And a name that is not there.
    if nexus_user::open(ours, "nosuch") != Err(nexus_user::Error::NotFound) {
        failed("init: FAILED: a name that is not there was opened");
        return;
    }
    // And removing something that is still open.
    if nexus_user::remove(ours, NOTE) != Err(nexus_user::Error::Filesystem) {
        failed("init: FAILED: a file was removed while it was still open");
        return;
    }

    // The directory now has the file in it, and says so.
    let Ok(length) = nexus_user::list(ours, &mut buffer) else {
        failed("init: FAILED: could not read its own directory");
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
        failed("init: FAILED: its directory does not hold what it wrote");
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
    nexus_user::log("init: PANIC").ok();
    // Straight to the status: a panicking program has no state left worth
    // consulting, and its waiter is owed a number that says so.
    nexus_user::exit_with(2)
}
