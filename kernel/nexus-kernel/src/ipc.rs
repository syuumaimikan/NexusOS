//! Handles, rights, and channels.
//!
//! # Why handles and not names
//!
//! A system call that names a resource by a global name — a path, a process
//! identifier, a port number — has to answer "is the caller allowed to touch
//! this?" every time it is made, from an ambient authority that lives somewhere
//! else. Every such check is a place to get it wrong, and the answer usually
//! comes down to who the caller *is* rather than what it was *given*.
//!
//! A handle is the other arrangement. It is an index into a table that belongs
//! to one process, and holding it is the authority: there is no way to name a
//! channel a process was not handed, so there is no check to forget. The rights
//! recorded beside it say what may be done with it, so the same object can be
//! given to one process to read and another to write.
//!
//! This is what the Linux and Windows compatibility layers will be written on
//! top of. Both of those present descriptors and handles of their own, and both
//! will be tables in a translation layer above this one rather than a second
//! idea of authority inside the kernel.
//!
//! # What a channel is
//!
//! Two endpoints, each with an inbox. Writing to one endpoint appends to the
//! *other's* inbox and wakes whoever is waiting there. Messages are whole:
//! a read returns one message or blocks, never half of one and never two run
//! together, because a byte stream would push framing into every user of it.
//!
//! An endpoint holds a weak reference to its peer, which is what makes "the
//! other end has gone" a fact the kernel can state rather than a timeout. Two
//! strong references would keep a dead channel alive forever.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

/// Longest message a channel will carry.
///
/// Small deliberately. A message is copied twice — out of the sender and into
/// the receiver — so anything large should be shared memory with a handle to
/// it, once there is such a thing, rather than a bigger number here.
pub const MAX_MESSAGE: usize = 256;

/// Messages an endpoint will hold before writes start failing.
///
/// A bound rather than a policy: without one, a receiver that stops reading
/// turns into unbounded kernel allocation driven by a sender that need not be
/// cooperating.
pub const MAX_QUEUED: usize = 64;

/// What may be done with a handle.
///
/// Checked on every use, and narrowed rather than widened when a handle is
/// passed on: a process can give away less than it has and never more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    /// Read messages.
    pub const READ: Self = Self(1 << 0);
    /// Write messages.
    pub const WRITE: Self = Self(1 << 1);
    /// Close the handle.
    pub const CLOSE: Self = Self(1 << 2);
    /// Pass the handle along a channel to another process.
    ///
    /// Separate from the rest because it is a different kind of authority: a
    /// process can be given something it may use and may not hand on, which is
    /// the difference between lending a capability and delegating it.
    pub const TRANSFER: Self = Self(1 << 3);
    /// Everything a freshly created endpoint carries.
    pub const ALL: Self = Self(0b1111);

    /// Whether every right in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The raw bits, for reporting across the system-call boundary.
    #[must_use]
    /// The rights those bits name.
    ///
    /// Bits this system does not define are dropped rather than rejected: a
    /// caller asking for a right that does not exist gets a handle without it,
    /// which is the safe reading of an unknown request.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Handles one message may carry.
///
/// A small bound, and a real one: each handle in a message is authority in
/// flight, and a message that could carry an unbounded number of them would let
/// a sender make the receiving table grow without the receiver agreeing to it.
///
/// Eight rather than four since the terminal needed five. A program is started
/// with everything it will ever be lent in one message -- deliberately, so that
/// a program cannot be half-endowed and cannot block waiting for a second
/// message nobody is going to send -- so this bound is also the bound on how
/// many kinds of authority one program may hold. Four turned out to be a limit
/// on the system's design rather than on a sender's ambition, which is the
/// wrong thing for it to be.
pub const MAX_HANDLES: usize = 8;

/// One message in flight.
///
/// Bytes and handles together, because they have to arrive together: a message
/// saying "here is the thing" and the thing itself are one fact, and delivering
/// them separately would put the ordering problem into every user of a channel.
pub struct Message {
    pub bytes: Vec<u8>,
    /// Handles the sender gave up and the receiver has not yet taken.
    ///
    /// Owned by the message while it is in flight. A message that is never read
    /// takes them down with it, which is what closes the leak where a process
    /// gives away its last reference to something and then nobody reads it.
    pub handles: Vec<Handle>,
}

