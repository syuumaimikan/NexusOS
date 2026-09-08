//! A virtio block device.
//!
//! The first storage NexusOS can read or write, and the first device it drives
//! by putting descriptors in memory and asking the hardware to come and fetch
//! them rather than by moving bytes through a port itself.
//!
//! # Legacy transport, on purpose
//!
//! Virtio has two PCI transports. The modern one advertises its registers
//! through capability structures in configuration space, each naming a memory
//! window and an offset; the legacy one puts every register in one I/O port
//! window at base address register zero. The legacy one is used here because it
//! is a great deal less code for a first block driver and reaches everything
//! this needs. Nothing above this module knows which it is, so the modern
//! transport is an addition rather than a rewrite when a device demands it.
//!
//! # How a request happens
//!
//! A virtqueue is three arrays the driver and the device share: descriptors
//! saying where the buffers are, an *available* ring the driver appends to, and
//! a *used* ring the device appends to. A block request is three descriptors
//! chained together — a header saying what and where, the data, and one byte
//! for the device to report on — and submitting it is appending the head of
//! that chain to the available ring and poking a port.
//!
//! # Waiting
//!
//! A request is submitted, and then the thread that made it *blocks*. The
//! device's interrupt is what wakes it. Between the two the thread is not on a
//! run queue, does not take a time slice, and does not appear in a scheduling
//! decision — where before it spun, holding a processor for the whole of a
//! seek on real hardware.
//!
//! The driver proves the interrupt before it relies on it. The first request
//! of the system's life is made the old way, spinning, and afterwards the
//! driver checks whether its interrupt handler ran at all. Only then does it
//! switch to blocking. A driver that assumed a routed interrupt would arrive
//! would hang the machine on the first firmware that routed it somewhere else,
//! and the failure would look like a disk that stopped answering.
//!
//! Falling back is not shameful and it is not silent: which mode the driver
//! settled into is printed at bring-up.

use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use nexus_abi::layout;

use crate::arch::io::{inl, inw, outb, outl, outw};
use crate::drivers::pci::{BaseAddress, Device};
use crate::kprintln;
use crate::memory;
use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;

/// Every virtio device answers to this vendor.
const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Subsystem identifier a virtio block device reports.
const SUBSYSTEM_BLOCK: u16 = 2;

/// Registers, as offsets from the port window's base.
mod register {
    /// What the device can do.
    pub const DEVICE_FEATURES: u16 = 0x00;
    /// What the driver agrees to.
    pub const DRIVER_FEATURES: u16 = 0x04;
    /// Physical page number of the selected queue.
    pub const QUEUE_ADDRESS: u16 = 0x08;
    /// How many descriptors the selected queue has.
    pub const QUEUE_SIZE: u16 = 0x0C;
    /// Which queue the registers above refer to.
    pub const QUEUE_SELECT: u16 = 0x0E;
    /// Written to tell the device a queue has something new.
    pub const QUEUE_NOTIFY: u16 = 0x10;
    /// How far through bring-up the driver has got.
    pub const DEVICE_STATUS: u16 = 0x12;
    /// Why the device raised an interrupt. Reading it acknowledges.
    pub const ISR: u16 = 0x13;
    /// First byte of the device's own configuration.
    pub const CONFIG: u16 = 0x14;
}

/// Status bits, written in order during bring-up.
mod status {
    /// The driver has noticed the device.
    pub const ACKNOWLEDGE: u8 = 1;
    /// The driver knows how to drive it.
    pub const DRIVER: u8 = 2;
    /// The driver is ready; the device may start.
    pub const DRIVER_OK: u8 = 4;
    /// Something went wrong and the driver has given up.
    pub const FAILED: u8 = 0x80;
}

/// Descriptor flags.
mod descriptor {
    /// The chain continues at `next`.
    pub const NEXT: u16 = 1;
    /// The *device* writes this buffer; without it the device reads it.
    pub const WRITE: u16 = 2;
}

/// Request types a block device understands.
mod request {
    /// Read from the disk into memory.
    pub const READ: u32 = 0;
    /// Write from memory to the disk.
    pub const WRITE: u32 = 1;
}

/// What the device writes into the status byte.
const STATUS_OK: u8 = 0;

/// Bytes in a disk sector, as virtio defines it regardless of the medium.
pub const SECTOR_SIZE: usize = 512;

/// One entry of the descriptor table.
#[repr(C)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

/// The header of a block request.
#[repr(C)]
struct RequestHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

