//! The network card: legacy virtio-net over a PCI I/O port window.
//!
//! The same transport as the disk, chosen for the same reason -- the modern
//! virtio transport reaches its registers through memory-mapped capability
//! structures in configuration space, and the legacy one is a window of ports.
//! For a first driver that is a great deal less code between "the device is
//! there" and "a frame went out".
//!
//! # What a network card is, in virtio terms
//!
//! Two queues instead of the disk's one. Queue 0 carries frames *in* and queue
//! 1 carries them *out*, and the difference between them is not a flag on a
//! request: it is which queue the buffer was put on. There is no request header
//! saying what to do, because there is only one thing to do with a frame.
//!
//! Receiving is therefore backwards from a disk. Nobody asks for a frame. The
//! driver hands the device a pile of empty buffers in advance and the device
//! fills them in whenever something arrives; a card with no buffers posted is a
//! card that drops everything. So the receive queue is filled at bring-up and
//! every buffer is handed straight back after it has been read.
//!
//! # The header
//!
//! Every buffer, in both directions, begins with a `virtio_net_hdr`. It carries
//! checksum and segmentation offload information that this driver neither
//! offers nor accepts -- it is written as zeroes going out and ignored coming
//! in. It cannot be omitted: the device reads or writes it regardless, and a
//! driver that left it out would have the device treat the first bytes of the
//! Ethernet frame as the header.
//!
//! Ten bytes, not twelve. The twelfth and eleventh are a *number of buffers*
//! field that exists only when `VIRTIO_NET_F_MRG_RXBUF` has been negotiated,
//! and this driver does not negotiate it -- one frame per buffer is what a
//! two-kilobyte buffer and a 1514-byte MTU mean anyway.
//!
//! Getting this wrong is silent in the worst way. The card takes the frame, the
//! device sends it, and nothing anywhere reports an error: the frame on the
//! wire is simply shifted by two bytes, so its destination address is
//! nonsense and no reply ever comes. That is what the first version of this
//! driver did, and what it looked like was a DHCP server that did not answer.
//!
//! # Who runs the stack
//!
//! Not the interrupt. The handler acknowledges the device and wakes a thread;
//! everything that looks at a frame -- ARP, IP, UDP, and the replies they send
//! -- happens in that thread, where it can take locks, allocate, and send
//! frames back out. An interrupt handler that did any of that would be an
//! interrupt handler that can block.

use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use nexus_abi::layout;

use crate::arch::io::{inb, inl, inw, outb, outl, outw};
use crate::drivers::pci::{BaseAddress, Device};
use crate::kprintln;
use crate::memory;
use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

/// Vendor identifier every virtio device reports.
const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Subsystem identifier a virtio network device reports.
const SUBSYSTEM_NET: u16 = 1;

/// Registers, as offsets from the port window's base. The transport's, so the
/// same as the block device's.
mod register {
    pub const DEVICE_FEATURES: u16 = 0x00;
    pub const DRIVER_FEATURES: u16 = 0x04;
    pub const QUEUE_ADDRESS: u16 = 0x08;
    pub const QUEUE_SIZE: u16 = 0x0C;
    pub const QUEUE_SELECT: u16 = 0x0E;
    pub const QUEUE_NOTIFY: u16 = 0x10;
    pub const DEVICE_STATUS: u16 = 0x12;
    pub const ISR: u16 = 0x13;
    pub const CONFIG: u16 = 0x14;
}

/// Status bits, written in order during bring-up.
mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FAILED: u8 = 0x80;
}

/// Descriptor flags.
mod descriptor {
    /// The *device* writes this buffer; without it the device reads it.
    pub const WRITE: u16 = 2;
}

/// Features this driver asks for.
mod feature {
    /// The device's configuration space carries its MAC address.
    ///
    /// The only one claimed. Every other feature changes the layout or the
    /// meaning of something, and a driver that accepted one it does not
    /// implement would be agreeing to a protocol it cannot speak. This one adds
    /// six bytes of configuration and nothing else.
    pub const MAC: u32 = 1 << 5;
}

/// Bytes of `virtio_net_hdr` in front of every frame, in both directions.
///
/// Ten, because `VIRTIO_NET_F_MRG_RXBUF` is not negotiated. See the note at the
/// top of this file: two bytes out and nothing reports it.
const HEADER: usize = 10;

