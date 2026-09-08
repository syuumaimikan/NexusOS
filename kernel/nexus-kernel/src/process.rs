//! Processes.
//!
//! Until now a user program was an address space and a thread that happened to
//! run in it, which is most of what a process is and none of what makes the
//! word useful. A process is the thing that *holds* — an address space, a
//! handle table, and eventually threads, memory and a name. Anything a thread
//! is allowed to do that is not simply arithmetic, it is allowed to do because
//! of the process it belongs to.
//!
//! # Shared, and why
//!
//! Threads hold an [`Arc<Process>`]. A process outlives whichever of its
//! threads is reaped first, and the last reference going away is what frees the
//! address space — by which point no processor can still have it loaded, since
//! a thread must be switched away from before it can be reaped, and every
//! switch sets `cr3`.
//!
//! # What is deliberately absent
//!
//! No parent, no children, no exit status, no way to create one from inside the
//! system. A process is created by the kernel at boot and ends when its thread
//! does. Those are the next things, and they want IPC underneath them: a
//! process asking for a process is a message to whoever is allowed to answer,
//! not a system call that trusts the asker.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ipc::HandleTable;
use crate::memory::address_space::AddressSpace;

/// A process identifier.
///
/// Distinct from a thread identifier on purpose: they are different things and
/// will diverge as soon as a process has more than one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessId(pub u64);

impl core::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// Everything a thread is allowed to reach by virtue of what it belongs to.
pub struct Process {
    pub id: ProcessId,
    pub name: String,
    /// The page tables its threads run on.
    pub address_space: Arc<AddressSpace>,
    /// The objects it may name, and what it may do with them.
    pub handles: HandleTable,
}

/// Next process identifier. Never reused, so a stale identifier in a log names
/// nothing rather than something else.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Processes created and destroyed.
static CREATED: AtomicU64 = AtomicU64::new(0);
static DESTROYED: AtomicU64 = AtomicU64::new(0);

impl Process {
    /// Create a process around an address space.
    #[must_use]
    pub fn new(name: &str, address_space: Arc<AddressSpace>) -> Arc<Self> {
        CREATED.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            id: ProcessId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            name: String::from(name),
            address_space,
            handles: HandleTable::new(),
        })
    }

    /// Physical root of its page tables.
    #[must_use]
    pub fn page_table_root(&self) -> u64 {
        self.address_space.root()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        DESTROYED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Processes created and destroyed since boot.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        CREATED.load(Ordering::Relaxed),
        DESTROYED.load(Ordering::Relaxed),
    )
}