/// Why an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// No virtio block device is present.
    NoDevice,
    /// The device is behind a memory window rather than ports, which the legacy
    /// transport does not use. Carries the window, because the number is the
    /// first thing anyone adding the modern transport will want.
    NotPortMapped { base: u64, sixty_four: bool },
    /// There was no memory for the queue or the bounce buffer.
    OutOfMemory,
    /// The device reported a queue this driver cannot fit.
    QueueTooLarge(u16),
    /// The request named a sector past the end of the disk.
    OutOfRange,
    /// The device never completed the request.
    Timeout,
    /// The device reported a failure.
    Failed(u8),
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no virtio block device"),
            Self::NotPortMapped { base, sixty_four } => write!(
                f,
                "the device is behind a {}-bit memory window at {base:#x}, not a port window",
                if *sixty_four { 64 } else { 32 }
            ),
            Self::OutOfMemory => f.write_str("out of memory for the virtqueue"),
            Self::QueueTooLarge(size) => write!(f, "the device wants a queue of {size}"),
            Self::OutOfRange => f.write_str("the sector is past the end of the disk"),
            Self::Timeout => f.write_str("the device never completed the request"),
            Self::Failed(status) => write!(f, "the device reported status {status}"),
        }
    }
}

/// Largest queue this driver will build.
///
/// The queue has to be physically contiguous, and the allocator hands out
/// power-of-two blocks; this bounds the block at 64 KiB. A device asking for
/// more is refused rather than quietly given a queue it will overrun.
const MAX_QUEUE: u16 = 256;

/// A ready device.
#[derive(Clone, Copy)]
struct Disk {
    /// Base of the port window.
    port: u16,
    /// Descriptors the device has.
    queue_size: u16,
    /// Physical address of the queue.
    queue_physical: u64,
    /// Physical address of the request header, data and status scratch.
    scratch_physical: u64,
    /// Sectors on the disk.
    capacity: u64,
    /// Where each part of the queue starts, as an offset from its base.
    available_offset: usize,
    used_offset: usize,
}

// SAFETY: every field is a plain number, and all access goes through the lock
// below; the device is driven from one place at a time on purpose.
unsafe impl Send for Disk {}

/// The one disk, once it has been found.
static DISK: IrqSpinLock<Option<Disk>> = IrqSpinLock::new(None);

/// Whether a request is in flight.
///
/// There is one scratch page and one descriptor chain, so there is one request
/// at a time. This is the gate, and it is a *sleeping* one: a thread that finds
/// the driver busy blocks rather than spins, because the thing it would be
/// spinning on is a disk.
static BUSY: AtomicBool = AtomicBool::new(false);
/// Threads waiting for the gate.
static FREE: WaitQueue = WaitQueue::new();
/// Threads waiting for the device to finish.
static DONE: WaitQueue = WaitQueue::new();