/// The largest Ethernet frame this driver will carry.
///
/// No jumbo frames: 1500 bytes of payload plus fourteen of header. A driver
/// that posted larger buffers would be claiming an MTU the rest of the system
/// does not use.
pub const MAX_FRAME: usize = 1514;

/// Bytes set aside per buffer: the header, the frame, and room to spare.
const BUFFER: usize = 2048;

/// How many frames the card may have in hand at once.
///
/// Enough that a burst is not dropped while the thread that drains them is
/// getting to the processor, and small enough to be one contiguous allocation.
const RECEIVE_BUFFERS: u16 = 32;
/// And going out. Fewer, because a transmit is finished with almost at once.
const TRANSMIT_BUFFERS: u16 = 8;

/// Queue indices, which is the only thing that says which way a frame goes.
const RECEIVE_QUEUE: u16 = 0;
const TRANSMIT_QUEUE: u16 = 1;

/// One entry of the descriptor table.
#[repr(C)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

/// One used-ring entry: which descriptor chain, and how many bytes are in it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Used {
    index: u32,
    length: u32,
}

/// What can go wrong bringing the card up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// No virtio network device on the bus.
    NoDevice,
    /// The device is memory mapped, and this driver speaks ports.
    NotPortMapped,
    /// A queue this driver needs is missing or larger than it can hold.
    QueueSize(u16),
    /// The frame allocator could not find room for the rings or the buffers.
    OutOfMemory,
    /// A frame longer than this driver will carry.
    TooLong(usize),
    /// The device did not take a frame within the time allowed.
    Timeout,
}

impl core::fmt::Display for NetError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => write!(formatter, "no virtio network device"),
            Self::NotPortMapped => write!(formatter, "the device is memory mapped"),
            Self::QueueSize(size) => write!(formatter, "a queue of {size} cannot be used"),
            Self::OutOfMemory => write!(formatter, "not enough memory for the queues"),
            Self::TooLong(bytes) => write!(formatter, "a frame of {bytes} bytes is too long"),
            Self::Timeout => write!(formatter, "the device did not take the frame"),
        }
    }
}

/// One virtqueue, as this driver uses it.
#[derive(Clone, Copy)]
struct Queue {
    /// Physical address of the whole ring.
    physical: u64,
    /// Descriptors it has.
    size: u16,
    /// Offsets of the available and used rings from the ring's base.
    available_offset: usize,
    used_offset: usize,
    /// Physical address of the first buffer, which are contiguous.
    buffers_physical: u64,
    /// The last used index this driver has looked at.
    seen: u16,
}

impl Queue {
    /// Where the descriptor table is, as an address this kernel can write.
    fn descriptors(&self) -> *mut Descriptor {
        layout::phys_to_virt(self.physical) as *mut Descriptor
    }

    /// Where the available ring's flags word is.
    fn available(&self) -> *mut u16 {
        (layout::phys_to_virt(self.physical) + self.available_offset as u64) as *mut u16
    }

    /// Where the used ring's flags word is.
    fn used(&self) -> *mut u16 {
        (layout::phys_to_virt(self.physical) + self.used_offset as u64) as *mut u16
    }

    /// Where buffer `index` starts, as an address this kernel can write.
    fn buffer(&self, index: u16) -> u64 {
        layout::phys_to_virt(self.buffers_physical + index as u64 * BUFFER as u64)
    }

    /// And its physical address, which is what the device is told.
    fn buffer_physical(&self, index: u16) -> u64 {
        self.buffers_physical + index as u64 * BUFFER as u64
    }

    /// Offer descriptor `index` to the device.
    ///
    /// # Safety
    ///
    /// The descriptor must already describe a buffer the device may use.
    unsafe fn offer(&self, index: u16) {
        // SAFETY: the ring is mapped through the direct map and `index` is
        // below the queue size, which bring-up checked.
        unsafe {
            let available = self.available();
            let at = available.add(1).read_volatile();
            available
                .add(2 + (at % self.size) as usize)
                .write_volatile(index);
            // The device must see the ring entry before it sees the new index,
            // or it will follow a slot that has not been written yet.
            fence(Ordering::SeqCst);
            available.add(1).write_volatile(at.wrapping_add(1));
            fence(Ordering::SeqCst);
        }
    }

    /// The device's used index.
    ///
    /// # Safety
    ///
    /// The queue must have been set up.
    unsafe fn used_index(&self) -> u16 {
        // SAFETY: as above.
        unsafe { self.used().add(1).read_volatile() }
    }

