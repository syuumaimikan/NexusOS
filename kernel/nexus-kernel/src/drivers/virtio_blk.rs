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
    /// Everything already written is on the medium before this answers.
    pub const FLUSH: u32 = 4;
}

/// Features this driver understands.
mod feature {
    /// The device takes a flush request, and says when it has finished one.
    ///
    /// The only optional feature claimed here, and the reason is not speed for
    /// its own sake. A device that is not asked for this is a device its host
    /// dare not give a write-back cache to -- there would be no way for the
    /// guest to say "this much must survive", so every write has to be treated
    /// as if it were that point. QEMU does exactly that: it runs the drive
    /// write-through, and a fresh eight-gigabyte NexusFS takes half a minute
    /// because every four-kilobyte write waits for a real disk.
    pub const FLUSH: u32 = 1 << 9;
}

/// What the device writes into the status byte.
const STATUS_OK: u8 = 0;

/// Bytes in a disk sector, as virtio defines it regardless of the medium.
pub const SECTOR_SIZE: usize = 512;

/// Sectors one request may carry.
///
/// Eight, which is four kilobytes, which is one filesystem block. That is the
/// number that matters: every read and write above this driver is a block, and
/// at one sector per request each of them cost eight round trips to the device
/// and eight waits for an interrupt. Formatting an eight-gigabyte NexusFS
/// writes sixteen thousand blocks of inode table, and at eight requests apiece
/// it did not finish.
pub const MAX_TRANSFER_SECTORS: usize = 8;

/// Frames the scratch region takes, as an order: two pages.
const SCRATCH_ORDER: usize = 1;
/// And how many bytes that is.
const SCRATCH_BYTES: usize = 4096 << SCRATCH_ORDER;
/// Where the data begins inside it. The second page, so that the header and the
/// status byte in the first are nowhere near what the device may overwrite.
const SCRATCH_DATA: u64 = 4096;

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
    /// The request named a sector past the end of the disk, or ran off it.
    OutOfRange,
    /// The buffer was not a whole number of sectors, or was more of them than
    /// one request carries.
    BadLength,
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
            Self::BadLength => write!(
                f,
                "a request carries between one and {MAX_TRANSFER_SECTORS} whole sectors"
            ),
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
    /// Whether the device agreed to take flush requests.
    flushes: bool,
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
/// Flushes asked for since boot.
static FLUSHES: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static POLL_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static WAIT_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Aggregate request paths; a wait attempt can return without sleeping when
/// an interrupt has already advanced the wait queue's generation.
#[derive(Clone, Copy, Debug)]
pub struct WaitStatistics {
    pub requests: u64,
    pub poll_completions: u64,
    pub wait_attempts: u64,
}

/// A diagnostic snapshot, without logging in the request path.
#[must_use]
pub fn wait_statistics() -> WaitStatistics {
    WaitStatistics {
        requests: REQUESTS.load(Ordering::Relaxed),
        poll_completions: POLL_COMPLETIONS.load(Ordering::Relaxed),
        wait_attempts: WAIT_ATTEMPTS.load(Ordering::Relaxed),
    }
}

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

        // One optional feature is claimed, and only one. Every other one
        // changes the layout or the semantics of something, and a driver that
        // accepted a feature it did not implement would be agreeing to a
        // protocol it does not speak.
        //
        // `FLUSH` is claimed because refusing it is not free. A host cannot
        // give a write-back cache to a guest with no way to ask for a barrier,
        // so it stops caching instead and every single write goes to the
        // medium. That is not a theory: this driver claimed nothing for months
        // and a fresh eight-gigabyte format took thirty-three seconds, which
        // `cache=unsafe` -- a host told to ignore flushes -- did in two.
        //
        // And it is claimed only if offered. A driver that assumed a feature
        // and sent a request the device does not know is a driver waiting for
        // an answer that is not coming.
        let offered = inl(port + register::DEVICE_FEATURES);
        let flushes = offered & feature::FLUSH != 0;
        outl(
            port + register::DRIVER_FEATURES,
            if flushes { feature::FLUSH } else { 0 },
        );

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

        // Two pages for the request header, the data and the status byte.
        // Contiguous because the device reads it by physical address, and two
        // rather than one because the second is entirely data: a request may
        // carry eight sectors, which is one filesystem block.
        let Some(scratch_physical) = memory::allocate_block(SCRATCH_ORDER) else {
            memory::free_block(queue_physical, order);
            outb(port + register::DEVICE_STATUS, status::FAILED);
            return Err(BlockError::OutOfMemory);
        };
        core::ptr::write_bytes(
            layout::phys_to_virt(scratch_physical) as *mut u8,
            0,
            SCRATCH_BYTES,
        );

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
            flushes,
        });

        INTERRUPT_LINE.store(u64::from(device.interrupt_line), Ordering::Release);

        kprintln!(
            "[blk ] virtio disk at {}: {} sectors ({} MiB), queue of {}, features {offered:#010x}",
            device.address,
            capacity,
            capacity * SECTOR_SIZE as u64 / (1024 * 1024),
            queue_size
        );
        if flushes {
            kprintln!("[blk ] the disk takes a flush, so the host may cache what is written");
        } else {
            kprintln!(
                "[blk ] this disk offers no flush; every write will wait for the medium"
            );
        }

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
    let length = buffer.len().min(SECTOR_SIZE);
    let mut copy = [0u8; SECTOR_SIZE];
    read_sectors(sector, &mut copy)?;
    buffer[..length].copy_from_slice(&copy[..length]);
    Ok(())
}