/// Interrupts the device has raised.
///
/// Counted rather than merely observed, because the count is what says the
/// interrupt is really arriving: a driver that switched to blocking on the
/// strength of a routing call returning `Ok` would hang on the first machine
/// where the routing was wrong.
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
/// Whether requests wait by blocking. False until the interrupt has proved
/// itself once.
static BLOCKING: AtomicBool = AtomicBool::new(false);
/// The interrupt line firmware assigned the device, for whoever routes it.
static INTERRUPT_LINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Sectors read and written, for diagnostics.
static SECTORS_READ: AtomicU64 = AtomicU64::new(0);
static SECTORS_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Bring up the first virtio block device on the bus.
///
/// # Safety
///
/// Call once, after PCI enumeration and with the heap and frame allocator
/// running.
pub unsafe fn init(devices: &[Device]) -> Result<u64, BlockError> {
    let device = devices
        .iter()
        .find(|device| {
            device.vendor == VIRTIO_VENDOR
                && (device.subsystem == SUBSYSTEM_BLOCK || device.device == 0x1001)
        })
        .ok_or(BlockError::NoDevice)?;

    // SAFETY: the device answered enumeration, so its configuration space is
    // readable, and this is the only driver touching it.
    unsafe { device.enable() };

    // SAFETY: as above.
    let port = match unsafe { device.base_address(0) } {
        BaseAddress::Port(port) => port,
        BaseAddress::Memory { base, sixty_four } => {
            return Err(BlockError::NotPortMapped { base, sixty_four })
        }
    };

    // SAFETY: the window belongs to this device, which nothing else drives.
    unsafe {
        // Reset, then walk up the status ladder. The order is the protocol: the
        // device may not touch a queue before `DRIVER_OK`, and the driver may
        // not read the device's configuration before `DRIVER`.
        outb(port + register::DEVICE_STATUS, 0);
        outb(port + register::DEVICE_STATUS, status::ACKNOWLEDGE);
        outb(
            port + register::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER,
        );

        // No optional feature is claimed. Every one of them changes the layout
        // or the semantics of something, and a driver that accepted a feature
        // it did not implement would be agreeing to a protocol it does not
        // speak. The bare device does what this needs.
        let offered = inl(port + register::DEVICE_FEATURES);
        outl(port + register::DRIVER_FEATURES, 0);

        // Queue zero is the only one a block device has.
        outw(port + register::QUEUE_SELECT, 0);
        let queue_size = inw(port + register::QUEUE_SIZE);
        if queue_size == 0 || queue_size > MAX_QUEUE {
            outb(port + register::DEVICE_STATUS, status::FAILED);
            return Err(BlockError::QueueTooLarge(queue_size));
        }

        let (available_offset, used_offset, bytes) = queue_layout(queue_size);
        let order = order_for(bytes);
        let Some(queue_physical) = memory::allocate_block(order) else {
            outb(port + register::DEVICE_STATUS, status::FAILED);
            return Err(BlockError::OutOfMemory);
        };
        core::ptr::write_bytes(
            layout::phys_to_virt(queue_physical) as *mut u8,
            0,
            1usize << (order + 12),
        );

        // One page for the request header, the sector, and the status byte.
        // Contiguous because the device reads it by physical address, and one
        // page is ample: a header is sixteen bytes and a sector five hundred
        // and twelve.
        let Some(scratch_physical) = memory::allocate_frame() else {
            memory::free_block(queue_physical, order);
            outb(port + register::DEVICE_STATUS, status::FAILED);
            return Err(BlockError::OutOfMemory);
        };
        core::ptr::write_bytes(layout::phys_to_virt(scratch_physical) as *mut u8, 0, 4096);

        // The device is told the queue's *page number*, which is the whole
        // reason the queue has to be page aligned.
        outl(
            port + register::QUEUE_ADDRESS,
            (queue_physical >> 12) as u32,
        );

        // The capacity is the first field of the device's own configuration,
        // in 512-byte sectors whatever the medium's real sector size is.
        let low = inl(port + register::CONFIG);
        let high = inl(port + register::CONFIG + 4);
        let capacity = (u64::from(high) << 32) | u64::from(low);

        outb(
            port + register::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK,
        );

        *DISK.lock() = Some(Disk {
            port,
            queue_size,
            queue_physical,
            scratch_physical,
            capacity,
            available_offset,
            used_offset,
        });

        INTERRUPT_LINE.store(u64::from(device.interrupt_line), Ordering::Release);

        kprintln!(
            "[blk ] virtio disk at {}: {} sectors ({} MiB), queue of {}, features {offered:#010x}",
            device.address,
            capacity,
            capacity * SECTOR_SIZE as u64 / (1024 * 1024),
            queue_size
        );

        Ok(capacity)
    }
}