    /// The used entry at `at`.
    ///
    /// # Safety
    ///
    /// The queue must have been set up.
    unsafe fn used_entry(&self, at: u16) -> Used {
        // SAFETY: the used ring begins two words past its flags and index, and
        // its entries are eight bytes each.
        unsafe {
            let entries = self.used().add(2) as *const Used;
            entries.add((at % self.size) as usize).read_volatile()
        }
    }
}

/// A ready card.
#[derive(Clone, Copy)]
struct Card {
    port: u16,
    receive: Queue,
    transmit: Queue,
    /// The address the card answers to.
    mac: [u8; 6],
}

// SAFETY: every field is a plain number, and all access goes through the lock
// below; the device is driven from one place at a time on purpose.
unsafe impl Send for Card {}

/// The one card, once it has been found.
static CARD: IrqSpinLock<Option<Card>> = IrqSpinLock::new(None);

/// Threads waiting for something to arrive.
static ARRIVED: WaitQueue = WaitQueue::new();
/// Whether a transmit is in flight, and who is waiting for one to finish.
static SENDING: AtomicBool = AtomicBool::new(false);
static SENT: WaitQueue = WaitQueue::new();

/// Interrupts the device has raised.
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
/// Whether waiting is done by blocking. False until an interrupt has arrived.
static BLOCKING: AtomicBool = AtomicBool::new(false);
/// The interrupt line firmware assigned the device, for whoever routes it.
static INTERRUPT_LINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Frames in and out, and what was dropped.
static RECEIVED: AtomicU64 = AtomicU64::new(0);
static TRANSMITTED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Bring up the first virtio network device on the bus.
///
/// # Safety
///
/// Call once, after PCI enumeration and with the frame allocator running.
pub unsafe fn init(devices: &[Device]) -> Result<[u8; 6], NetError> {
    let device = devices
        .iter()
        .find(|device| {
            device.vendor == VIRTIO_VENDOR
                && (device.subsystem == SUBSYSTEM_NET || device.device == 0x1000)
        })
        .ok_or(NetError::NoDevice)?;

    // SAFETY: the device answered enumeration, so its configuration space is
    // readable, and this is the only driver touching it.
    unsafe { device.enable() };
    // And this one does wait on its interrupt, so it asks for it back.
    // SAFETY: this is that device's driver and its handler is installed.
    unsafe { device.take_interrupts() };

    // SAFETY: as above.
    let port = match unsafe { device.base_address(0) } {
        BaseAddress::Port(port) => port,
        BaseAddress::Memory { .. } => return Err(NetError::NotPortMapped),
    };

    // SAFETY: the window belongs to this device, which nothing else drives.
    unsafe {
        outb(port + register::DEVICE_STATUS, 0);
        outb(port + register::DEVICE_STATUS, status::ACKNOWLEDGE);
        outb(
            port + register::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER,
        );

        let offered = inl(port + register::DEVICE_FEATURES);
        // Only what is understood, and only if it is offered: claiming a
        // feature the device did not offer is a driver lying about what it can
        // do, and the device is entitled to fail bring-up for it.
        let agreed = offered & feature::MAC;
        outl(port + register::DRIVER_FEATURES, agreed);

        let receive = match set_up_queue(port, RECEIVE_QUEUE, RECEIVE_BUFFERS) {
            Ok(queue) => queue,
            Err(error) => {
                outb(port + register::DEVICE_STATUS, status::FAILED);
                return Err(error);
            }
        };
        let transmit = match set_up_queue(port, TRANSMIT_QUEUE, TRANSMIT_BUFFERS) {
            Ok(queue) => queue,
            Err(error) => {
                outb(port + register::DEVICE_STATUS, status::FAILED);
                return Err(error);
            }
        };

        // The address, if the device agreed to tell us. Without that feature
        // there is nothing to read and the card has no address of its own --
        // which this driver reports rather than inventing one, because a made-up
        // MAC is a machine that works until two of them meet.
        let mut mac = [0u8; 6];
        if agreed & feature::MAC != 0 {
            for (index, byte) in mac.iter_mut().enumerate() {
                *byte = inb(port + register::CONFIG + index as u16);
            }
        }

        outb(
            port + register::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK,
        );

        // Every receive buffer, handed over at once. A card with none posted
        // drops what arrives, and nothing asks for a frame the way something
        // asks for a sector -- so they go now, before anything can arrive.
        for index in 0..RECEIVE_BUFFERS {
            receive.offer(index);
        }
        outw(port + register::QUEUE_NOTIFY, RECEIVE_QUEUE);

        *CARD.lock() = Some(Card {
            port,
            receive,
            transmit,
            mac,
        });
        INTERRUPT_LINE.store(u64::from(device.interrupt_line), Ordering::Release);

        kprintln!(
            "[net ] virtio card at {}: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, \
             queues of {} and {}, features {offered:#010x}",
            device.address,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            receive.size,
            transmit.size
        );

        Ok(mac)
    }
}

