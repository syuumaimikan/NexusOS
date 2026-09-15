//! Bulk-only transport, and the SCSI commands that travel on it.
//!
//! The last two of the four layers, together because they are inseparable in
//! practice: bulk-only transport is a wrapper whose entire purpose is to carry
//! a SCSI command, and a SCSI command with no wrapper has nowhere to go.
//!
//! # What a transfer looks like
//!
//! Three bulk transfers, every time, in this order:
//!
//! 1. a **command block wrapper** out: thirty-one bytes, a signature, a tag,
//!    how much data is coming and which way, and the SCSI command itself;
//! 2. the **data**, in or out, if there is any;
//! 3. a **command status wrapper** back: thirteen bytes saying whether it
//!    worked and how much of the data did not move.
//!
//! The tag is echoed in the status, and a status whose tag does not match the
//! command is a status for somebody else's command -- which is checked here,
//! because the alternative is reading one command's result as another's.
//!
//! # Why SCSI
//!
//! Because a USB stick is not a new kind of thing. It presents the command set
//! a SCSI disk has presented since the 1980s, and the `08.06.50` in its
//! descriptor says exactly that: mass storage, *SCSI transparent command set*,
//! bulk-only transport. `READ(10)` and `WRITE(10)` are the two that matter, and
//! they are the same commands a hard disk of that era answered.

use nexus_abi::layout;

use crate::drivers::usb::{self, Attached};
use crate::drivers::xhci;
use crate::drivers::xhci_rings::{trb, Producer};
use crate::kprintln;

/// The signature a command block wrapper starts with: `USBC`.
const CBW_SIGNATURE: u32 = 0x4342_5355;
/// And a status wrapper: `USBS`.
const CSW_SIGNATURE: u32 = 0x5342_5355;

/// How long each wrapper is.
const CBW_LENGTH: u32 = 31;
const CSW_LENGTH: u32 = 13;

/// SCSI operation codes.
mod scsi {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
}

/// A disk reached over USB.
pub struct Disk {
    /// How many blocks it has, and how big each is.
    pub blocks: u64,
    pub block_size: u32,
    /// What it calls itself, from `INQUIRY`.
    pub vendor: [u8; 8],
    pub product: [u8; 16],
    /// The next tag to put on a command.
    tag: u32,
}

/// Why an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The controller refused something.
    Controller,
    /// The status wrapper was not one.
    BadStatus,
    /// It answered somebody else's command.
    WrongTag,
    /// The device says the command failed.
    Failed(u8),
    /// The device has no endpoints this can use.
    NoEndpoints,
    /// A request for more than the buffer holds.
    TooMuch,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Controller => out.write_str("the controller refused a transfer"),
            Self::BadStatus => out.write_str("what came back was not a status wrapper"),
            Self::WrongTag => {
                out.write_str("the status was for a different command than the one sent")
            }
            Self::Failed(status) => write!(out, "the device refused the command (status {status})"),
            Self::NoEndpoints => out.write_str("the device offers no pair of bulk endpoints"),
            Self::TooMuch => out.write_str("more was asked for than the buffer holds"),
        }
    }
}

/// The most one read or write moves.
///
/// One page, which is eight blocks of the usual size. A larger transfer would
/// need either a larger buffer or a scatter list, and one page is what the
/// frame allocator hands out.
pub const MAX_TRANSFER: u32 = 4096;

/// Which endpoint context index an endpoint has.
///
/// The controller numbers them by direction as well as number: endpoint *n*
/// going in is `2n + 1`, going out is `2n`. Endpoint zero is 1 and is the only
/// one that is both.
const fn context_index(endpoint: u8, inbound: bool) -> u8 {
    endpoint * 2 + if inbound { 1 } else { 0 }
}