/// Where the three parts of a queue of `size` descriptors sit, and how many
/// bytes the whole thing needs.
///
/// The used ring starts on a 4096-byte boundary measured from the start of the
/// queue. That padding is not an optimisation: the legacy layout defines it,
/// and a used ring one byte out of place is a device writing completions into
/// the middle of the available ring.
fn queue_layout(size: u16) -> (usize, usize, usize) {
    const ALIGNMENT: usize = 4096;
    let size = size as usize;

    let descriptors = 16 * size;
    let available = descriptors;
    // flags, index, one entry per descriptor, and the used-event word.
    let available_bytes = 2 + 2 + 2 * size + 2;

    let used = (available + available_bytes).div_ceil(ALIGNMENT) * ALIGNMENT;
    // flags, index, an eight-byte entry per descriptor, and the avail-event word.
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

/// Read one sector into `buffer`.
pub fn read_sector(sector: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
    transfer(request::READ, sector, buffer)?;
    SECTORS_READ.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Write one sector from `buffer`.
pub fn write_sector(sector: u64, buffer: &[u8]) -> Result<(), BlockError> {
    // The scratch page is filled here rather than inside `transfer`, which does
    // not know which direction it is going.
    let mut copy = [0u8; SECTOR_SIZE];
    let length = buffer.len().min(SECTOR_SIZE);
    copy[..length].copy_from_slice(&buffer[..length]);
    transfer(request::WRITE, sector, &mut copy)?;
    SECTORS_WRITTEN.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Sectors on the disk, or zero if there is none.
#[must_use]
pub fn capacity() -> u64 {
    DISK.lock().as_ref().map_or(0, |disk| disk.capacity)
}

/// Sectors read and written since boot.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        SECTORS_READ.load(Ordering::Relaxed),
        SECTORS_WRITTEN.load(Ordering::Relaxed),
    )
}

/// Spins waiting for the device before giving up.
///
/// Generous enough that a busy host does not look like a broken device, and
/// bounded so that a broken device does not look like a hang.
const COMPLETION_SPINS: u32 = 50_000_000;

/// Do one request, in whichever direction.
fn transfer(kind: u32, sector: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
    // One request at a time, and the waiting for a turn is done by blocking
    // wherever there is a scheduler to block under. Held across the whole
    // request, so the scratch page and the descriptor chain belong to this
    // caller until it is finished with them.
    acquire();
    let result = one_transfer(kind, sector, buffer);
    release();
    result
}

/// Take the driver, blocking until it is free.
fn acquire() {
    loop {
        // Read before the attempt, so a release that happens in between is seen
        // as a change and not slept through.
        let seen = FREE.generation();
        if !BUSY.swap(true, Ordering::Acquire) {
            return;
        }
        if crate::sched::current_id().is_none() {
            // No scheduler yet, so nothing to block: this is the boot thread
            // before the scheduler exists, and there is nobody else to run.
            core::hint::spin_loop();
            continue;
        }
        FREE.wait_if_unchanged(seen);
    }
}

/// Give it back.
fn release() {
    BUSY.store(false, Ordering::Release);
    FREE.wake_one();
}

/// One request, with the gate already held.
fn one_transfer(kind: u32, sector: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
    // A copy rather than the lock. Every field is a plain number that never
    // changes after bring-up, and holding an interrupt-safe lock across the
    // wait below would be holding it across a context switch -- which is a
    // processor spinning on a lock whose owner is asleep.
    let disk = {
        let guard = DISK.lock();
        *guard.as_ref().ok_or(BlockError::NoDevice)?
    };

    if sector >= disk.capacity {
        return Err(BlockError::OutOfRange);
    }

    let queue = layout::phys_to_virt(disk.queue_physical);
    let scratch = layout::phys_to_virt(disk.scratch_physical);

    // The scratch page, laid out: header, then the sector, then one byte for
    // the device to report on. Kept apart because the device is told each one's
    // address separately and writes only the last.
    let header_physical = disk.scratch_physical;
    let data_physical = disk.scratch_physical + 512;
    let status_physical = disk.scratch_physical + 1024;

    // SAFETY: the scratch page is mapped through the direct map, owned by this
    // driver, and large enough for all three. The gate above makes this caller
    // the only one touching it.
    unsafe {
        (scratch as *mut RequestHeader).write_volatile(RequestHeader {
            kind,
            reserved: 0,
            sector,
        });
        status_slot(scratch).write_volatile(0xFF);

        if kind == request::WRITE {
            core::ptr::copy_nonoverlapping(
                buffer.as_ptr(),
                (scratch + 512) as *mut u8,
                buffer.len().min(SECTOR_SIZE),
            );
        }
    }

    // Three descriptors: what to do, where the data goes, and how it went. The
    // device reads the first, reads or writes the second depending on the
    // direction, and always writes the third.
    let data_flags = if kind == request::READ {
        descriptor::WRITE
    } else {
        0
    };

    let used = (queue + disk.used_offset as u64) as *const u16;

    // SAFETY: the queue is mapped through the direct map and is large enough
    // for the descriptors this writes, which is checked at bring-up.
    let before = unsafe {
        let descriptors = queue as *mut Descriptor;
        descriptors.write_volatile(Descriptor {
            address: header_physical,
            length: 16,
            flags: descriptor::NEXT,
            next: 1,
        });
        descriptors.add(1).write_volatile(Descriptor {
            address: data_physical,
            length: SECTOR_SIZE as u32,
            flags: descriptor::NEXT | data_flags,
            next: 2,
        });
        descriptors.add(2).write_volatile(Descriptor {
            address: status_physical,
            length: 1,
            flags: descriptor::WRITE,
            next: 0,
        });

        // Append the head of the chain to the available ring. The index is free
        // running and wraps naturally; the ring slot is the index modulo its
        // size, which is what the device also computes.
        let available = (queue + disk.available_offset as u64) as *mut u16;
        let index = available.add(1).read_volatile();
        available
            .add(2 + (index % disk.queue_size) as usize)
            .write_volatile(0);

        // The device must see the descriptors and the ring entry before it sees
        // the new index, or it will follow a chain that is not there yet.
        fence(Ordering::SeqCst);
        available.add(1).write_volatile(index.wrapping_add(1));
        fence(Ordering::SeqCst);

        let before = used.add(1).read_volatile();

        // Read *before* the notify. The device may complete and its interrupt
        // may run before this line returns, and a waiter that read the counter
        // afterwards would miss the wake-up that had already happened.
        let generation = DONE.generation();

        outw(disk.port + register::QUEUE_NOTIFY, 0);
        (before, generation)
    };

    let (before, generation) = before;
    wait_for_completion(used, before, generation)?;

    // SAFETY: the device has published a used entry, so it has finished with
    // the scratch page, and the gate means nobody else has started with it.
    unsafe {
        fence(Ordering::SeqCst);

        let status = status_slot(scratch).read_volatile();
        if status != STATUS_OK {
            return Err(BlockError::Failed(status));
        }

        if kind == request::READ {
            core::ptr::copy_nonoverlapping(
                (scratch + 512) as *const u8,
                buffer.as_mut_ptr(),
                buffer.len().min(SECTOR_SIZE),
            );
        }
    }

    Ok(())
}

/// Wait for the device to publish a used entry past `before`.
///
/// Blocking where the interrupt has proved itself, spinning where it has not.
/// The spin is bounded and reports rather than hanging; the block cannot be
/// bounded the same way and does not need to be, because it is only ever
/// entered once an interrupt has been seen to arrive.
fn wait_for_completion(used: *const u16, before: u16, generation: u64) -> Result<(), BlockError> {
    if BLOCKING.load(Ordering::Relaxed) && crate::sched::current_id().is_some() {
        let mut generation = generation;
        loop {
            // SAFETY: the used ring is mapped through the direct map and this
            // is its index field, written by the device and read here.
            if unsafe { used.add(1).read_volatile() } != before {
                return Ok(());
            }
            DONE.wait_if_unchanged(generation);
            generation = DONE.generation();
        }
    }

    let mut spins = 0u32;
    loop {
        // SAFETY: as above.
        if unsafe { used.add(1).read_volatile() } != before {
            return Ok(());
        }
        spins += 1;
        if spins >= COMPLETION_SPINS {
            return Err(BlockError::Timeout);
        }
        core::hint::spin_loop();
    }
}

/// The device has finished something.
///
/// Reading the interrupt status register is what acknowledges it at the device,
/// and it has to happen whether or not anybody is waiting: a level-triggered
/// line that is never acknowledged is an interrupt storm.
///
/// # Safety
///
/// Call only as the handler for this device's vector.
pub unsafe fn on_interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);

    let port = {
        let guard = DISK.lock();
        match guard.as_ref() {
            Some(disk) => disk.port,
            None => return,
        }
    };

    // SAFETY: the window belongs to this device, and reading this register is
    // how the protocol says to acknowledge.
    let _reason = unsafe { crate::arch::io::inb(port + register::ISR) };

    // Everyone, not one. There is a single request in flight, so there is at
    // most one thread to wake -- but a spurious wake costs a re-read of a
    // counter, and a lost one costs a disk that stopped answering.
    DONE.wake_all();
}

