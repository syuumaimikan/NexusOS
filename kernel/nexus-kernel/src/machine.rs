//! What this machine is doing, as something a program can hold.
//!
//! How much memory there is and how much is left, how many processors are
//! running, how many processes and threads exist, how many context switches
//! there have been. Numbers a program might reasonably want and cannot get any
//! other way — and, until this file, could only get by reading kernel memory,
//! which is the thing nothing in user space may do.
//!
//! # Why it is a channel and not a system call
//!
//! Because a system call cannot be withheld. Every call in
//! [`crate::arch::syscall`] is available to every process that runs; there is
//! no way to give one program `Uptime` and not another. A channel can be lent,
//! lent narrowly, withheld entirely, and stops existing when it is closed.
//!
//! That matters most for exactly this information. An agent that can see how
//! busy the machine is can say something about what the person using it is
//! doing, and whether it may see that should be a decision somebody makes by
//! handing over a handle — not a consequence of it being a program.
//!
//! # No names
//!
//! This service answers with counts and sizes and nothing else. There is no
//! process list and no process name, and that is a decision rather than an
//! omission: `BIN/BROWSE.ELF` is a fact about what a person is doing, and a
//! list of them is a description of somebody's afternoon. Numbers describe
//! load. Names describe a person.
//!
//! # What the numbers are not
//!
//! They are not one atomic snapshot. Each is read from where it lives, one
//! after another, and the machine goes on running in between; a reader that
//! subtracted two of them and expected the difference to be exact would be
//! wrong. `taken_at` is when the reading started. This is said here, and in the
//! reply's documentation, because a service that quietly implies more precision
//! than it has is worse than one that has less.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::IrqSpinLock;
use crate::{ipc, kprintln, sched};

/// Ask for the snapshot.
const ASK: &[u8] = b"syst";
/// It worked, and the payload follows.
const GOOD: &[u8] = b"ok  ";
/// It did not, and a two-byte reason follows.
const BAD: &[u8] = b"err!";

/// The layout of the payload. Bumped when a field moves or changes meaning.
///
/// A reader must ask for a version it knows. An unknown one is refused rather
/// than answered with something else that happens to be the same length: a
/// reader that guessed would read a field that had moved and report a number
/// that was never true.
const VERSION: u16 = 1;

/// How many bytes version 1 of the payload is.
///
/// Eight eight-byte fields and three four-byte ones. `shared/nexus-machine`
/// holds the same layout and tests it by writing a record and reading it back;
/// that test is why this is seventy-six and not the seventy-two it was.
const PAYLOAD: usize = 76;

/// Why a request was refused.
const UNKNOWN_TAG: u16 = 1;
const MALFORMED: u16 = 2;
const NO_SUCH_VERSION: u16 = 3;

/// How long a snapshot is reused before another is taken.
///
/// A caller that polls tightly gets the same answer with the same `taken_at`
/// rather than an error, which is the behaviour that lets a window redraw
/// whenever it likes without anybody having to think about rate limits. And it
/// is what stops a program making the kernel walk its own tables in a loop.
const FRESH_MS: u64 = 100;

/// The ends of the channels the kernel holds.
static SERVICES: IrqSpinLock<Vec<Arc<ipc::Endpoint>>> = IrqSpinLock::new(Vec::new());

/// The last snapshot, and when it was taken.
static CACHED: IrqSpinLock<Option<(u64, Snapshot)>> = IrqSpinLock::new(None);

/// Requests answered and requests refused, for the monitor.
static ANSWERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Requests answered, requests refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        ANSWERED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// What one reading of the machine says.
///
/// Every count that grows for the life of the machine is a `u64`. A
/// context-switch total is thirteen million after five seconds of one
/// self-test; thirty-two bits of it would wrap inside a day, and a counter that
/// wraps is a counter whose differences are sometimes enormous and negative.
#[derive(Clone, Copy, Default)]
struct Snapshot {
    taken_at: u64,
    memory_total: u64,
    memory_free: u64,
    heap_used: u64,
    heap_total: u64,
    processors: u32,
    processes_running: u32,
    threads: u32,
    processes_started: u64,
    processes_ended: u64,
    context_switches: u64,
}

impl Snapshot {
    /// Write it as the reply carries it: little-endian, fixed offsets.
    fn to_bytes(self) -> [u8; PAYLOAD] {
        let mut out = [0u8; PAYLOAD];
        let mut at = 0;
        let put64 = |value: u64, out: &mut [u8; PAYLOAD], at: &mut usize| {
            out[*at..*at + 8].copy_from_slice(&value.to_le_bytes());
            *at += 8;
        };
        put64(self.taken_at, &mut out, &mut at);
        put64(self.memory_total, &mut out, &mut at);
        put64(self.memory_free, &mut out, &mut at);
        put64(self.heap_used, &mut out, &mut at);
        put64(self.heap_total, &mut out, &mut at);
        put64(self.processes_started, &mut out, &mut at);
        put64(self.processes_ended, &mut out, &mut at);
        put64(self.context_switches, &mut out, &mut at);
        out[at..at + 4].copy_from_slice(&self.processors.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&self.processes_running.to_le_bytes());
        out[at + 8..at + 12].copy_from_slice(&self.threads.to_le_bytes());
        out
    }
}