/// Make a device's bulk endpoints usable, and find out what disk it is.
///
/// # Errors
///
/// Anything the controller or the device refused.
///
/// # Safety
///
/// The device must be attached and addressed.
pub unsafe fn open(attached: &mut Attached) -> Result<Disk, Trouble> {
    let (Some((in_endpoint, in_packet)), Some((out_endpoint, out_packet))) =
        (attached.device.bulk_in, attached.device.bulk_out)
    else {
        return Err(Trouble::NoEndpoints);
    };

    // A configuration has to be chosen before its endpoints exist. A device in
    // the addressed state has exactly one endpoint -- zero -- however many its
    // descriptors listed.
    // SAFETY: the device is addressed and its control ring is live.
    unsafe { usb::set_configuration(attached) }.map_err(|_| Trouble::Controller)?;

    let Some(in_ring) = Producer::new() else {
        return Err(Trouble::Controller);
    };
    let Some(out_ring) = Producer::new() else {
        return Err(Trouble::Controller);
    };

    let in_index = context_index(in_endpoint, true);
    let out_index = context_index(out_endpoint, false);
    let highest = in_index.max(out_index);

    let size = xhci::context_size();
    // SAFETY: the input context is a frame this device owns, and every offset
    // written is inside it -- the highest is context 31, which at 64 bytes each
    // is 2048 bytes into a 4096-byte page.
    unsafe {
        let at = layout::phys_to_virt(attached.input());
        core::ptr::write_bytes(at as *mut u8, 0, 4096);

        // Add the slot context and the two endpoints. The slot context is in
        // the list because its "context entries" field has to grow to cover
        // the new endpoints, and a context that is added must be supplied.
        let add = 1u32 | (1 << in_index) | (1 << out_index);
        core::ptr::write_volatile((at + 4) as *mut u32, add);

        // The slot context again, as it must now read: same speed and port as
        // before, with the entry count raised to the highest endpoint in use.
        let slot_at = at + size;
        let speed = (unsafe_port_speed(attached.device.port)) << 20;
        core::ptr::write_volatile(slot_at as *mut u32, speed | (u32::from(highest) << 27));
        core::ptr::write_volatile((slot_at + 4) as *mut u32, (attached.device.port as u32) << 16);

        // Bulk IN is endpoint type 6, bulk OUT is 2. Three retries, as for the
        // control endpoint and for the same reason.
        let in_at = at + size * (1 + u64::from(in_index));
        core::ptr::write_volatile(
            (in_at + 4) as *mut u32,
            (6 << 3) | (3 << 1) | (u32::from(in_packet) << 16),
        );
        core::ptr::write_volatile((in_at + 8) as *mut u64, in_ring.physical() | 1);
        core::ptr::write_volatile((in_at + 16) as *mut u32, u32::from(in_packet));

        let out_at = at + size * (1 + u64::from(out_index));
        core::ptr::write_volatile(
            (out_at + 4) as *mut u32,
            (2 << 3) | (3 << 1) | (u32::from(out_packet) << 16),
        );
        core::ptr::write_volatile((out_at + 8) as *mut u64, out_ring.physical() | 1);
        core::ptr::write_volatile((out_at + 16) as *mut u32, u32::from(out_packet));
    }

    // SAFETY: the controller is running and the input context is built.
    unsafe { xhci::configure_endpoint(attached.device.slot, attached.input()) }
        .map_err(|_| Trouble::Controller)?;

    attached.in_ring = Some(in_ring);
    attached.out_ring = Some(out_ring);

    let mut disk = Disk {
        blocks: 0,
        block_size: 0,
        vendor: [0; 8],
        product: [0; 16],
        tag: 1,
    };

    // What it calls itself. Thirty-six bytes, of which the interesting part is
    // eight of vendor and sixteen of product.
    let mut inquiry = [0u8; 16];
    inquiry[0] = scsi::INQUIRY;
    inquiry[4] = 36;
    // SAFETY: the endpoints are configured and the rings are live.
    unsafe { command(attached, &mut disk, &inquiry, 6, 36, true)? };
    // SAFETY: thirty-six bytes were just read into the buffer.
    unsafe {
        let at = attached.buffer_at();
        core::ptr::copy_nonoverlapping((at + 8) as *const u8, disk.vendor.as_mut_ptr(), 8);
        core::ptr::copy_nonoverlapping((at + 16) as *const u8, disk.product.as_mut_ptr(), 16);
    }

    // Is it ready? A stick usually is; a card reader with no card is not, and
    // says so rather than failing the capacity read in a way that reads like a
    // broken driver.
    let ready = [scsi::TEST_UNIT_READY; 1];
    let mut command_block = [0u8; 16];
    command_block[0] = ready[0];
    // SAFETY: as above.
    if unsafe { command(attached, &mut disk, &command_block, 6, 0, false) }.is_err() {
        // One retry after asking why. `REQUEST SENSE` is not politeness: a
        // device that has just been plugged in reports a unit attention
        // condition on its first command, and will keep reporting it until
        // somebody reads the sense data.
        let mut sense = [0u8; 16];
        sense[0] = scsi::REQUEST_SENSE;
        sense[4] = 18;
        // SAFETY: as above.
        let _ = unsafe { command(attached, &mut disk, &sense, 6, 18, true) };
        // SAFETY: as above.
        unsafe { command(attached, &mut disk, &command_block, 6, 0, false)? };
    }

    // How big it is. Eight bytes: the last block's number and the block size,
    // both big-endian, because SCSI predates the settling of that argument.
    let mut capacity = [0u8; 16];
    capacity[0] = scsi::READ_CAPACITY_10;
    // SAFETY: as above.
    unsafe { command(attached, &mut disk, &capacity, 10, 8, true)? };
    // SAFETY: eight bytes were just read.
    unsafe {
        let at = attached.buffer_at();
        let last = u32::from_be(core::ptr::read_unaligned(at as *const u32));
        let size = u32::from_be(core::ptr::read_unaligned((at + 4) as *const u32));
        // The command reports the *last* block, so the count is one more --
        // which is the classic off-by-one in every SCSI driver ever written.
        disk.blocks = u64::from(last) + 1;
        disk.block_size = size;
    }

    Ok(disk)
}