/// One end of a channel.
pub struct Endpoint {
    /// Messages written by the peer and not yet read here.
    inbox: IrqSpinLock<VecDeque<Message>>,
    /// Threads blocked in [`Endpoint::receive`] on this end.
    arrivals: WaitQueue,
    /// The other end, weakly, so that a channel both of whose ends are still
    /// referenced by their owners does not keep itself alive after they go.
    peer: IrqSpinLock<Weak<Endpoint>>,
    /// Wait sets to tell when a message arrives or the peer goes.
    watchers: crate::waitset::Watchers,
}

/// Why a channel operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    /// The message is longer than [`MAX_MESSAGE`].
    TooLong,
    /// The message carries more handles than [`MAX_HANDLES`].
    TooManyHandles,
    /// The peer's inbox is full.
    Full,
    /// The other end has been closed.
    PeerClosed,
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::TooLong => "the message is too long",
            Self::TooManyHandles => "the message carries too many handles",
            Self::Full => "the receiving end is full",
            Self::PeerClosed => "the other end of the channel is closed",
        })
    }
}

impl Endpoint {
    /// Create a connected pair.
    #[must_use]
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let first = Arc::new(Self::detached());
        let second = Arc::new(Self::detached());
        *first.peer.lock() = Arc::downgrade(&second);
        *second.peer.lock() = Arc::downgrade(&first);
        CHANNELS_CREATED.fetch_add(1, Ordering::Relaxed);
        (first, second)
    }

    fn detached() -> Self {
        Self {
            inbox: IrqSpinLock::new(VecDeque::new()),
            arrivals: WaitQueue::new(),
            peer: IrqSpinLock::new(Weak::new()),
            watchers: crate::waitset::Watchers::new(),
        }
    }

    /// Start telling `set` when a message arrives or the peer goes.
    pub fn watch(&self, set: alloc::sync::Weak<crate::waitset::WaitSet>) {
        self.watchers.add(set);
    }

    /// Whether the other end still exists.
    #[must_use]
    pub fn peer_open(&self) -> bool {
        self.peer.lock().strong_count() > 0
    }

    /// Append `message` to the peer's inbox and wake a reader there.
    ///
    /// Writing puts the message on the *other* end, which is what makes a
    /// channel a channel rather than a shared queue: a process cannot read back
    /// what it wrote.
    pub fn send(&self, message: &[u8], handles: Vec<Handle>) -> Result<usize, ChannelError> {
        if message.len() > MAX_MESSAGE {
            return Err(ChannelError::TooLong);
        }
        if handles.len() > MAX_HANDLES {
            return Err(ChannelError::TooManyHandles);
        }

        let Some(peer) = self.peer.lock().upgrade() else {
            return Err(ChannelError::PeerClosed);
        };

        {
            let mut inbox = peer.inbox.lock();
            if inbox.len() >= MAX_QUEUED {
                return Err(ChannelError::Full);
            }
            inbox.push_back(Message {
                bytes: message.to_vec(),
                handles,
            });
        }

        // Outside the inbox lock, and after the push, so a thread this wakes
        // finds the message already there.
        peer.arrivals.wake_one();
        peer.watchers.signal();
        MESSAGES_SENT.fetch_add(1, Ordering::Relaxed);
        Ok(message.len())
    }

    /// Take the next message, if one is waiting.
    #[must_use]
    pub fn try_receive(&self) -> Option<Message> {
        let message = self.inbox.lock().pop_front();
        if message.is_some() {
            MESSAGES_RECEIVED.fetch_add(1, Ordering::Relaxed);
        }
        message
    }

    /// Take the next message, blocking until one arrives.
    ///
    /// Returns `None` only when there is nothing to wait for: the peer has gone
    /// and the inbox is empty, so no message can ever arrive. Blocking forever
    /// on a channel nobody holds the other end of would be a hang, and a hang
    /// the kernel could have known about is a bug.
    pub fn receive(&self) -> Option<Message> {
        loop {
            if let Some(message) = self.try_receive() {
                return Some(message);
            }
            if !self.peer_open() {
                return None;
            }
            // Asked to stop. Reported the same way as a closed peer, because
            // to the caller it is the same fact: no message is coming, and
            // there is nothing further for this thread to do.
            if crate::sched::cancelled() {
                return None;
            }

            // The condition is re-checked inside the wait, under the queue's
            // lock, which is what closes the window between the check above and
            // the block below.
            self.arrivals.wait_until(|| {
                !self.inbox.lock().is_empty() || self.peer.lock().strong_count() == 0
            });
        }
    }

    /// Messages waiting to be read here.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.inbox.lock().len()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Anyone blocked on the peer is waiting for a message that can no
        // longer come; waking them lets `receive` notice and report it rather
        // than waiting for a write that will never happen.
        if let Some(peer) = self.peer.lock().upgrade() {
            // Every waiter, not one: the peer going is permanent, and a thread
            // left asleep on it would be asleep on something that cannot
            // change again.
            peer.arrivals.wake_all();
            peer.watchers.signal();
        }
    }
}