/// Whether the device's interrupt has been seen to work.
///
/// Called after the first request. Until this says yes, every request spins.
pub fn adopt_interrupt() {
    if INTERRUPTS.load(Ordering::Relaxed) == 0 {
        kprintln!(
            "[blk ] no interrupt arrived from the disk; requests will spin \
             (correct, and it costs a processor for the length of every one)"
        );
        return;
    }
    BLOCKING.store(true, Ordering::Release);
    kprintln!("[blk ] the disk raised its interrupt; requests now block instead of spinning");
}

/// The interrupt line firmware gave the device, if there is one.
#[must_use]
pub fn interrupt_line() -> Option<u8> {
    match INTERRUPT_LINE.load(Ordering::Acquire) {
        u64::MAX => None,
        // 0xFF is what a device reports when firmware connected it to nothing.
        0xFF => None,
        line => u8::try_from(line).ok(),
    }
}

/// Interrupts the disk has raised, and whether requests block.
#[must_use]
pub fn interrupt_statistics() -> (u64, bool) {
    (
        INTERRUPTS.load(Ordering::Relaxed),
        BLOCKING.load(Ordering::Relaxed),
    )
}

/// Where the status byte lives inside the scratch page.
///
/// A kilobyte in, well clear of the header at the start and the sector at 512,
/// so the device writing one byte cannot touch either.
fn status_slot(scratch: u64) -> *mut u8 {
    (scratch + 1024) as *mut u8
}

/// Whether a disk is present.
#[must_use]
pub fn is_present() -> bool {
    DISK.lock().is_some()
}
