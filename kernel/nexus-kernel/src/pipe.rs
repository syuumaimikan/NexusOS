//! A stream of bytes with two ends.
//!
//! This system has channels, and a channel is not a pipe. A channel carries
//! *messages*: what comes out of one read is exactly what went into one write,
//! and that boundary is a feature — it is what lets a service read one request
//! without knowing how long it is. A pipe has no boundaries at all. Three
//! writes of ten bytes are thirty bytes, and a reader asking for seven gets
//! seven.
//!
//! Neither can be built out of the other without losing something. A pipe on
//! top of a channel would either lose the ability to read part of a message or
//! grow a buffer and a cursor in every caller; a channel on top of a pipe would
//! need a length prefix and a parser, which is what a message *is*. So this is
//! its own object.
//!
//! # Why it is here and not in the compatibility layer
//!
//! Because a byte stream is not a Linux idea. Linux's `pipe` is one way of
//! asking for one; a terminal is another, a socket is another. This system had
//! none, which meant a program here could hand another program a *message* and
//! could not hand it a *stream* — and every shell pipeline anybody has ever
//! written is a stream.
//!
//! So it is a Nexus object, with a Nexus system call, held by ordinary handles
//! with ordinary rights. The Linux `pipe2` is a translation of it, in
//! `compat::linux_files`, and there is nothing a translated program can do with
//! one that a Nexus program cannot.
//!
//! # The two ends
//!
//! A [`Pipe`] is the buffer. A [`PipeEnd`] is one side of it, and a handle names
//! an end rather than the pipe — because "you may read this" and "you may write
//! this" are different authorities, and a single object that did both would
//! make the two indistinguishable.
//!
//! Which end a handle names is what makes end-of-file work. A reader is at the
//! end of the stream when the buffer is empty *and no write end is left*, and
//! nothing else is a reliable signal: a buffer that is merely empty may be
//! filled a microsecond later. The count is kept on the pipe and moved by
//! `PipeEnd`'s `Drop`, so it falls to zero exactly when the last handle to the
//! writing side is closed — including after a `dup`, where two handles name one
//! end and the end goes when the second of them does.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;
use crate::waitset::Watchers;

/// How many bytes a pipe holds before a writer has to wait.
///
/// Sixty-four kilobytes, which is what Linux uses. The number is not arbitrary
/// on either system: it is the amount of slack that lets a producer run ahead
/// of a consumer through a burst without either of them blocking, and it is
/// small enough that a program writing into a pipe nobody reads is stopped
/// rather than allowed to take the machine's memory.
pub const CAPACITY: usize = 64 * 1024;

/// Pipes made, and bytes through them, for the monitor.
static CREATED: AtomicU64 = AtomicU64::new(0);
static CARRIED: AtomicU64 = AtomicU64::new(0);

/// Pipes created and bytes carried.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        CREATED.load(Ordering::Relaxed),
        CARRIED.load(Ordering::Relaxed),
    )
}

/// The buffer between the two ends.
pub struct Pipe {
    bytes: IrqSpinLock<VecDeque<u8>>,
    /// Threads blocked waiting for something to read.
    readable: WaitQueue,
    /// Threads blocked waiting for room to write.
    writable: WaitQueue,
    /// Wait sets to tell when either of those changes.
    watchers: Watchers,
    /// How many ends of each kind are open. See the note at the top.
    readers: AtomicUsize,
    writers: AtomicUsize,
}

/// One side of a pipe: the authority to read it, or to write it.
pub struct PipeEnd {
    pipe: Arc<Pipe>,
    writing: bool,
}

/// Why a read or a write could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeError {
    /// The end is the other kind: a read of a write end, or the reverse.
    WrongEnd,
    /// Every read end has been closed, so nothing will ever read this.
    NoReader,
    /// The calling thread was asked to stop while it waited.
    Cancelled,
}

impl core::fmt::Display for PipeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::WrongEnd => "that end of the pipe cannot be used that way",
            Self::NoReader => "nothing is reading the other end",
            Self::Cancelled => "the thread was asked to stop",
        })
    }
}

impl Pipe {
    /// A pipe, and the two ends of it.
    #[must_use]
    pub fn pair() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
        let pipe = Arc::new(Self {
            bytes: IrqSpinLock::new(VecDeque::new()),
            readable: WaitQueue::new(),
            writable: WaitQueue::new(),
            watchers: Watchers::new(),
            readers: AtomicUsize::new(1),
            writers: AtomicUsize::new(1),
        });
        CREATED.fetch_add(1, Ordering::Relaxed);
        (
            Arc::new(PipeEnd {
                pipe: Arc::clone(&pipe),
                writing: false,
            }),
            Arc::new(PipeEnd {
                pipe,
                writing: true,
            }),
        )
    }

    /// Start telling `set` when this pipe may have become ready.
    pub fn watch(&self, set: alloc::sync::Weak<crate::waitset::WaitSet>) {
        self.watchers.add(set);
    }

    /// How many bytes are waiting to be read.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.bytes.lock().len()
    }

    /// How much room is left for a writer.
    #[must_use]
    pub fn room(&self) -> usize {
        CAPACITY.saturating_sub(self.queued())
    }

    /// Whether any end that can write is still open.
    #[must_use]
    pub fn writers_open(&self) -> bool {
        self.writers.load(Ordering::Acquire) > 0
    }

    /// Whether any end that can read is still open.
    #[must_use]
    pub fn readers_open(&self) -> bool {
        self.readers.load(Ordering::Acquire) > 0
    }
}