/// Set one queue up: ask how large it is, find memory for it, and describe the
/// buffers to it.
///
/// # Safety
///
/// The device must be past `DRIVER` and not yet at `DRIVER_OK`.
unsafe fn set_up_queue(port: u16, which: u16, buffers: u16) -> Result<Queue, NetError> {
    // SAFETY: the caller has the device in the right state and owns the window.
    unsafe {
        outw(port + register::QUEUE_SELECT, which);
        let size = inw(port + register::QUEUE_SIZE);
        if size == 0 || size < buffers {
            return Err(NetError::QueueSize(size));
        }

        let (available_offset, used_offset, bytes) = queue_layout(size);
        let order = order_for(bytes);
        let physical = memory::allocate_block(order).ok_or(NetError::OutOfMemory)?;
        core::ptr::write_bytes(
            layout::phys_to_virt(physical) as *mut u8,
            0,
            1usize << (order + 12),
        );

        let buffer_bytes = buffers as usize * BUFFER;
        let buffer_order = order_for(buffer_bytes);
        let Some(buffers_physical) = memory::allocate_block(buffer_order) else {
            memory::free_block(physical, order);
            return Err(NetError::OutOfMemory);
        };
        core::ptr::write_bytes(
            layout::phys_to_virt(buffers_physical) as *mut u8,
            0,
            1usize << (buffer_order + 12),
        );

        outl(port + register::QUEUE_ADDRESS, (physical >> 12) as u32);

        let queue = Queue {
            physical,
            size,
            available_offset,
            used_offset,
            buffers_physical,
            seen: 0,
        };

        // One descriptor per buffer, described once and reused for ever. The
        // addresses never change, so the only thing that moves is which of them
        // the device has been offered.
        let descriptors = queue.descriptors();
        for index in 0..buffers {
            descriptors.add(index as usize).write_volatile(Descriptor {
                address: queue.buffer_physical(index),
                length: BUFFER as u32,
                // The receive queue is written by the device; the transmit
                // queue is read by it. That flag is the whole difference.
                flags: if which == RECEIVE_QUEUE {
                    descriptor::WRITE
                } else {
                    0
                },
                next: 0,
            });
        }

        Ok(queue)
    }
}

/// Where the three parts of a queue of `size` descriptors sit, and how many
/// bytes the whole thing needs.
///
/// The used ring starts on a 4096-byte boundary measured from the start of the
/// queue -- the legacy layout defines that padding, and a used ring one byte
/// out of place is a device writing completions into the available ring.
fn queue_layout(size: u16) -> (usize, usize, usize) {
    const ALIGNMENT: usize = 4096;
    let size = size as usize;

    let descriptors = 16 * size;
    let available = descriptors;
    let available_bytes = 2 + 2 + 2 * size + 2;

    let used = (available + available_bytes).div_ceil(ALIGNMENT) * ALIGNMENT;
    let used_bytes = 2 + 2 + 8 * size + 2;

    (available, used, used + used_bytes)
}

/// Smallest buddy order whose block holds `bytes`.
fn order_for(bytes: usize) -> usize {
    let mut order = 0;
    while (1usize << (order + 12)) < bytes {
        order += 1;
    }
    order
}

/// Send one frame.
///
/// Blocks until the card has taken it, which is not the same as it having gone
/// anywhere: a network card acknowledges that it has the frame, and what
/// happens after that is the network's business and not something the card can
/// report on.
pub fn transmit(frame: &[u8]) -> Result<(), NetError> {
    if frame.len() > MAX_FRAME {
        return Err(NetError::TooLong(frame.len()));
    }

    acquire();
    let result = one_transmit(frame);
    release();
    result
}