/// Largest shared memory object this kernel will create.
///
/// A bound rather than a policy: the size comes from a process, and every byte
/// of it is physical memory that process is asking the kernel to set aside.
///
/// Sixteen megabytes. It was one, which was enough for every window that lived
/// in a corner of the screen and not enough for the first one that filled it:
/// a surface at 1920 by 1200 is nine megabytes of pixels. A bound that is too
/// small does not fail safely -- it fails as a program that will not start,
/// with nothing on screen to say why.
///
/// It is still a bound, and it is still there for the reason it always was: the
/// number comes from a process, and without a limit a process could ask the
/// kernel to set aside all of memory one page at a time.
pub const MAX_MEMORY_OBJECT: usize = 16 << 20;

/// Memory that more than one process can see.
///
/// A channel copies its message twice, once out of the sender and once into the
/// receiver, which is right for a request and wrong for a framebuffer. This is
/// the other arrangement: the frames exist once, and a process that holds a
/// handle can map them into its own address space at an address of its
/// choosing.
///
/// The frames are individually allocated rather than one contiguous block. A
/// block would have to be freed at the order it was taken at, and the pages of
/// this are handed back one at a time as the object goes away; individually
/// allocated pages merge back into whatever blocks they came from on their own.
pub struct MemoryObject {
    frames: Vec<u64>,
    size: usize,
    /// Whether the frames are this object's to free.
    ///
    /// False for memory that was already somewhere -- the framebuffer belongs
    /// to the firmware and the hardware behind it, and handing it back to the
    /// page allocator would be handing out the display.
    owned: bool,
}

impl MemoryObject {
    /// Allocate `size` bytes, rounded up to a page, and zero them.
    ///
    /// Zeroed because the frames have been somewhere: handing a process pages
    /// still holding another process's data would be a way of reading memory
    /// nobody granted.
    pub fn new(size: usize) -> Option<Arc<Self>> {
        if size == 0 || size > MAX_MEMORY_OBJECT {
            return None;
        }
        let pages = size.div_ceil(PAGE_SIZE);

        let mut frames = Vec::with_capacity(pages);
        for _ in 0..pages {
            let Some(frame) = crate::memory::allocate_frame() else {
                // Give back what was taken. A partial object would be a leak
                // the caller could not have known about.
                for frame in frames {
                    // SAFETY: each was allocated here and never mapped.
                    unsafe { crate::memory::free_frame(frame) };
                }
                return None;
            };
            // SAFETY: just allocated, so nothing else refers to it, and the
            // direct map reaches every frame.
            unsafe {
                core::ptr::write_bytes(
                    nexus_abi::layout::phys_to_virt(frame) as *mut u8,
                    0,
                    PAGE_SIZE,
                );
            }
            frames.push(frame);
        }

        OBJECTS_CREATED.fetch_add(1, Ordering::Relaxed);
        Some(Arc::new(Self {
            frames,
            size,
            owned: true,
        }))
    }