/// The speed a port is running at, read again.
///
/// The slot context has to be rewritten for Configure Endpoint and it carries
/// the speed, so the speed has to come from somewhere. Read from the port
/// rather than remembered, because the port is where it is true.
fn unsafe_port_speed(port: u64) -> u32 {
    // SAFETY: the controller is running and the port is one it reported.
    let value = unsafe { xhci::read_port(port - 1) };
    (value >> xhci::port::SPEED_SHIFT) & xhci::port::SPEED_MASK
}

/// Read blocks from the disk into its buffer.
///
/// # Errors
///
/// Anything the device refused, or a request larger than one page.
///
/// # Safety
///
/// The disk must have been opened.
pub unsafe fn read(
    attached: &mut Attached,
    disk: &mut Disk,
    block: u64,
    count: u16,
) -> Result<(), Trouble> {
    let bytes = u32::from(count) * disk.block_size;
    if bytes > MAX_TRANSFER {
        return Err(Trouble::TooMuch);
    }
    let mut command_block = [0u8; 16];
    command_block[0] = scsi::READ_10;
    command_block[2..6].copy_from_slice(&(block as u32).to_be_bytes());
    command_block[7..9].copy_from_slice(&count.to_be_bytes());
    // SAFETY: upheld by the caller.
    unsafe { command(attached, disk, &command_block, 10, bytes, true) }
}

/// Write blocks from its buffer onto the disk.
///
/// # Errors
///
/// As [`read`].
///
/// # Safety
///
/// As [`read`], and the buffer must already hold what is to be written.
pub unsafe fn write(
    attached: &mut Attached,
    disk: &mut Disk,
    block: u64,
    count: u16,
) -> Result<(), Trouble> {
    let bytes = u32::from(count) * disk.block_size;
    if bytes > MAX_TRANSFER {
        return Err(Trouble::TooMuch);
    }
    let mut command_block = [0u8; 16];
    command_block[0] = scsi::WRITE_10;
    command_block[2..6].copy_from_slice(&(block as u32).to_be_bytes());
    command_block[7..9].copy_from_slice(&count.to_be_bytes());
    // SAFETY: upheld by the caller.
    unsafe { command(attached, disk, &command_block, 10, bytes, false) }
}