/// Read `buffer.len() / 512` consecutive sectors into `buffer`.
///
/// # Errors
///
/// [`BlockError::BadLength`] unless the buffer is a whole number of sectors and
/// no more than [`MAX_TRANSFER_SECTORS`] of them; otherwise as the device says.
pub fn read_sectors(sector: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
    let count = check_length(buffer.len())?;
    transfer(request::READ, sector, buffer)?;
    SECTORS_READ.fetch_add(count as u64, Ordering::Relaxed);
    Ok(())
}

/// Write one sector from `buffer`.
pub fn write_sector(sector: u64, buffer: &[u8]) -> Result<(), BlockError> {
    let mut copy = [0u8; SECTOR_SIZE];
    let length = buffer.len().min(SECTOR_SIZE);
    copy[..length].copy_from_slice(&buffer[..length]);
    write_sectors(sector, &copy)
}

/// Write `buffer.len() / 512` consecutive sectors from `buffer`.
///
/// # Errors
///
/// As [`read_sectors`].
pub fn write_sectors(sector: u64, buffer: &[u8]) -> Result<(), BlockError> {
    let count = check_length(buffer.len())?;
    // Copied into a buffer of this function's own rather than handed over,
    // because the device is given a physical address and the caller's slice is
    // wherever the caller put it. `transfer` takes it by exclusive reference
    // because a read fills it; nothing is read back here.
    let mut copy = [0u8; MAX_TRANSFER_SECTORS * SECTOR_SIZE];
    copy[..buffer.len()].copy_from_slice(buffer);
    transfer(request::WRITE, sector, &mut copy[..buffer.len()])?;
    SECTORS_WRITTEN.fetch_add(count as u64, Ordering::Relaxed);
    Ok(())
}

