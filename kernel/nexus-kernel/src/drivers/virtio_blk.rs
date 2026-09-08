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
//! # Polling, for now
//!
//! Completion is waited for by reading the used ring rather than by taking the
//! device's interrupt. That is the wrong shape for a system that cares about
//! power, and it is the right shape for the first driver: an interrupt-driven
//! request needs the completion to hand a waiting thread back its buffer, which
//! needs the block layer that does not exist yet. The wait is bounded and
//! reports rather than hanging, which is the part that matters either way.

use core::sync::atomic::{fence, AtomicU64, Ordering};

use nexus_abi::layout;

use crate::arch::io::{inl, inw, outb, outl, outw};
use crate::drivers::pci::{BaseAddress, Device};
use crate::kprintln;
use crate::memory;
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
    let mut guard = DISK.lock();
    let disk = guard.as_mut().ok_or(BlockError::NoDevice)?;

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
    // driver, and large enough for all three.
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

    // SAFETY: the queue is mapped through the direct map and is large enough
    // for the descriptors this writes, which is checked at bring-up.
    unsafe {
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

        let used = (queue + disk.used_offset as u64) as *const u16;
        let before = used.add(1).read_volatile();

        outw(disk.port + register::QUEUE_NOTIFY, 0);

        let mut spins = 0u32;
        while used.add(1).read_volatile() == before {
            spins += 1;
            if spins >= COMPLETION_SPINS {
                return Err(BlockError::Timeout);
            }
            core::hint::spin_loop();
        }
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