/// Read the machine, now.
fn take() -> Snapshot {
    let now = crate::arch::time::ticks();
    let memory = crate::memory::stats();
    let heap = crate::memory::heap::stats();
    let threads = sched::stats();
    let (started, ended) = crate::process::statistics();

    // Bytes, not frames. A frame is four kibibytes on this port and something
    // else on another, and a number whose unit is a property of the machine it
    // came from is a number nobody can compare.
    let (memory_total, memory_free) = match memory {
        Some(memory) => (memory.managed_frames * 4096, memory.free_frames * 4096),
        None => (0, 0),
    };

    Snapshot {
        taken_at: now,
        memory_total,
        memory_free,
        heap_used: heap.used as u64,
        heap_total: heap.total as u64,
        processors: crate::arch::percpu::online_count() as u32,
        // What started and has not ended. Saturating, because the two counters
        // are read one after the other and a process that ends between them
        // would otherwise make this the largest number a u64 can hold.
        processes_running: started.saturating_sub(ended) as u32,
        threads: threads.threads as u32,
        processes_started: started,
        processes_ended: ended,
        context_switches: threads.context_switches,
    }
}

/// The snapshot, taken again only if the last one has gone stale.
fn current() -> Snapshot {
    let now = crate::arch::time::ticks();
    let mut cached = CACHED.lock();
    if let Some((taken, snapshot)) = *cached {
        if now.saturating_sub(taken) < FRESH_MS {
            return snapshot;
        }
    }
    let snapshot = take();
    *cached = Some((now, snapshot));
    snapshot
}

/// Make a channel to this service, and return the end a program should hold.
///
/// The program needs read *and* write on it: a request-and-reply service is
/// asked as well as answered, and a handle without write cannot carry the ask.
/// "Read-only" here describes what the service does to the machine, not the
/// rights on the handle.
pub fn endpoint() -> Arc<ipc::Endpoint> {
    let (service, client) = ipc::Endpoint::pair();
    SERVICES.lock().push(service);
    client
}

/// Start the thread that answers.
pub fn start_thread() {
    match sched::spawn(
        "machine",
        // Below anything a person is waiting on. Nobody is watching a number.
        sched::thread::Priority::Background,
        machine_thread,
        0,
    ) {
        Ok(id) => kprintln!("[mach] machine information thread {id} started"),
        Err(error) => kprintln!("[mach] could not start the machine thread: {error}"),
    }
}

/// Wait to be asked, and answer.
fn machine_thread(_argument: usize) {
    let set = Arc::new(crate::waitset::WaitSet::new());
    let mut watched = 0usize;

    loop {
        // The set is rebuilt when the list of services changes, which happens
        // when a program is lent one and when one goes away. Rebuilt rather
        // than maintained because the list is three or four long and the
        // bookkeeping to keep a set in step with it would be more code than the
        // rebuild it replaces.
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

        let seen = set.change_count();
        let answered = serve(&services);

        if !answered {
            // Read before testing and blocked only if nothing has changed
            // since, which is the discipline every wait in this kernel follows:
            // a request arriving between the two would otherwise be a request
            // nobody wakes for.
            //
            // With a deadline as well, because a service lent after this thread
            // last looked is not a member of the set and cannot wake it.
            set.wait_since(seen, Some(crate::arch::time::ticks() + 500));
        }
    }
}

/// Answer everything that has been asked. Returns whether anything was.
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

/// Do one request.
///
/// `syst` with the version the caller understands. There is one request,
/// because there is one thing to ask.
fn answer(request: &[u8]) -> Vec<u8> {
    use core::sync::atomic::Ordering;

    let refuse = |code: u16| {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        let mut reply = Vec::with_capacity(6);
        reply.extend_from_slice(BAD);
        reply.extend_from_slice(&code.to_le_bytes());
        reply
    };

    let Some(tag) = request.get(..4) else {
        return refuse(MALFORMED);
    };
    if tag != ASK {
        return refuse(UNKNOWN_TAG);
    }
    // Exactly the tag and the version, and nothing after it. A request with
    // trailing bytes is a request from something that believes this protocol is
    // a different shape, and answering it would confirm the belief.
    if request.len() != 6 {
        return refuse(MALFORMED);
    }
    let wanted = u16::from_le_bytes([request[4], request[5]]);
    if wanted != VERSION {
        return refuse(NO_SUCH_VERSION);
    }

    let snapshot = current();
    ANSWERED.fetch_add(1, Ordering::Relaxed);

    let mut reply = Vec::with_capacity(8 + PAYLOAD);
    reply.extend_from_slice(GOOD);
    reply.extend_from_slice(&VERSION.to_le_bytes());
    reply.extend_from_slice(&(PAYLOAD as u16).to_le_bytes());
    reply.extend_from_slice(&snapshot.to_bytes());
    reply
}