/// Take the transmit path, blocking until it is free.
fn acquire() {
    loop {
        let seen = SENT.generation();
        if !SENDING.swap(true, Ordering::Acquire) {
            return;
        }
        if crate::sched::current_id().is_none() {
            core::hint::spin_loop();
            continue;
        }
        SENT.wait_if_unchanged(seen);
    }
}

/// Give it back.
fn release() {
    SENDING.store(false, Ordering::Release);
    SENT.wake_one();
}

/// How long a transmit may spin before the device is called broken.
const COMPLETION_SPINS: u32 = 50_000_000;

/// One frame, with the transmit path already held.
fn one_transmit(frame: &[u8]) -> Result<(), NetError> {
    let card = {
        let guard = CARD.lock();
        *guard.as_ref().ok_or(NetError::NoDevice)?
    };
    let queue = card.transmit;

    // Buffer zero every time: this path is held by one caller and the frame is
    // finished with before it is released, so there is nothing for the others
    // to hold. They exist because the device may not have consumed the previous
    // one when the next is offered -- the used ring is what says otherwise.
    let buffer = queue.buffer(0);

    // SAFETY: the buffer is mapped through the direct map, is BUFFER bytes, and
    // the frame is at most MAX_FRAME which is smaller than BUFFER - HEADER.
    unsafe {
        core::ptr::write_bytes(buffer as *mut u8, 0, HEADER);
        core::ptr::copy_nonoverlapping(
            frame.as_ptr(),
            (buffer + HEADER as u64) as *mut u8,
            frame.len(),
        );

        // The length is the frame plus its header, not the buffer: the device
        // sends exactly what it is told the descriptor covers.
        queue.descriptors().write_volatile(Descriptor {
            address: queue.buffer_physical(0),
            length: (HEADER + frame.len()) as u32,
            flags: 0,
            next: 0,
        });

        let before = queue.used_index();
        // Read before the notify: the device may finish and its interrupt may
        // run before the next line returns, and a waiter that read the counter
        // afterwards would sleep through the wake-up that already happened.
        let generation = SENT.generation();

        queue.offer(0);
        outw(card.port + register::QUEUE_NOTIFY, TRANSMIT_QUEUE);

        wait_for_send(&queue, before, generation)?;
    }

    TRANSMITTED.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Wait for the card to take the frame.
///
/// # Safety
///
/// The queue must be one this driver set up.
unsafe fn wait_for_send(queue: &Queue, before: u16, generation: u64) -> Result<(), NetError> {
    if BLOCKING.load(Ordering::Relaxed) && crate::sched::current_id().is_some() {
        let mut generation = generation;
        loop {
            // SAFETY: upheld by the caller.
            if unsafe { queue.used_index() } != before {
                return Ok(());
            }
            SENT.wait_if_unchanged(generation);
            generation = SENT.generation();
        }
    }

    let mut spins = 0u32;
    loop {
        // SAFETY: upheld by the caller.
        if unsafe { queue.used_index() } != before {
            return Ok(());
        }
        spins += 1;
        if spins >= COMPLETION_SPINS {
            return Err(NetError::Timeout);
        }
        core::hint::spin_loop();
    }
}

/// Take one frame the card has received, if there is one.
///
/// The frame is copied into `into` and the buffer is handed straight back to
/// the card. Returns how many bytes the frame was.
///
/// Copying rather than lending the buffer out is deliberate: a buffer that was
/// still on loan when the card wanted it back would be a receive queue that
/// shrinks every time something takes its time reading a frame.
pub fn receive(into: &mut [u8]) -> Option<usize> {
    let mut guard = CARD.lock();
    let card = guard.as_mut()?;
    let queue = &mut card.receive;

    // SAFETY: the queue was set up at bring-up and is mapped through the direct
    // map; `seen` only ever names entries the device has published.
    let (index, length) = unsafe {
        if queue.used_index() == queue.seen {
            return None;
        }
        fence(Ordering::SeqCst);
        let entry = queue.used_entry(queue.seen);
        queue.seen = queue.seen.wrapping_add(1);
        (entry.index as u16, entry.length as usize)
    };

    // The length includes the header the device wrote, which nothing above this
    // has any use for.
    let frame = length.saturating_sub(HEADER).min(MAX_FRAME);
    let taken = frame.min(into.len());
    if taken < frame {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }

    // SAFETY: `index` came out of the used ring, so it names a buffer this
    // driver posted, and the copy is bounded by both lengths.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (queue.buffer(index) + HEADER as u64) as *const u8,
            into.as_mut_ptr(),
            taken,
        );
        // Straight back to the card. A buffer kept back is a buffer the card
        // cannot fill, and thirty-two of those is a card that has stopped
        // receiving.
        queue.offer(index);
        outw(card.port + register::QUEUE_NOTIFY, RECEIVE_QUEUE);
    }

    RECEIVED.fetch_add(1, Ordering::Relaxed);
    Some(taken)
}

