//! TLB shootdown.
//!
//! `invlpg` invalidates a translation on the processor that executes it and
//! nowhere else. While only the boot processor ran threads that was enough;
//! now that every processor does, unmapping a page on one leaves the others
//! free to keep using a cached translation to memory that has been freed and
//! handed to someone else. Nothing about that fails loudly — it is a silent
//! read or write to the wrong page — which is exactly why it has to be closed
//! rather than watched for.
//!
//! # How a shootdown runs
//!
//! One processor at a time initiates, serialised by [`SHOOTDOWN`]. It writes
//! the range into each other online processor's mailbox, bumps that mailbox's
//! sequence number, sends an interrupt to wake it, invalidates locally, then
//! waits for every mailbox to report the sequence it was given.
//!
//! # Why the waiting is the hard part
//!
//! The obvious implementation deadlocks. A processor spinning for any
//! interrupt-masking lock cannot take the shootdown interrupt, so an initiator
//! that holds a lock the spinner wants waits forever for an acknowledgement
//! that can never arrive. That is not hypothetical here: reaping a finished
//! thread unmaps its stack while holding the scheduler lock, which is precisely
//! the shape of the problem.
//!
//! So a mailbox is *polled* as well as interrupted. [`service_pending`] is
//! lock-free and is called from every spin loop in [`crate::sync`], so a
//! processor waiting for a lock still answers shootdowns. The interrupt is what
//! reaches a processor that is idle or running with interrupts enabled; the
//! polling is what reaches one that is spinning with them masked. Between them
//! there is no state a processor can be in and not answer.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::percpu::{self, MAX_PROCESSORS};
use crate::sync::SpinLock;

/// A [`Mailbox::pages`] value meaning "everything", used when a change is too
/// large or too structural to describe as a page range.
const ALL_PAGES: u64 = u64::MAX;

/// Largest range still worth invalidating one page at a time.
///
/// Beyond this, reloading `cr3` costs less than the individual `invlpg`s, even
/// counting the translations it throws away that were still wanted.
const MAX_INDIVIDUAL_PAGES: u64 = 32;

/// One processor's shootdown mailbox.
///
/// Cache-line aligned so that acknowledging on one processor does not bounce
/// the line another is polling.
#[repr(align(64))]
struct Mailbox {
    /// First address of the range to invalidate.
    start: AtomicU64,
    /// Pages in the range, or [`ALL_PAGES`].
    pages: AtomicU64,
    /// Incremented by the initiator once `start` and `pages` are written.
    requested: AtomicU64,
    /// Set to `requested` by the target once it has invalidated.
    completed: AtomicU64,
}

impl Mailbox {
    const fn new() -> Self {
        Self {
            start: AtomicU64::new(0),
            pages: AtomicU64::new(0),
            requested: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        }
    }
}

static MAILBOXES: [Mailbox; MAX_PROCESSORS] = [const { Mailbox::new() }; MAX_PROCESSORS];

/// Serialises initiators, so only one processor writes a given mailbox.
///
/// Deliberately a plain [`SpinLock`] and not an `IrqSpinLock`: an initiator
/// must keep answering shootdowns while it waits, and this lock's spin loop is
/// one of the places that happens.
static SHOOTDOWN: SpinLock<()> = SpinLock::new(());

/// Shootdowns performed, for diagnostics.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Shootdowns that gave up waiting for an acknowledgement.
static TIMEOUTS: AtomicUsize = AtomicUsize::new(0);

/// Spins to wait for one processor before declaring it unresponsive.
///
/// Generous: the target may be finishing an interrupt handler or a critical
/// section of its own. It exists so that a processor wedged for some unrelated
/// reason degrades the system rather than hanging it, and so that the failure
/// is reported instead of looking like a freeze.
const ACK_SPIN_LIMIT: u32 = 20_000_000;

/// Invalidate `pages` pages starting at `start` on every processor.
///
/// # Safety
///
/// The caller must have already made the mapping change that this announces.
/// Invalidating before the page tables are updated would let a processor
/// re-cache the translation being removed.
pub unsafe fn shoot_down(start: u64, pages: u64) {
    // SAFETY: upheld by the caller.
    unsafe { broadcast(start, pages) };
}

/// Invalidate every translation on every processor.
///
/// # Safety
///
/// As [`shoot_down`]. Used for changes that are not a page range — tearing down
/// a whole top-level entry, for instance.
pub unsafe fn shoot_down_all() {
    // SAFETY: upheld by the caller.
    unsafe { broadcast(0, ALL_PAGES) };
}

