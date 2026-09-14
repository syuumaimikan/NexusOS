//! Crash reports: what ended, why, and somewhere it survives a reboot.
//!
//! A fault that only ever appears on a serial line is a fault nobody sees on a
//! machine with no serial line. What makes a report useful is that it is still
//! there afterwards, so this writes one to the filesystem — and the next boot
//! reads them back and says how many it found.
//!
//! # Why the report is not written where it is made
//!
//! Because it is made inside an exception handler. Writing a file means the
//! block device, the journal and a sleeping lock, and the thing that just
//! faulted may have been holding any of them: a handler that reached for a lock
//! its own thread already owns would turn a program's bug into a machine that
//! stops. So the handler does the one thing that cannot block — writes into a
//! fixed array under an interrupt-safe lock — and the monitor thread, which is
//! an ordinary thread with nothing held, takes what is there and writes it out.
//!
//! That is the difference between a crash report and a crash: the report has to
//! work when the system is in the worst state it has been in all boot.

use alloc::format;
use alloc::string::String;

use crate::sync::IrqSpinLock;

/// How many reports are held before the monitor gets to them.
///
/// Eight. A machine producing nine faults before the monitor next runs has a
/// problem the ninth report would not explain, and a fixed array is what makes
/// this safe to fill from an exception handler.
const PENDING: usize = 8;

/// The directory reports are kept in.
const DIRECTORY: &str = "crash";

/// One report, as much as can be recorded without allocating.
#[derive(Clone, Copy)]
struct Report {
    used: bool,
    process: u64,
    /// The name, copied rather than borrowed: the process it belongs to is
    /// about to stop existing.
    name: [u8; 32],
    name_length: usize,
    what: [u8; 48],
    what_length: usize,
    ticks: u64,
}

impl Report {
    const fn empty() -> Self {
        Self {
            used: false,
            process: 0,
            name: [0; 32],
            name_length: 0,
            what: [0; 48],
            what_length: 0,
            ticks: 0,
        }
    }
}

/// What is waiting to be written.
static REPORTS: IrqSpinLock<[Report; PENDING]> = IrqSpinLock::new([Report::empty(); PENDING]);

/// Faults recorded, and reports dropped because there was no room.
static RECORDED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DROPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Reports written to the filesystem.
static WRITTEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Note that a process was ended by a fault.
///
/// Safe to call from an exception handler: it takes one interrupt-safe lock,
/// copies bytes into an array, and returns. Nothing here allocates, reads a
/// disk, or waits for anything.
pub fn record(process: u64, name: &str, what: &str) {
    let mut reports = REPORTS.lock();
    let Some(slot) = reports.iter_mut().find(|report| !report.used) else {
        DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    };

    *slot = Report::empty();
    slot.used = true;
    slot.process = process;
    slot.ticks = crate::arch::time::ticks();

    let name = name.as_bytes();
    slot.name_length = name.len().min(slot.name.len());
    slot.name[..slot.name_length].copy_from_slice(&name[..slot.name_length]);

    let what = what.as_bytes();
    slot.what_length = what.len().min(slot.what.len());
    slot.what[..slot.what_length].copy_from_slice(&what[..slot.what_length]);

    RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Write whatever is waiting to the filesystem.
///
/// Called from an ordinary thread, holding nothing. Returns how many were
/// written.
pub fn flush() -> usize {
    // Taken out from under the lock first, so the filesystem work happens with
    // nothing held: an interrupt-safe lock held across a disk write is a
    // processor spinning on a lock whose owner is asleep.
    let taken = {
        let mut reports = REPORTS.lock();
        let mut taken = [Report::empty(); PENDING];
        for (slot, report) in taken.iter_mut().zip(reports.iter_mut()) {
            if report.used {
                *slot = *report;
                *report = Report::empty();
            }
        }
        taken
    };

    let mut written = 0;
    for report in taken.iter().filter(|report| report.used) {
        let name = core::str::from_utf8(&report.name[..report.name_length]).unwrap_or("?");
        let what = core::str::from_utf8(&report.what[..report.what_length]).unwrap_or("?");
        let text = format!(
            "process {} \"{}\"\nended by {}\nat {} ticks\n",
            report.process, name, what, report.ticks
        );
        let file = format!("{}.txt", report.process);
        match crate::fs::store::seed(DIRECTORY, &file, text.as_bytes()) {
            Ok(_) => {
                written += 1;
                WRITTEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                crate::kprintln!("[crash] wrote {DIRECTORY}/{file}: {name} ended by {what}");
            }
            Err(error) => {
                crate::kprintln!("[crash] could not write {DIRECTORY}/{file}: {error}");
            }
        }
    }
    written
}

/// What previous boots left behind.
///
/// Read once, at startup. A machine that had crashed and could not say so
/// afterwards would be a machine where the interesting failures are the ones
/// nobody hears about.
pub fn report_previous() {
    let Ok(entries) = crate::fs::store::list(DIRECTORY) else {
        // No directory means nothing has ever crashed, which is the ordinary
        // case and not worth a line.
        return;
    };
    if entries.is_empty() {
        return;
    }
    crate::kprintln!(
        "[crash] {} report(s) from previous boots are on the filesystem",
        entries.len()
    );
    for entry in entries.iter().take(4) {
        let path = format!("{DIRECTORY}/{}", entry.name.as_str());
        match read(&path) {
            Some(text) => {
                let first = text.lines().next().unwrap_or("");
                let second = text.lines().nth(1).unwrap_or("");
                crate::kprintln!("[crash]   {}: {first}, {second}", entry.name.as_str());
            }
            None => crate::kprintln!("[crash]   {}: unreadable", entry.name.as_str()),
        }
    }
}

/// Read one report back.
fn read(path: &str) -> Option<String> {
    let (directory, name) = path.split_once('/')?;
    let root = crate::fs::store::root().ok()?;
    let folder = crate::fs::store::open_child(&root, directory).ok()?;
    let file = crate::fs::store::open_child(&folder, name).ok()?;
    let bytes = crate::fs::store::read_node(&file).ok()?;
    String::from_utf8(bytes).ok()
}

/// Faults recorded, reports written, and reports dropped for want of room.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering;
    (
        RECORDED.load(Ordering::Relaxed),
        WRITTEN.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
    )
}