/// One whole bulk-only transaction: wrapper out, data, status back.
///
/// # Safety
///
/// The endpoints must be configured and the rings live.
unsafe fn command(
    attached: &mut Attached,
    disk: &mut Disk,
    block: &[u8; 16],
    block_length: u8,
    data: u32,
    reading: bool,
) -> Result<(), Trouble> {
    let tag = disk.tag;
    disk.tag = disk.tag.wrapping_add(1);

    // The command wrapper, built in the second page so that the data page is
    // untouched -- a write's data is already in it.
    // SAFETY: the command page is this device's and thirty-one bytes fit in it.
    unsafe {
        let at = layout::phys_to_virt(attached.command_physical());
        core::ptr::write_bytes(at as *mut u8, 0, 64);
        core::ptr::write_volatile(at as *mut u32, CBW_SIGNATURE.to_le());
        core::ptr::write_volatile((at + 4) as *mut u32, tag);
        core::ptr::write_volatile((at + 8) as *mut u32, data);
        // Direction, in the top bit. Zero means out, which is also what it
        // means when there is no data at all.
        core::ptr::write_volatile((at + 12) as *mut u8, if reading { 0x80 } else { 0x00 });
        // Logical unit zero. A stick has one; a card reader may have several
        // and this drives the first.
        core::ptr::write_volatile((at + 13) as *mut u8, 0);
        core::ptr::write_volatile((at + 14) as *mut u8, block_length);
        core::ptr::copy_nonoverlapping(block.as_ptr(), (at + 15) as *mut u8, 16);
    }

    // Out it goes.
    // SAFETY: upheld by the caller.
    unsafe {
        bulk(attached, attached.command_physical(), CBW_LENGTH, false)?;
    }

    // Then the data, if there is any.
    if data > 0 {
        // SAFETY: as above.
        unsafe { bulk(attached, attached.buffer_physical(), data, reading)? };
    }

    // And the status. Read into the command page after the wrapper, so that a
    // status read cannot overwrite the command that is still being referred to.
    // SAFETY: as above.
    unsafe {
        bulk(
            attached,
            attached.command_physical() + 64,
            CSW_LENGTH,
            true,
        )?;
    }

    // SAFETY: thirteen bytes were just read into that offset.
    let (signature, echoed, status) = unsafe {
        let at = layout::phys_to_virt(attached.command_physical()) + 64;
        (
            u32::from_le(core::ptr::read_unaligned(at as *const u32)),
            core::ptr::read_unaligned((at + 4) as *const u32),
            core::ptr::read_volatile((at + 12) as *const u8),
        )
    };

    if signature != CSW_SIGNATURE {
        return Err(Trouble::BadStatus);
    }
    // The tag is the whole reason a tag exists. A status carrying somebody
    // else's tag means the two transactions have been confused, and reading it
    // as this one's answer is how a driver reports success for a command that
    // failed.
    if echoed != tag {
        return Err(Trouble::WrongTag);
    }
    if status != 0 {
        return Err(Trouble::Failed(status));
    }
    Ok(())
}

/// One bulk transfer, in or out.
///
/// # Safety
///
/// The endpoints must be configured, and `physical` must name at least `length`
/// bytes this device may use.
unsafe fn bulk(
    attached: &mut Attached,
    physical: u64,
    length: u32,
    reading: bool,
) -> Result<(), Trouble> {
    let (endpoint, ring) = if reading {
        let Some((endpoint, _)) = attached.device.bulk_in else {
            return Err(Trouble::NoEndpoints);
        };
        (
            context_index(endpoint, true),
            attached.in_ring.as_mut().ok_or(Trouble::NoEndpoints)?,
        )
    } else {
        let Some((endpoint, _)) = attached.device.bulk_out else {
            return Err(Trouble::NoEndpoints);
        };
        (
            context_index(endpoint, false),
            attached.out_ring.as_mut().ok_or(Trouble::NoEndpoints)?,
        )
    };

    // One normal TRB, asking for an interrupt when it is done and when it is
    // short. A bulk read of a status wrapper is usually exactly its length, but
    // a device that has less to say sends less, and without the short-packet
    // bit that transfer would produce no event at all.
    ring.push(
        physical,
        length,
        (trb::NORMAL << 10) | (1 << 5) | (1 << 2),
    );

    let slot = attached.device.slot;
    // SAFETY: upheld by the caller.
    unsafe { xhci::ring_doorbell(slot, endpoint) };
    // SAFETY: a transfer was just started.
    unsafe { xhci::await_transfer(length) }.map_err(|_| Trouble::Controller)?;
    Ok(())
}