/// Do the invalidation here and ask every other online processor to do the same.
///
/// # Safety
///
/// As [`shoot_down`].
unsafe fn broadcast(start: u64, pages: u64) {
    // Always invalidate locally, whether or not anyone else is running. This is
    // the single-processor case as well as the first step of the shared one.
    // SAFETY: invalidation can only discard cached translations.
    unsafe { invalidate(start, pages) };

    // Nothing to coordinate before the other processors are up, and taking a
    // lock during early bring-up would be a needless way to fail.
    if percpu::online_count() <= 1 {
        return;
    }

    // Deliberately stop here, leaving every other processor with whatever it
    // had cached. See the feature's note in Cargo.toml: this is how the
    // self-test that looks for stale translations is shown to have teeth.
    #[cfg(feature = "inject-no-shootdown")]
    return;

    #[cfg(not(feature = "inject-no-shootdown"))]
    {
        let here = percpu::cpu_index() as usize;
        let _guard = SHOOTDOWN.lock();
        COUNT.fetch_add(1, Ordering::Relaxed);

        // Publish to every other online processor, then wake them all, then wait.
        // Sending every interrupt before waiting for any acknowledgement is what
        // makes the cost one round trip rather than one per processor.
        let mut targets = [0u64; MAX_PROCESSORS];
        for index in 0..MAX_PROCESSORS {
            if index == here {
                continue;
            }
            // `snapshot` reports nothing for a processor that is not online, which
            // is the filter this needs.
            let Some((apic_id, _, _)) = percpu::snapshot(index) else {
                continue;
            };

            let mailbox = &MAILBOXES[index];
            mailbox.start.store(start, Ordering::Relaxed);
            mailbox.pages.store(pages, Ordering::Relaxed);
            // Release: the range must be visible to anyone who sees this sequence.
            let sequence = mailbox.requested.fetch_add(1, Ordering::Release) + 1;
            targets[index] = sequence;

            // SAFETY: `apic_id` came from a processor that reported itself online,
            // and the vector has a handler installed.
            unsafe { super::apic::send_fixed(apic_id, super::interrupts::TLB_SHOOTDOWN_VECTOR) };
        }

        for (index, &sequence) in targets.iter().enumerate() {
            if sequence == 0 {
                continue;
            }
            if !wait_for(index, sequence) {
                TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                crate::kprintln!(
                "[tlb ] processor {index} did not acknowledge a shootdown; its translations may be stale"
            );
            }
        }
    }
}

/// Spin until processor `index` reports `sequence`, answering our own mailbox
/// meanwhile. Returns false if it never does.
fn wait_for(index: usize, sequence: u64) -> bool {
    let mailbox = &MAILBOXES[index];
    let mut spins = 0u32;
    while mailbox.completed.load(Ordering::Acquire) < sequence {
        // The target may be spinning for a lock this processor holds, and its
        // spin loop is answering shootdowns; this processor has to do the same
        // or two initiators can wait on each other.
        service_pending();
        spins += 1;
        if spins >= ACK_SPIN_LIMIT {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Answer a shootdown request addressed to this processor, if there is one.
///
/// Lock-free and cheap when there is nothing to do — one load of a line that is
/// almost always unmodified — which is what makes it acceptable in every spin
/// loop in the kernel.
pub fn service_pending() {
    if !percpu::is_installed() {
        return;
    }
    let index = percpu::cpu_index() as usize;
    if index >= MAX_PROCESSORS {
        return;
    }

    let mailbox = &MAILBOXES[index];
    // Acquire: pairs with the initiator's release, so the range read below is
    // the one that belongs to this sequence.
    let requested = mailbox.requested.load(Ordering::Acquire);
    if mailbox.completed.load(Ordering::Relaxed) >= requested {
        return;
    }

    let start = mailbox.start.load(Ordering::Relaxed);
    let pages = mailbox.pages.load(Ordering::Relaxed);
    // SAFETY: invalidation can only discard cached translations.
    unsafe { invalidate(start, pages) };

    // Release: the invalidation must be complete before the initiator sees the
    // acknowledgement and concludes it is safe to reuse the memory.
    mailbox.completed.store(requested, Ordering::Release);
}

/// The shootdown interrupt handler's body.
pub fn on_interrupt() {
    service_pending();
}

/// Discard the named translations on this processor.
///
/// # Safety
///
/// None beyond the obvious: discarding a cached translation is always allowed,
/// so this is safe in every state. It is `unsafe` only because `flush_all`
/// reloads `cr3`, which requires the active root table to still describe the
/// executing code and stack.
unsafe fn invalidate(start: u64, pages: u64) {
    if pages == ALL_PAGES || pages > MAX_INDIVIDUAL_PAGES {
        // SAFETY: the caller changed a mapping in the address space it is
        // running in, so its own code and stack are still mapped.
        unsafe { crate::memory::paging::flush_all() };
        return;
    }
    for page in 0..pages {
        crate::memory::paging::flush(start + page * 4096);
    }
}

/// Shootdowns performed and shootdowns that timed out.
#[must_use]
pub fn statistics() -> (usize, usize) {
    (
        COUNT.load(Ordering::Relaxed),
        TIMEOUTS.load(Ordering::Relaxed),
    )
}