impl PipeEnd {
    /// Whether this end is the one that writes.
    #[must_use]
    pub const fn is_writing(&self) -> bool {
        self.writing
    }

    /// The pipe behind it.
    #[must_use]
    pub fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }

    /// Whether a read on this end would return without waiting.
    ///
    /// True at end of file as well as with bytes waiting, and that is not a
    /// mistake: a reader whose writers have all gone must be woken, because the
    /// zero it is about to read is the answer it has been waiting for. A
    /// readiness test that reported only "there are bytes" would leave every
    /// reader of a finished pipe asleep for ever.
    #[must_use]
    pub fn readable(&self) -> bool {
        !self.writing && (self.pipe.queued() > 0 || !self.pipe.writers_open())
    }

    /// Whether a write on this end would return without waiting.
    ///
    /// True with no readers left, for the same reason: the error it is about to
    /// get is an answer.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.writing && (self.pipe.room() > 0 || !self.pipe.readers_open())
    }

    /// Take up to `wanted` bytes.
    ///
    /// Blocks while the pipe is empty and a writer could still arrive, which is
    /// what a pipe is for. Returns zero — end of file — only when the buffer is
    /// empty *and* every write end has been closed, because those two together
    /// are the only thing that means no more is coming.
    ///
    /// # Errors
    ///
    /// [`PipeError::WrongEnd`] on the writing end; [`PipeError::Cancelled`] if
    /// the thread is asked to stop while it waits.
    pub fn read(&self, wanted: usize) -> Result<alloc::vec::Vec<u8>, PipeError> {
        if self.writing {
            return Err(PipeError::WrongEnd);
        }
        loop {
            // The generation is read before the buffer is looked at, so a write
            // that lands between the two is seen as a change rather than slept
            // through. The same handshake every wait in this kernel uses.
            let seen = self.pipe.readable.generation();
            {
                let mut bytes = self.pipe.bytes.lock();
                if !bytes.is_empty() {
                    let take = wanted.min(bytes.len());
                    let out: alloc::vec::Vec<u8> = bytes.drain(..take).collect();
                    drop(bytes);
                    CARRIED.fetch_add(take as u64, Ordering::Relaxed);
                    // A reader taking bytes out has made room, which is what a
                    // blocked writer is waiting for.
                    self.pipe.writable.wake_all();
                    self.pipe.watchers.signal();
                    return Ok(out);
                }
            }
            if !self.pipe.writers_open() {
                // End of file, and the honest answer rather than a wait that
                // cannot end.
                return Ok(alloc::vec::Vec::new());
            }
            if crate::sched::cancelled() {
                return Err(PipeError::Cancelled);
            }
            self.pipe.readable.wait_if_unchanged(seen);
        }
    }

    /// Put bytes in, waiting for room.
    ///
    /// Returns how many were taken, which may be fewer than offered: a writer
    /// filling a pipe faster than it is drained gets a short write, which is a
    /// thing every caller of `write` already handles because it happens on
    /// Linux too.
    ///
    /// # Errors
    ///
    /// [`PipeError::WrongEnd`] on the reading end, and [`PipeError::NoReader`]
    /// when every read end has gone — which on Linux is `SIGPIPE` and `EPIPE`,
    /// and here is only the second of those, because there are no signals.
    pub fn write(&self, data: &[u8]) -> Result<usize, PipeError> {
        if !self.writing {
            return Err(PipeError::WrongEnd);
        }
        if data.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pipe.readers_open() {
                return Err(PipeError::NoReader);
            }
            let seen = self.pipe.writable.generation();
            {
                let mut bytes = self.pipe.bytes.lock();
                let room = CAPACITY.saturating_sub(bytes.len());
                if room > 0 {
                    let take = room.min(data.len());
                    bytes.extend(&data[..take]);
                    drop(bytes);
                    CARRIED.fetch_add(take as u64, Ordering::Relaxed);
                    self.pipe.readable.wake_all();
                    self.pipe.watchers.signal();
                    return Ok(take);
                }
            }
            if crate::sched::cancelled() {
                return Err(PipeError::Cancelled);
            }
            self.pipe.writable.wait_if_unchanged(seen);
        }
    }
}

impl Drop for PipeEnd {
    /// The last handle to this end has gone.
    ///
    /// This is where end-of-file comes from. A reader blocked on an empty pipe
    /// is woken so that it can see there are no writers left and return zero;
    /// a writer blocked for room is woken so that it can see there is nobody to
    /// read what it was writing. Without the wake, both of them wait for a
    /// change that has already happened and will not happen again.
    fn drop(&mut self) {
        if self.writing {
            self.pipe.writers.fetch_sub(1, Ordering::Release);
        } else {
            self.pipe.readers.fetch_sub(1, Ordering::Release);
        }
        self.pipe.readable.wake_all();
        self.pipe.writable.wake_all();
        self.pipe.watchers.signal();
    }
}
