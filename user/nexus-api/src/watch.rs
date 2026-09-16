//! Waiting for whichever of several things happens first.
//!
//! The kernel has a wait set: put handles in under keys of your own choosing,
//! wait, and be told which keys are ready. This is that, with the two pieces of
//! bookkeeping every caller was writing for itself -- a buffer big enough for
//! the answer, and remembering which key meant what.
//!
//! # Why a program wants this rather than a loop of reads
//!
//! A window that reads from one channel is a window that stops answering the
//! keyboard. The case this exists for is exactly that: something slow is
//! producing output a piece at a time, and the person in front of the machine
//! has to be able to type -- and to stop it -- while it does. One wait covers
//! both, so neither waits for the other.

use alloc::vec;
use alloc::vec::Vec;

use nexus_user::{Error, Handle};

/// A set of handles, waited on together.
pub struct Watch {
    set: Handle,
    /// How many handles are in it, which is how large the answer can be.
    watched: usize,
}

impl Watch {
    /// Somewhere to wait.
    ///
    /// # Errors
    ///
    /// Whatever the kernel says, unchanged.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            set: nexus_user::wait_set()?,
            watched: 0,
        })
    }

    /// Watch `handle` under `key`.
    ///
    /// The key is yours and comes back unchanged -- whatever the program
    /// already calls that thing, not an index to look up.
    ///
    /// Channels and processes only: a channel is ready when a message is
    /// waiting *or* its peer has gone, and a process when it has ended.
    ///
    /// # Errors
    ///
    /// Whatever the kernel says, unchanged.
    pub fn add(&mut self, handle: Handle, key: u64) -> Result<(), Error> {
        nexus_user::watch(self.set, handle, key)?;
        self.watched += 1;
        Ok(())
    }

    /// Stop watching whatever has `key`.
    ///
    /// # Errors
    ///
    /// Whatever the kernel says, unchanged.
    pub fn remove(&mut self, key: u64) -> Result<(), Error> {
        nexus_user::unwatch(self.set, key)?;
        self.watched = self.watched.saturating_sub(1);
        Ok(())
    }

    /// Block until something is ready, and say which.
    ///
    /// # Errors
    ///
    /// Whatever the kernel says, unchanged.
    pub fn wait(&self) -> Result<Vec<u64>, Error> {
        self.wait_until(nexus_user::FOREVER)
    }

    /// The same, giving up after `milliseconds` and returning nothing.
    ///
    /// An empty answer and a timeout are deliberately the same thing: both mean
    /// "nothing of yours is ready", and a caller that needs to tell them apart
    /// knows what it put in the set.
    ///
    /// # Errors
    ///
    /// Whatever the kernel says, unchanged.
    pub fn wait_until(&self, milliseconds: u64) -> Result<Vec<u64>, Error> {
        // Room for every handle to be ready at once, which is the case a busy
        // machine actually produces: two clients that both wrote, a key, and a
        // process that ended can all arrive together.
        let mut keys = vec![0u64; self.watched.max(1)];
        let ready = nexus_user::wait_any_until(self.set, &mut keys, milliseconds)?;
        keys.truncate(ready);
        Ok(keys)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        nexus_user::close(self.set).ok();
    }
}