/// Everything written before this call is on the medium when it returns.
///
/// The barrier the journal is built on. Without it a filesystem that writes its
/// intention, then the change, then the record that the change is done has no
/// way to stop a host from reordering those three -- and a journal whose
/// entries can arrive out of order is a journal that describes a disk that
/// never existed.
///
/// Returns `Ok(())` and does nothing on a device that did not offer the
/// feature, which is not a lie by omission: such a device was never given a
/// write-back cache in the first place, so everything already written is
/// already where a flush would have put it.
///
/// # Errors
///
/// [`BlockError::NoDevice`] if there is no disk, [`BlockError::Failed`] if the
/// device refused, [`BlockError::Timeout`] if it never answered.
pub fn flush() -> Result<(), BlockError> {
    let flushes = {
        let guard = DISK.lock();
        guard.as_ref().ok_or(BlockError::NoDevice)?.flushes
    };
    if !flushes {
        return Ok(());
    }

    acquire();
    let result = one_flush();
    release();
    if result.is_ok() {
        FLUSHES.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// How many sectors a buffer of `bytes` is, if it is a legal size.
fn check_length(bytes: usize) -> Result<usize, BlockError> {
    if bytes == 0 || !bytes.is_multiple_of(SECTOR_SIZE) || bytes > MAX_TRANSFER_SECTORS * SECTOR_SIZE
    {
        return Err(BlockError::BadLength);
    }
    Ok(bytes / SECTOR_SIZE)
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

/// Flushes asked for since boot, and whether the device takes them at all.
#[must_use]
pub fn flush_statistics() -> (u64, bool) {
    let flushes = DISK.lock().as_ref().is_some_and(|disk| disk.flushes);
    (FLUSHES.load(Ordering::Relaxed), flushes)
}

/// Spins waiting for the device before giving up.
///
/// Generous enough that a busy host does not look like a broken device, and
/// bounded so that a broken device does not look like a hang.
const COMPLETION_SPINS: u32 = 50_000_000;

/// Spins before giving up on the device answering straight away.
///
/// Sized by what it is waiting for rather than by feel: a virtio request to an
/// emulated disk is tens of microseconds, and this is a few hundred thousand
/// spins, which is the same order. Too small and every request pays for a
/// context switch; too large and a thread waiting on a genuinely slow device
/// holds a processor that something else could be using.
const HANDOFF_SPINS: u32 = 400_000;

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

    // The *last* sector the request touches, not the first. Checking only the
    // first would let a request that starts inside the disk run off the end of
    // it, which is the whole reason a multi-sector request needs a check the
    // single-sector one did not.
    let count = (buffer.len() / SECTOR_SIZE) as u64;
    if sector >= disk.capacity || count == 0 || disk.capacity - sector < count {
        return Err(BlockError::OutOfRange);
    }

    let queue = layout::phys_to_virt(disk.queue_physical);
    let scratch = layout::phys_to_virt(disk.scratch_physical);

    // The scratch region, laid out: the header and the status byte in the first
    // page, the data in the second. Kept apart because the device is told each
    // one's address separately, and the data is given a page of its own so that
    // a full eight-sector transfer has room without reaching the status byte.
    let header_physical = disk.scratch_physical;
    let data_physical = disk.scratch_physical + SCRATCH_DATA;
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
                (scratch + SCRATCH_DATA) as *mut u8,
                buffer.len(),
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
            length: buffer.len() as u32,
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

        // Sample completion state before publishing the request. The device
        // can consume an available entry before the notify port is written.
        let before = used.add(1).read_volatile();
        let generation = DONE.generation();
        REQUESTS.fetch_add(1, Ordering::Relaxed);

        // The device must see the descriptors and the ring entry before it sees
        // the new index, or it will follow a chain that is not there yet.
        fence(Ordering::SeqCst);
        available.add(1).write_volatile(index.wrapping_add(1));
        fence(Ordering::SeqCst);

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
                (scratch + SCRATCH_DATA) as *const u8,
                buffer.as_mut_ptr(),
                buffer.len(),
            );
        }
    }

    Ok(())
}