/// Whether a frame is waiting to be taken.
///
/// Public because the network thread waits on more than this card, and
/// something that waits on several things has to be able to ask each of them
/// whether it already has work. There is deliberately no `wait_for_frame` any
/// more: a thread that waited on this queue alone would sleep through a program
/// asking it to open a connection, which is the other half of its job.
#[must_use]
pub fn has_frame_waiting() -> bool {
    has_frame()
}

/// Whether a frame is waiting to be taken.
fn has_frame() -> bool {
    let guard = CARD.lock();
    let Some(card) = guard.as_ref() else {
        return false;
    };
    // SAFETY: the queue was set up at bring-up.
    unsafe { card.receive.used_index() != card.receive.seen }
}

/// The card has something to say.
///
/// Reading the interrupt status register acknowledges it at the device, and
/// that has to happen whether or not anyone is waiting: a level-triggered line
/// that is never acknowledged is an interrupt storm.
///
/// # Safety
///
/// Call only as the handler for this device's vector.
pub unsafe fn on_interrupt() {
    let port = {
        let guard = CARD.lock();
        match guard.as_ref() {
            Some(card) => card.port,
            None => return,
        }
    };

    // SAFETY: the window belongs to this device, and reading this register is
    // both how the protocol says to acknowledge and how it says whether this
    // device raised the line at all.
    let reason = unsafe { inb(port + register::ISR) };
    if reason == 0 {
        // Somebody else's interrupt on a shared pin. Not counted, because the
        // count is what decides whether this card's interrupt works -- and a
        // driver that counted the disk's interrupts as its own would switch to
        // blocking on the strength of a line it is not actually on.
        return;
    }
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);

    // One interrupt covers both queues -- the register says which, and it is
    // cheaper to wake both than to work it out and be wrong.
    ARRIVED.wake_all();
    SENT.wake_all();
    // And the network thread, which since it began answering programs waits on
    // a set rather than on this queue. Both are woken because both have
    // waiters: the thread is on one of them and whatever is sending a frame is
    // on the other.
    crate::net::service::signal();
}

/// Take the card's interrupt into use, if it has proved itself.
///
/// Returns true only on the pass that changes the answer, so the caller can say
/// so once. Until it does, waiting is done by polling: a driver that started
/// blocking on the strength of a routing call returning `Ok` would hang on the
/// first machine where the routing was wrong, and there is nothing to ask a
/// network card that would make it answer.
///
/// Asked repeatedly rather than once, because the first frames arrive during
/// bring-up while the driver is already polling in a tight loop -- they are
/// drained before their interrupt is serviced, and a driver that checked once
/// and gave up would poll for ever on a machine whose interrupt works.
pub fn adopt_interrupt() -> bool {
    if BLOCKING.load(Ordering::Relaxed) || INTERRUPTS.load(Ordering::Relaxed) == 0 {
        return false;
    }
    BLOCKING.store(true, Ordering::Release);
    true
}

/// Whether waiting is done by blocking yet.
pub fn is_blocking() -> bool {
    BLOCKING.load(Ordering::Relaxed)
}

/// The line firmware assigned the device, for whoever routes it.
pub fn interrupt_line() -> Option<u8> {
    let line = INTERRUPT_LINE.load(Ordering::Acquire);
    if line == u64::MAX {
        None
    } else {
        Some(line as u8)
    }
}

/// The address the card answers to.
pub fn address() -> Option<[u8; 6]> {
    CARD.lock().as_ref().map(|card| card.mac)
}

/// Whether a card was found.
pub fn is_present() -> bool {
    CARD.lock().is_some()
}

/// Frames in, frames out, frames dropped, and interrupts taken.
pub fn statistics() -> (u64, u64, u64, u64) {
    (
        RECEIVED.load(Ordering::Relaxed),
        TRANSMITTED.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
        INTERRUPTS.load(Ordering::Relaxed),
    )
}
