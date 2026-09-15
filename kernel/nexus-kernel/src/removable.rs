//! The service a program reaches a removable drive through.
//!
//! The kernel can read and write a USB drive's blocks and mount a filesystem
//! off it. This is how anything *above* the kernel gets at it.
//!
//! # Why a service and not a directory
//!
//! Every other file on this machine is reached by opening a name in a directory
//! handle. A removable drive could have been made to look the same -- a `USB`
//! directory alongside `PICTURES` -- and it deliberately is not, for two
//! reasons.
//!
//! The first is that the node layer underneath those handles is one filesystem:
//! an inode number on the store, with no room in it for "which disk". Making
//! room would mean changing the type every open file in the system is named by,
//! which is a great deal of risk to take on behalf of a feature that does not
//! need it.
//!
//! The second is that a removable drive *is not like* a fixed one, and a
//! interface that hid the difference would be hiding something true. It can be
//! absent. It can be pulled out between two calls. A handle that stayed valid
//! across that would be lying, and a program that had no way to find out would
//! be a program that wrote into nothing.
//!
//! So it is a channel, and every request names the drive again. A program
//! holding this can be told "that drive is gone" at any point, which is the
//! honest shape.
//!
//! # What holding it means
//!
//! Everything, on every removable drive. There is no per-drive capability and
//! no read-only version of this channel yet; a program lent it can write to any
//! drive that is plugged in. That is a real limit on how finely this can be
//! handed out, and it is why the compositor lends it to the file manager and
//! not to a browser.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::usb_storage;
use crate::fs::fat32;
use crate::{ipc, kprintln, sched};

/// What a program asks for.
mod ask {
    /// How many drives there are, and what shape each is.
    pub const DRIVES: &[u8] = b"drv?";
    /// What is in a directory on one.
    pub const LIST: &[u8] = b"list";
    /// The contents of a file.
    pub const READ: &[u8] = b"read";
    /// Replace a file's contents.
    pub const WRITE: &[u8] = b"writ";
}

/// What it gets back.
const GOOD: &[u8] = b"ok  ";
const BAD: &[u8] = b"err!";

/// Why a request was refused.
mod why {
    /// No drive with that number.
    pub const NO_DRIVE: u16 = 1;
    /// The drive has no filesystem this reads.
    pub const NO_FILESYSTEM: u16 = 2;
    /// No such file or directory.
    pub const NOT_FOUND: u16 = 3;
    /// The request did not parse.
    pub const MALFORMED: u16 = 4;
    /// Writing is not implemented for this filesystem yet.
    pub const READ_ONLY: u16 = 5;
    /// More was asked for than one message carries.
    pub const TOO_BIG: u16 = 6;
}

/// The most a reply carries.
///
/// A message is 256 bytes, so anything larger has to be asked for in pieces --
/// which `read` does, by offset. This bounds one piece.
const CHUNK: usize = 192;

/// Channels this service answers on.
static SERVICES: crate::sync::IrqSpinLock<Vec<Arc<ipc::Endpoint>>> =
    crate::sync::IrqSpinLock::new(Vec::new());

/// Requests answered and refused.
static ANSWERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many requests have been answered, and how many refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        ANSWERED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// A channel to this service, for a program to be lent.
pub fn endpoint() -> Arc<ipc::Endpoint> {
    let (service, client) = ipc::Endpoint::pair();
    SERVICES.lock().push(service);
    client
}

/// Start the thread that answers.
pub fn start_thread() {
    match sched::spawn(
        "removable",
        // Background: a person waiting for a directory listing is waiting, but
        // not the way somebody pressing the power button is.
        sched::thread::Priority::Background,
        removable_thread,
        0,
    ) {
        Ok(id) => kprintln!("[rem ] removable-drive thread {id} started"),
        Err(error) => kprintln!("[rem ] could not start the removable-drive thread: {error}"),
    }
}

/// Wait to be asked, and answer.
fn removable_thread(_argument: usize) {
    let set = Arc::new(crate::waitset::WaitSet::new());
    let mut watched = 0usize;

    loop {
        // The list can grow: a program started later gets its own channel.
        let services: Vec<Arc<ipc::Endpoint>> = SERVICES.lock().clone();
        if services.len() != watched {
            for key in 0..watched as u64 {
                set.remove(key).ok();
            }
            for (key, service) in services.iter().enumerate() {
                set.add(
                    key as u64,
                    crate::waitset::Watched::Channel(Arc::clone(service)),
                )
                .ok();
            }
            watched = services.len();
        }

        // Read the counter, test, then block only if nothing has changed --
        // the discipline every wait in this kernel follows.
        let seen = set.change_count();
        if !serve(&services) {
            // With a deadline as well, because a channel lent after this thread
            // last looked is not in the set and cannot wake it.
            set.wait_since(seen, Some(crate::arch::time::ticks() + 500));
        }
    }
}

/// Answer whatever has arrived. Returns whether anything had.
fn serve(services: &[Arc<ipc::Endpoint>]) -> bool {
    let mut did = false;
    let mut gone = false;

    for service in services {
        // Bounded, so one program asking in a tight loop cannot stop the others
        // being answered.
        for _ in 0..16 {
            let Some(request) = service.try_receive() else {
                break;
            };
            did = true;
            let reply = answer(&request.bytes);
            if service.send(&reply, Vec::new()).is_err() {
                gone = true;
                break;
            }
        }
        if !service.peer_open() {
            gone = true;
        }
    }

    if gone {
        SERVICES.lock().retain(|service| service.peer_open());
    }
    did
}