/// Say what was found, for the log.
pub fn describe(disk: &Disk) {
    let vendor = core::str::from_utf8(&disk.vendor).unwrap_or("????????");
    let product = core::str::from_utf8(&disk.product).unwrap_or("????????????????");
    let megabytes = disk.blocks * u64::from(disk.block_size) / (1024 * 1024);
    kprintln!(
        "[usb ] {} {}: {} blocks of {} bytes, {} MiB",
        vendor.trim_end(),
        product.trim_end(),
        disk.blocks,
        disk.block_size,
        megabytes
    );
}

/// Read a sector, write one, and read it back.
///
/// The claim being tested is "an external USB drive can be read and written",
/// and each half of it needs its own evidence:
///
/// * **Reading** is checked against content the image was built with. Every
///   sector of it says its own number, so a read that returns the *wrong*
///   sector fails here rather than looking like a success. A driver that
///   returned sector zero for every request would pass a test that only read
///   sector zero.
/// * **Writing** is checked by reading it back -- and by reading back a
///   *different* sector afterwards, because a write that went to the wrong
///   place would otherwise look exactly like a write that went to the right
///   one.
///
/// It writes to the last sector, which is the one nothing else on the image
/// depends on.
///
/// # Safety
///
/// The disk must have been opened.
pub unsafe fn verify(attached: &mut Attached, disk: &mut Disk) {
    if disk.blocks < 8 || disk.block_size as usize != 512 {
        kprintln!("[usb ] the disk is an odd shape; not testing it");
        return;
    }

    // A sector that is not the first. Zero is what a broken driver returns.
    const PROBE: u64 = 5;
    // SAFETY: the disk is open and one block fits the buffer.
    if let Err(trouble) = unsafe { read(attached, disk, PROBE, 1) } {
        kprintln!("[usb ] FAILED: could not read block {PROBE}: {trouble}");
        return;
    }
    // SAFETY: 512 bytes were just read into the buffer.
    let text = unsafe {
        core::slice::from_raw_parts(attached.buffer_at() as *const u8, 32)
    };
    let expected = b"NEXUS USB SECTOR 000005";
    if &text[..expected.len()] != expected {
        kprintln!(
            "[usb ] FAILED: block {PROBE} says {:?}, not what the image was built with",
            core::str::from_utf8(&text[..24]).unwrap_or("<not text>")
        );
        return;
    }
    kprintln!("[usb ] read block {PROBE} and it holds what the image was built with");

    // Now write. The last block, so nothing else cares what is in it.
    let last = disk.blocks - 1;
    let marker = b"NEXUS WROTE THIS OVER USB";
    // SAFETY: the buffer is a page and this writes 512 bytes of it.
    unsafe {
        let at = attached.buffer_at() as *mut u8;
        core::ptr::write_bytes(at, b'.', 512);
        core::ptr::copy_nonoverlapping(marker.as_ptr(), at, marker.len());
    }
    // SAFETY: the disk is open and the buffer holds what is to be written.
    if let Err(trouble) = unsafe { write(attached, disk, last, 1) } {
        kprintln!("[usb ] FAILED: could not write block {last}: {trouble}");
        return;
    }

    // Read something else first, so that the buffer cannot simply still hold
    // what was written. Without this the read-back would pass against a driver
    // whose write and read both did nothing.
    // SAFETY: as above.
    if unsafe { read(attached, disk, PROBE, 1) }.is_err() {
        kprintln!("[usb ] FAILED: could not read back after writing");
        return;
    }

    // SAFETY: as above.
    if let Err(trouble) = unsafe { read(attached, disk, last, 1) } {
        kprintln!("[usb ] FAILED: could not read block {last} back: {trouble}");
        return;
    }
    // SAFETY: 512 bytes were just read.
    let back = unsafe {
        core::slice::from_raw_parts(attached.buffer_at() as *const u8, marker.len())
    };
    if back == marker {
        kprintln!("[usb ] wrote block {last} and read it back: the drive can be written to");
    } else {
        kprintln!(
            "[usb ] FAILED: block {last} reads {:?} after being written",
            core::str::from_utf8(back).unwrap_or("<not text>")
        );
    }
}