/// A flush, with the gate already held.
///
/// Two descriptors rather than three, and that is the whole difference from a
/// transfer. A flush carries nothing: the header says what to do and the status
/// byte says how it went, and a data descriptor of length zero is not the same
/// thing as no data descriptor -- the specification describes this request as
/// header and status, and a device is within its rights to refuse the other
/// shape.
fn one_flush() -> Result<(), BlockError> {
    let disk = {
        let guard = DISK.lock();
        *guard.as_ref().ok_or(BlockError::NoDevice)?
    };

    let queue = layout::phys_to_virt(disk.queue_physical);
    let scratch = layout::phys_to_virt(disk.scratch_physical);
    let header_physical = disk.scratch_physical;
    let status_physical = disk.scratch_physical + 1024;

    // SAFETY: the scratch page is mapped through the direct map and the gate
    // makes this caller the only one touching it. The sector field is unused by
    // a flush and is written zero rather than left as whatever the last request
    // put there, because a device is allowed to look.
    unsafe {
        (scratch as *mut RequestHeader).write_volatile(RequestHeader {
            kind: request::FLUSH,
            reserved: 0,
            sector: 0,
        });
        status_slot(scratch).write_volatile(0xFF);
    }

    let used = (queue + disk.used_offset as u64) as *const u16;

    // SAFETY: as in `one_transfer` -- the queue is mapped through the direct
    // map and is large enough for two descriptors.
    let (before, generation) = unsafe {
        let descriptors = queue as *mut Descriptor;
        descriptors.write_volatile(Descriptor {
            address: header_physical,
            length: 16,
            flags: descriptor::NEXT,
            next: 1,
        });
        descriptors.add(1).write_volatile(Descriptor {
            address: status_physical,
            length: 1,
            flags: descriptor::WRITE,
            next: 0,
        });

        let available = (queue + disk.available_offset as u64) as *mut u16;
        let index = available.add(1).read_volatile();
        available
            .add(2 + (index % disk.queue_size) as usize)
            .write_volatile(0);

        let before = used.add(1).read_volatile();
        let generation = DONE.generation();
        REQUESTS.fetch_add(1, Ordering::Relaxed);

        fence(Ordering::SeqCst);
        available.add(1).write_volatile(index.wrapping_add(1));
        fence(Ordering::SeqCst);

        outw(disk.port + register::QUEUE_NOTIFY, 0);
        (before, generation)
    };

    wait_for_completion(used, before, generation)?;

    // SAFETY: the device has published a used entry, so it has finished with
    // the status byte.
    unsafe {
        fence(Ordering::SeqCst);
        let status = status_slot(scratch).read_volatile();
        if status != STATUS_OK {
            return Err(BlockError::Failed(status));
        }
    }

    Ok(())
}

/// Wait for the device to publish a used entry past `before`.
///
/// Blocking where the interrupt has proved itself, spinning where it has not.
/// The spin is bounded; the blocking path currently has no deadline. A past
/// interrupt proves routing, not future device health. Returning a timeout
/// safely requires quiescing the device before another request can reuse its
/// descriptor chain and DMA scratch memory.
fn wait_for_completion(used: *const u16, before: u16, generation: u64) -> Result<(), BlockError> {
    // A short spin first, however this is going to wait.
    //
    // An emulated disk answers in microseconds, and going to sleep for that
    // costs two context switches and the latency of being picked again -- which
    // measured at milliseconds, not microseconds. Formatting an eight-gigabyte
    // NexusFS is sixteen thousand block writes, and at a millisecond each that
    // is a boot that never finishes. This catches the ordinary case without
    // sleeping at all; the block below is still there for the request that
    // really is slow, so nothing is busy-waiting on a disk that is thinking.
    for _ in 0..HANDOFF_SPINS {
        // SAFETY: the used ring is mapped through the direct map and this is
        // its index field, written by the device and read here.
        if unsafe { used.add(1).read_volatile() } != before {
            POLL_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        core::hint::spin_loop();
    }

    if BLOCKING.load(Ordering::Relaxed) && crate::sched::current_id().is_some() {
        let mut generation = generation;
        let mut attempted_wait = false;
        loop {
            // SAFETY: the used ring is mapped through the direct map and this
            // is its index field, written by the device and read here.
            if unsafe { used.add(1).read_volatile() } != before {
                if !attempted_wait {
                    POLL_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            attempted_wait = true;
            WAIT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            DONE.wait_if_unchanged(generation);
            generation = DONE.generation();
        }
    }

    let mut spins = 0u32;
    loop {
        // SAFETY: as above.
        if unsafe { used.add(1).read_volatile() } != before {
            POLL_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
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
    let port = {
        let guard = DISK.lock();
        match guard.as_ref() {
            Some(disk) => disk.port,
            None => return,
        }
    };

    // SAFETY: the window belongs to this device, and reading this register is
    // how the protocol says to acknowledge.
    let reason = unsafe { crate::arch::io::inb(port + register::ISR) };
    if reason == 0 {
        // Another device raised this shared line. It cannot prove that our
        // completion interrupt works or wake a disk request.
        return;
    }
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);

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