/// Refuse, with a reason.
fn refuse(reason: u16) -> Vec<u8> {
    REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut out = Vec::with_capacity(6);
    out.extend_from_slice(BAD);
    out.extend_from_slice(&reason.to_le_bytes());
    out
}

/// Work out what was asked and answer it.
fn answer(message: &[u8]) -> Vec<u8> {
    if message.len() < 4 {
        return refuse(why::MALFORMED);
    }
    let tag = &message[..4];
    let body = &message[4..];

    if tag == ask::DRIVES {
        return drives();
    }

    // Everything else names a drive in its first byte.
    if body.is_empty() {
        return refuse(why::MALFORMED);
    }
    let drive = body[0] as usize;
    let rest = &body[1..];

    match tag {
        _ if tag == ask::LIST => list(drive, rest),
        _ if tag == ask::READ => read(drive, rest),
        // Writing a FAT32 file needs the allocator this reader does not have:
        // finding free clusters, chaining them, and updating both copies of the
        // table. Refused by name rather than half-done, because a write that
        // corrupted somebody's stick would be much worse than one that did not
        // happen. The blocks underneath *can* be written -- see
        // `usb_storage::write_block` -- so this is the filesystem's gap and not
        // the driver's.
        _ if tag == ask::WRITE => refuse(why::READ_ONLY),
        _ => refuse(why::MALFORMED),
    }
}

/// `drv?`: how many drives, and the shape of each.
///
/// One byte of count, then per drive: eight bytes of block count, four of block
/// size, and one saying whether a filesystem could be mounted on it.
fn drives() -> Vec<u8> {
    ANSWERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let shapes = usb_storage::shapes();
    let mut out = Vec::with_capacity(4 + 1 + shapes.len() * 13);
    out.extend_from_slice(GOOD);
    out.push(shapes.len().min(255) as u8);
    for (drive, (blocks, block_size)) in shapes.iter().enumerate().take(255) {
        out.extend_from_slice(&blocks.to_le_bytes());
        out.extend_from_slice(&block_size.to_le_bytes());
        // Whether this one has a filesystem, asked of *this* drive. Written as
        // `shapes.len()` first, which asked about a drive that does not exist
        // and so said no for every one of them.
        out.push(u8::from(mount(drive).is_some()));
    }
    out
}

/// Mount a drive's filesystem, if it has one this reads.
///
/// Mounted fresh for every request rather than kept. A removable drive can be
/// pulled out between two calls, and a mounted volume held across that would be
/// a handle onto a disk that is not there -- which would be found out by
/// reading rubbish rather than by being told.
///
/// It costs a few sector reads per request. That is the right trade for
/// something a person uses a few times a minute.
fn mount(drive: usize) -> Option<fat32::Volume> {
    let source = fat32::Source::Usb(drive);
    let partitions = crate::fs::gpt::read_on(source).ok()?;
    let partition = partitions.iter().find(|partition| partition.is_esp())?;
    fat32::Volume::mount_on(source, partition.first_lba).ok()
}

/// `list`: what is in a directory.
///
/// The body is the drive and then the path, as text. An empty path is the root.
fn list(drive: usize, path: &[u8]) -> Vec<u8> {
    if drive >= usb_storage::shapes().len() {
        return refuse(why::NO_DRIVE);
    }
    let Some(volume) = mount(drive) else {
        return refuse(why::NO_FILESYSTEM);
    };
    let Ok(path) = core::str::from_utf8(path) else {
        return refuse(why::MALFORMED);
    };
    let Ok(entries) = volume.read_directory_at(path) else {
        return refuse(why::NOT_FOUND);
    };

    ANSWERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Each entry: one byte of name length, the name, one byte saying whether it
    // is a directory, four bytes of size. Stopped when the message is full, and
    // the count at the front says how many actually fit -- so a caller can tell
    // a short listing from a complete one.
    let mut out = Vec::with_capacity(ipc::MAX_MESSAGE);
    out.extend_from_slice(GOOD);
    out.push(0);
    let mut fitted = 0u8;
    for entry in &entries {
        let name = entry.name.as_bytes();
        if name.len() > 255 || out.len() + 6 + name.len() > CHUNK {
            break;
        }
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out.push(u8::from(entry.is_directory));
        out.extend_from_slice(&entry.size.to_le_bytes());
        fitted += 1;
    }
    out[4] = fitted;
    out
}

/// `read`: part of a file.
///
/// The body is the drive, four bytes of offset, and then the path. A reply
/// carries at most [`CHUNK`] bytes, so a caller reads a file by asking
/// repeatedly with a rising offset -- and a short answer means the end.
fn read(drive: usize, body: &[u8]) -> Vec<u8> {
    if drive >= usb_storage::shapes().len() {
        return refuse(why::NO_DRIVE);
    }
    if body.len() < 4 {
        return refuse(why::MALFORMED);
    }
    let offset = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let Ok(path) = core::str::from_utf8(&body[4..]) else {
        return refuse(why::MALFORMED);
    };
    let Some(volume) = mount(drive) else {
        return refuse(why::NO_FILESYSTEM);
    };
    let Ok(bytes) = volume.read_file(path) else {
        return refuse(why::NOT_FOUND);
    };
    if offset > bytes.len() {
        return refuse(why::TOO_BIG);
    }

    ANSWERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let piece = &bytes[offset..(offset + CHUNK).min(bytes.len())];
    let mut out = Vec::with_capacity(8 + piece.len());
    out.extend_from_slice(GOOD);
    // How long the whole file is, so a caller knows what it is reading towards
    // rather than having to ask until it gets nothing.
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(piece);
    out
}