    /// Describe memory that already exists, without taking ownership of it.
    ///
    /// The framebuffer is the reason this exists: it is physical memory the
    /// firmware chose, and a process that maps it is looking at the display
    /// rather than at pages the allocator handed out. Freeing those frames when
    /// the last handle went would hand the screen to whatever asked next.
    ///
    /// # Safety
    ///
    /// `base` must be a page-aligned physical region of at least `size` bytes
    /// that stays valid for the life of the system, and that the caller is
    /// entitled to expose to a process.
    pub unsafe fn borrowed(base: u64, size: usize) -> Option<Arc<Self>> {
        if size == 0 || !base.is_multiple_of(PAGE_SIZE as u64) {
            return None;
        }
        let pages = size.div_ceil(PAGE_SIZE);
        let frames = (0..pages)
            .map(|index| base + (index * PAGE_SIZE) as u64)
            .collect();

        OBJECTS_CREATED.fetch_add(1, Ordering::Relaxed);
        Some(Arc::new(Self {
            frames,
            size,
            owned: false,
        }))
    }

    /// Bytes the object holds.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Pages it occupies.
    #[must_use]
    pub fn pages(&self) -> usize {
        self.frames.len()
    }

    /// The frame backing page `index`.
    #[must_use]
    pub fn frame(&self, index: usize) -> Option<u64> {
        self.frames.get(index).copied()
    }
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        // The last handle has gone, and with it every mapping: an address space
        // that mapped these frames marked them as not its own, so nothing else
        // will free them.
        if self.owned {
            for &frame in &self.frames {
                // SAFETY: no mapping refers to them any more.
                unsafe { crate::memory::free_frame(frame) };
            }
        }
        OBJECTS_DESTROYED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Bytes in a page, as this module needs it.
const PAGE_SIZE: usize = 4096;

/// Shared memory objects created and destroyed.
static OBJECTS_CREATED: AtomicU64 = AtomicU64::new(0);
static OBJECTS_DESTROYED: AtomicU64 = AtomicU64::new(0);

/// Shared memory objects created and destroyed since boot.
#[must_use]
pub fn memory_statistics() -> (u64, u64) {
    (
        OBJECTS_CREATED.load(Ordering::Relaxed),
        OBJECTS_DESTROYED.load(Ordering::Relaxed),
    )
}

/// A kernel object a handle can refer to.
///
/// An enum rather than a trait object: the set is closed and small, and a match
/// that must be updated when a kind is added is worth more here than dynamic
/// dispatch.
#[derive(Clone)]
pub enum Object {
    /// One end of a channel.
    Channel(Arc<Endpoint>),
    /// Memory more than one process can map.
    Memory(Arc<MemoryObject>),
    /// An open file or directory in the system's filesystem.
    ///
    /// A directory handle is the authority to reach what is under it, and
    /// nothing else: there is no call that takes a path. A file handle is the
    /// authority to read or write that one file.
    Node(Arc<crate::fs::store::Node>),
    /// A process that was started, and what became of it.
    ///
    /// The completion rather than the process, so that remembering a program
    /// that has finished costs a name and a number rather than the address
    /// space it was running in.
    Process(Arc<crate::process::Completion>),
    /// Somewhere to wait for whichever of several things happens first.
    WaitSet(Arc<crate::waitset::WaitSet>),
}

impl Object {
    /// A short name, for diagnostics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Channel(_) => "channel",
            Self::Memory(_) => "memory",
            Self::Node(node) => {
                if node.is_directory() {
                    "directory"
                } else {
                    "file"
                }
            }
            Self::Process(_) => "process",
            Self::WaitSet(_) => "wait set",
        }
    }
}

/// An object together with what its holder may do to it.
#[derive(Clone)]
pub struct Handle {
    pub object: Object,
    pub rights: Rights,
}

/// A process's handles.
///
/// The numbers are per-process and mean nothing outside it. They are never
/// reused within a table, so a handle closed and then used again is reported as
/// a bad handle rather than silently naming whatever took its place.
pub struct HandleTable {
    entries: IrqSpinLock<BTreeMap<u32, Handle>>,
    next: AtomicU32,
}

