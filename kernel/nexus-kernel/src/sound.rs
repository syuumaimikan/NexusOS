//! Sound, as something a program can hold.
//!
//! The same shape as the network: a channel, and holding an end of it is the
//! whole of the authority. There is no system call that makes a noise, so a
//! program that was never handed this cannot make one — which matters more for
//! sound than it looks, because the speaker is the one output on this machine
//! that cannot be ignored by looking elsewhere.
//!
//! # Why it is a thread and not a call
//!
//! A tone lasts. Playing one means starting it, waiting, and stopping it, and
//! the waiting must not happen on the caller's thread: a program that asked for
//! a half-second chime would be a window that stopped repainting for half a
//! second. So the request returns at once and a thread of the kernel's own does
//! the waiting.
//!
//! That also makes the queue meaningful. Two programs asking for a tone at the
//! same moment cannot both have the speaker — there is one — so the second waits
//! for the first rather than cutting it off, and a program that asked for a
//! sequence gets a sequence.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::IrqSpinLock;
use crate::{drivers::speaker, ipc, kprintln, sched};

/// How many tones may be waiting to be played.
///
/// A bound rather than a policy: a program asking for a thousand notes should
/// not be able to make the kernel hold a thousand notes.
const MAX_QUEUED: usize = 32;

/// The longest one tone will be held for.
const MAX_MS: u64 = 3_000;

/// One tone, waiting its turn.
struct Note {
    hertz: u32,
    milliseconds: u64,
}

/// What is waiting to be played.
static QUEUE: IrqSpinLock<VecDeque<Note>> = IrqSpinLock::new(VecDeque::new());

/// The ends of the channel the kernel holds.
static SERVICES: IrqSpinLock<Vec<Arc<ipc::Endpoint>>> = IrqSpinLock::new(Vec::new());

/// Notes played and requests refused, for the monitor.
static PLAYED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Notes played, requests refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        PLAYED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// Make a channel to the speaker, and return the end a program should hold.
pub fn endpoint() -> Arc<ipc::Endpoint> {
    let (service, client) = ipc::Endpoint::pair();
    SERVICES.lock().push(service);
    client
}

/// Ask for a tone from inside the kernel.
///
/// What the start-up chime uses, and what a program's request turns into.
pub fn play(hertz: u32, milliseconds: u64) -> bool {
    let mut queue = QUEUE.lock();
    if queue.len() >= MAX_QUEUED {
        REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return false;
    }
    queue.push_back(Note {
        hertz,
        milliseconds: milliseconds.min(MAX_MS),
    });
    true
}

/// Throw away whatever is waiting and stop what is playing.
pub fn silence() {
    QUEUE.lock().clear();
    speaker::stop();
}

/// Start the thread that plays what has been asked for.
pub fn start_thread() {
    match sched::spawn(
        "sound",
        // Below the interface but above background work: a note that is late is
        // a note in the wrong place, and a note that delays a keystroke is
        // worse.
        sched::thread::Priority::Normal,
        sound_thread,
        0,
    ) {
        Ok(id) => kprintln!("[snd ] sound thread {id} started"),
        Err(error) => kprintln!("[snd ] could not start the sound thread: {error}"),
    }
}

/// Play what has been asked for, and answer whoever asked.
fn sound_thread(_argument: usize) {
    // The machine saying it is up. First, and before anything can ask for
    // anything, so that the chime is the chime rather than whatever a program
    // queued during bring-up.
    for (hertz, milliseconds) in [(523u32, 90u64), (659, 90), (784, 150)] {
        speaker::tone(hertz, milliseconds);
        sched::sleep_ms(25);
    }
    kprintln!("[snd ] played the start-up chime");

    loop {
        serve();

        let next = QUEUE.lock().pop_front();
        match next {
            Some(note) => {
                speaker::tone(note.hertz, note.milliseconds);
                PLAYED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                // A gap between notes, so a sequence is a sequence rather than
                // one long slur at whichever frequency came last.
                sched::sleep_ms(15);
            }
            // Nothing to play. Polled rather than blocked, because this thread
            // waits on two things -- a queue and a channel -- and twenty
            // milliseconds of latency on a beep is inaudible where the
            // machinery to wait on both would be real.
            None => sched::sleep_ms(20),
        }
    }
}

/// Answer everything that has been asked.
fn serve() {
    let services: Vec<Arc<ipc::Endpoint>> = SERVICES.lock().clone();
    let mut gone = false;

    for service in &services {
        for _ in 0..16 {
            if service.queued() == 0 {
                break;
            }
            let Some(request) = service.try_receive() else {
                break;
            };
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
}

/// Do one request.
///
/// `tone` with a frequency and a duration, or `hush` to stop everything. Two
/// requests, because a speaker with one bit has two things it can be told.
fn answer(request: &[u8]) -> Vec<u8> {
    let Some(what) = request.get(..4) else {
        return b"err!a request with no tag".to_vec();
    };
    let rest = &request[4..];

    match what {
        b"tone" => {
            if rest.len() < 8 {
                return b"err!a tone needs a frequency and a length".to_vec();
            }
            let hertz = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let milliseconds = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as u64;
            if play(hertz, milliseconds) {
                b"ok  ".to_vec()
            } else {
                b"err!too many notes are already waiting".to_vec()
            }
        }
        b"hush" => {
            silence();
            b"ok  ".to_vec()
        }
        _ => b"err!no such request".to_vec(),
    }
}