/// Why a handle could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    /// No such handle in this process.
    NotFound,
    /// The handle does not carry the rights the operation needs.
    Denied,
    /// The handle names the other kind of object.
    ///
    /// Constructible now that there are two kinds. A call that asks for a
    /// channel and is handed memory is a mistake worth naming rather than one
    /// to report as a bad handle.
    WrongKind,
}

impl core::fmt::Display for HandleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NotFound => "no such handle",
            Self::Denied => "the handle does not carry that right",
            Self::WrongKind => "the handle names the other kind of object",
        })
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: IrqSpinLock::new(BTreeMap::new()),
            // Handle zero is never issued, so a zeroed variable is not a valid
            // handle by accident.
            next: AtomicU32::new(1),
        }
    }

    /// One whose handle numbers begin at `first`.
    ///
    /// For a process running a foreign interface that has already spoken for
    /// some of the low numbers. A Linux program's file descriptors 0, 1 and 2
    /// are its standard input, output and error; its descriptors are its
    /// handles, so its handles must not start at 1, or its first `open` would
    /// return the number every `printf` writes to.
    #[must_use]
    pub fn starting_at(first: u32) -> Self {
        let table = Self::new();
        table.next.store(first, Ordering::Relaxed);
        table
    }

    /// Add `object` with `rights`, returning the handle.
    pub fn insert(&self, object: Object, rights: Rights) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().insert(id, Handle { object, rights });
        id
    }

    /// The channel endpoint `id` names, if it names one and carries `needed`.
    pub fn channel(&self, id: u32, needed: Rights) -> Result<Arc<Endpoint>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Channel(endpoint) => Ok(Arc::clone(endpoint)),
            _ => Err(HandleError::WrongKind),
        }
    }

    /// The memory object `id` names, if it names one and carries `needed`.
    pub fn memory(&self, id: u32, needed: Rights) -> Result<Arc<MemoryObject>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Memory(memory) => Ok(Arc::clone(memory)),
            _ => Err(HandleError::WrongKind),
        }
    }

    /// The open file or directory `id` names, if it names one and carries
    /// `needed`.
    pub fn node(
        &self,
        id: u32,
        needed: Rights,
    ) -> Result<Arc<crate::fs::store::Node>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Node(node) => Ok(Arc::clone(node)),
            _ => Err(HandleError::WrongKind),
        }
    }

    /// The process `id` names, if it names one and carries `needed`.
    pub fn process(
        &self,
        id: u32,
        needed: Rights,
    ) -> Result<Arc<crate::process::Completion>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Process(completion) => Ok(Arc::clone(completion)),
            _ => Err(HandleError::WrongKind),
        }
    }

    /// The wait set `id` names, if it names one and carries `needed`.
    pub fn wait_set(
        &self,
        id: u32,
        needed: Rights,
    ) -> Result<Arc<crate::waitset::WaitSet>, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::WaitSet(set) => Ok(Arc::clone(set)),
            _ => Err(HandleError::WrongKind),
        }
    }

    /// The thing `id` names, if it is something a wait set can watch.
    ///
    /// A channel or a process. Memory and directories are never not ready, so
    /// watching one would be a program waiting for something that has already
    /// happened and will not happen again -- which reads as a hang and is one.
    pub fn watchable(
        &self,
        id: u32,
        needed: Rights,
    ) -> Result<crate::waitset::Watched, HandleError> {
        let entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        match &handle.object {
            Object::Channel(endpoint) => Ok(crate::waitset::Watched::Channel(Arc::clone(endpoint))),
            Object::Process(completion) => {
                Ok(crate::waitset::Watched::Process(Arc::clone(completion)))
            }
            _ => Err(HandleError::WrongKind),
        }
    }

    /// Another handle to the same object, carrying no more than this one.
    ///
    /// What a program needs to hand something on and keep it. Without it, a
    /// process that gives a client a buffer has given it away: the object dies
    /// with the client's handle, and the frames it was mapping go back to the
    /// allocator underneath whoever else still had them mapped.
    ///
    /// Rights can only be dropped, never gained. A duplicate that could add one
    /// would make every handle equal to the most powerful handle in the system,
    /// which is the whole of what a capability is, undone in one call.
    pub fn duplicate(&self, id: u32, rights: Rights) -> Result<u32, HandleError> {
        let object = {
            let entries = self.entries.lock();
            let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
            if !handle.rights.contains(rights) {
                return Err(HandleError::Denied);
            }
            handle.object.clone()
        };
        Ok(self.insert(object, rights))
    }

    /// What `id` may be used for.
    pub fn rights(&self, id: u32) -> Result<Rights, HandleError> {
        self.entries
            .lock()
            .get(&id)
            .map(|handle| handle.rights)
            .ok_or(HandleError::NotFound)
    }

    /// Remove `id` and return it, for handing to someone else.
    ///
    /// Moving, not copying. The sender gives it up at the moment the message is
    /// built, so there is never an instant where two processes both hold it and
    /// the transfer could be observed half-done.
    pub fn take(&self, id: u32, needed: Rights) -> Result<Handle, HandleError> {
        let mut entries = self.entries.lock();
        let handle = entries.get(&id).ok_or(HandleError::NotFound)?;
        if !handle.rights.contains(needed) {
            return Err(HandleError::Denied);
        }
        Ok(entries.remove(&id).expect("the handle was just found"))
    }

    /// Put a handle into this table, returning the number it now answers to.
    ///
    /// Used both for a handle arriving in a message and for one handed back
    /// after a send that could not be completed. The number is always a new
    /// one: a handle that left and came back is not the same handle, and
    /// reusing its old number would name it to a caller that had already been
    /// told it was gone.
    pub fn restore(&self, handle: Handle) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().insert(id, handle);
        id
    }

    /// Drop `id`, releasing its reference to the object.
    ///
    /// The last handle to an endpoint going away is what tells the other end
    /// the channel is closed, so this is not merely bookkeeping.
    pub fn close(&self, id: u32) -> Result<(), HandleError> {
        self.entries
            .lock()
            .remove(&id)
            .map(|_| ())
            .ok_or(HandleError::NotFound)
    }

    /// Close every handle, and say how many there were.
    ///
    /// For a process that has finished. Until this existed, a program's handles
    /// were released when its thread was *reaped*, which happens on a five
    /// second timer in the monitor thread -- so the channel a program wrote its
    /// output to stayed open for up to five seconds after the program had
    /// exited, and a shell reading that channel waited all of it. Every command
    /// in the terminal paused after finishing, for no reason a person could
    /// see. Exiting closes what you hold; that is what it means everywhere
    /// else, and it is what it means here now.
    ///
    /// The entries are taken out under the lock and dropped *outside* it, for
    /// the reason `sched::reap_finished` gives at greater length: dropping an
    /// endpoint wakes whoever was waiting on the other end, and doing that
    /// while holding this lock is a deadlock waiting for a second process.
    pub fn close_all(&self) -> usize {
        let taken: Vec<Handle> = {
            let mut entries = self.entries.lock();
            // `core::mem::take` rather than `drain`: a `BTreeMap` behind this
            // lock guard has no `drain`, and swapping an empty map in leaves
            // the table usable if anything reaches it afterwards.
            core::mem::take(&mut *entries).into_values().collect()
        };
        let count = taken.len();
        drop(taken);
        count
    }

    /// How many handles are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// A line per handle, for diagnostics.
    pub fn describe(&self) -> Vec<(u32, &'static str, u32)> {
        self.entries
            .lock()
            .iter()
            .map(|(id, handle)| (*id, handle.object.kind(), handle.rights.bits()))
            .collect()
    }
}

/// Channels created since boot.
static CHANNELS_CREATED: AtomicU64 = AtomicU64::new(0);
/// Messages written.
static MESSAGES_SENT: AtomicU64 = AtomicU64::new(0);
/// Messages read.
static MESSAGES_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// Channels created, messages sent, messages received.
#[must_use]
pub fn statistics() -> (u64, u64, u64) {
    (
        CHANNELS_CREATED.load(Ordering::Relaxed),
        MESSAGES_SENT.load(Ordering::Relaxed),
        MESSAGES_RECEIVED.load(Ordering::Relaxed),
    )
}
