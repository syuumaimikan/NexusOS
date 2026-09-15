//! What a USB device is, above the controller that it is plugged into.
//!
//! The second of four layers. `xhci.rs` knows about rings and slots and knows
//! nothing about USB; this knows about descriptors and endpoints and knows
//! nothing about TRBs beyond asking for a transfer.
//!
//! # Getting a device to say anything
//!
//! A device on a freshly reset port has no address and will answer only on
//! endpoint zero. Bringing it into use is four steps and they are strictly
//! ordered:
//!
//! 1. **Enable Slot** — the controller hands out a slot, which is the device's
//!    handle from then on.
//! 2. **An input context** — a description of the device: how fast it is, which
//!    root port it is on, and where its endpoint-zero transfer ring lives.
//! 3. **Address Device** — the controller reads that, addresses the device on
//!    the bus, and writes back a device context.
//! 4. **`GET_DESCRIPTOR`** — and only now will it tell you what it is.
//!
//! Each step needs the one before it. A `GET_DESCRIPTOR` sent to a device that
//! has not been addressed goes nowhere, and the failure is a transfer that
//! never completes rather than an error saying so.
//!
//! # Control transfers
//!
//! Endpoint zero speaks in three stages: a **setup** stage carrying the
//! eight-byte request, an optional **data** stage, and a **status** stage going
//! the other way. All three go on the ring as TRBs and one event comes back,
//! because only the last one asks for an interrupt on completion.

use nexus_abi::layout;

use crate::drivers::xhci;
use crate::drivers::xhci_rings::{trb, Producer};
use crate::kprintln;

/// Standard requests, as a setup packet's `bRequest`.
mod request {
    pub const GET_DESCRIPTOR: u8 = 6;
    pub const SET_CONFIGURATION: u8 = 9;
}

/// Descriptor types, as the high byte of a `GET_DESCRIPTOR` value.
mod descriptor {
    pub const DEVICE: u16 = 1;
    pub const CONFIGURATION: u16 = 2;
}

/// Which way a control transfer's data goes, in the setup packet's first byte.
mod direction {
    /// Device to host.
    pub const IN: u8 = 0x80;
    /// Host to device.
    pub const OUT: u8 = 0x00;
}

/// What this driver learned about a device.
#[derive(Debug, Clone)]
pub struct Device {
    /// The controller's handle for it.
    pub slot: u8,
    /// Which root port it is on, counting from one.
    pub port: u64,
    /// Its class, subclass and protocol, from the interface descriptor.
    ///
    /// From the *interface* and not the device: a mass storage device almost
    /// always reports class zero at the device level and says what it really is
    /// per interface, which is how a device with a card reader and a hub in it
    /// describes itself.
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// Who made it and what they call it.
    pub vendor: u16,
    pub product: u16,
    /// The configuration value to select, and the interface number.
    pub configuration: u8,
    pub interface: u8,
    /// The bulk endpoints, if it has a pair: their numbers and packet sizes.
    pub bulk_in: Option<(u8, u16)>,
    pub bulk_out: Option<(u8, u16)>,
}

/// The mass-storage class, and the one subclass and protocol this drives.
pub const MASS_STORAGE: u8 = 0x08;
/// SCSI transparent command set.
pub const SCSI: u8 = 0x06;
/// Bulk-only transport.
pub const BULK_ONLY: u8 = 0x50;

/// Everything kept alive for one device.
///
/// The controller reads these frames while the device exists. Held rather than
/// freed, for the same reason the rings are.
pub struct Attached {
    pub device: Device,
    /// Its endpoint-zero ring, and the bulk pair's.
    pub control: Producer,
    pub in_ring: Option<Producer>,
    pub out_ring: Option<Producer>,
    /// The input context, rebuilt for each command that takes one.
    input: u64,
    /// A page to move data through.
    pub buffer: u64,
    /// And a second one for command and status wrappers.
    ///
    /// Separate from the data page on purpose: a write puts its data in
    /// `buffer` and then has to build a wrapper describing it, and one page
    /// for both would mean the wrapper overwriting what it describes.
    pub command: u64,
}

impl Attached {
    /// Where the data buffer is, as this kernel can read it.
    #[must_use]
    pub fn buffer_at(&self) -> u64 {
        layout::phys_to_virt(self.buffer)
    }

    /// Its physical address, which is what a TRB names.
    #[must_use]
    pub const fn buffer_physical(&self) -> u64 {
        self.buffer
    }

    /// And the command page's.
    #[must_use]
    pub const fn command_physical(&self) -> u64 {
        self.command
    }
}

/// Bring up whatever is on `port`, and say what it is.
///
/// # Errors
///
/// Anything the controller refused, or a device that did not answer.
///
/// # Safety
///
/// The controller must be running and the port must be enabled.
pub unsafe fn attach(port: u64, speed: u32) -> Result<Attached, xhci::Trouble> {
    // SAFETY: upheld by the caller.
    let slot = unsafe { xhci::enable_slot()? };

    let Some(control) = Producer::new() else {
        return Err(xhci::Trouble::NoMemory);
    };
    let Some(input) = crate::memory::allocate_frame() else {
        return Err(xhci::Trouble::NoMemory);
    };
    let Some(output) = crate::memory::allocate_frame() else {
        return Err(xhci::Trouble::NoMemory);
    };
    let Some(buffer) = crate::memory::allocate_frame() else {
        return Err(xhci::Trouble::NoMemory);
    };
    let Some(command) = crate::memory::allocate_frame() else {
        return Err(xhci::Trouble::NoMemory);
    };

    // SAFETY: three fresh frames, each this device's alone.
    unsafe {
        core::ptr::write_bytes(layout::phys_to_virt(input) as *mut u8, 0, 4096);
        core::ptr::write_bytes(layout::phys_to_virt(output) as *mut u8, 0, 4096);
        core::ptr::write_bytes(layout::phys_to_virt(buffer) as *mut u8, 0, 4096);
        core::ptr::write_bytes(layout::phys_to_virt(command) as *mut u8, 0, 4096);
    }

    // The device context goes in the array at this slot's index. The controller
    // writes into it; nothing here reads it, but it must exist and be its own
    // page or the controller has nowhere to put what it knows.
    // SAFETY: the array is a frame this driver allocated, with one entry per
    // slot, and `slot` is one the controller just handed out.
    unsafe {
        core::ptr::write_volatile(
            (layout::phys_to_virt(crate::drivers::xhci_rings::contexts()) + u64::from(slot) * 8) as *mut u64,
            output,
        );
    }

    // The input context: a control word saying which contexts are being set,
    // then the slot context, then endpoint zero's.
    let size = xhci::context_size();
    let packet = first_packet_size(speed);
    // SAFETY: the frame is this device's and every offset below is inside it.
    unsafe {
        let at = layout::phys_to_virt(input);
        // Add the slot context and endpoint zero. Nothing is being dropped.
        core::ptr::write_volatile((at + 4) as *mut u32, 0b11);

        // The slot context, one context in.
        let slot_at = at + size;
        // Route string zero -- this device is on a root port, not behind a hub
        // -- the speed, and one context entry, which is endpoint zero.
        core::ptr::write_volatile(slot_at as *mut u32, (speed << 20) | (1 << 27));
        // Which root port. Counted from one, which is why the caller's index
        // has already had one added to it.
        core::ptr::write_volatile((slot_at + 4) as *mut u32, (port as u32) << 16);

        // Endpoint zero's context, two contexts in.
        let endpoint_at = at + size * 2;
        // Type 4 is a control endpoint; three is the error count the
        // specification asks for, and a zero there means "do not retry", which
        // turns one lost packet into a dead device.
        core::ptr::write_volatile(
            (endpoint_at + 4) as *mut u32,
            (4 << 3) | (3 << 1) | (u32::from(packet) << 16),
        );
        // Where its ring is, and the cycle bit the controller should start on.
        core::ptr::write_volatile((endpoint_at + 8) as *mut u64, control.physical() | 1);
        // The average TRB length. Eight is what a setup packet is.
        core::ptr::write_volatile((endpoint_at + 16) as *mut u32, 8);
    }

    // SAFETY: the controller is running, the slot came from it, and the input
    // context is built and stays where it is.
    unsafe { xhci::address_device(slot, input)? };

    let mut attached = Attached {
        device: Device {
            slot,
            port,
            class: 0,
            subclass: 0,
            protocol: 0,
            vendor: 0,
            product: 0,
            configuration: 0,
            interface: 0,
            bulk_in: None,
            bulk_out: None,
        },
        control,
        in_ring: None,
        out_ring: None,
        input,
        buffer,
        command,
    };

    // Now it will talk. The device descriptor is eighteen bytes and says who
    // made it; the configuration descriptor is a tree and says what it can do.
    // SAFETY: the device is addressed and its ring is live.
    let read = unsafe { control_in(&mut attached, descriptor::DEVICE, 0, 18)? };
    if read < 18 {
        kprintln!("[usb ] the device on port {port} gave {read} bytes of an 18-byte descriptor");
        return Err(xhci::Trouble::Stuck("did not describe itself"));
    }
    // SAFETY: eighteen bytes were just read into the buffer.
    unsafe {
        let at = attached.buffer_at();
        attached.device.vendor = core::ptr::read_unaligned((at + 8) as *const u16);
        attached.device.product = core::ptr::read_unaligned((at + 10) as *const u16);
    }

    // The configuration descriptor, twice: once for its own nine bytes, which
    // say how long the whole tree is, and once for the tree. Asking for a fixed
    // large size instead would work for most devices and truncate the rest.
    // SAFETY: as above.
    let read = unsafe { control_in(&mut attached, descriptor::CONFIGURATION, 0, 9)? };
    if read < 9 {
        return Err(xhci::Trouble::Stuck("did not describe its configuration"));
    }
    // SAFETY: nine bytes were just read.
    let (total, configuration) = unsafe {
        let at = attached.buffer_at();
        (
            core::ptr::read_unaligned((at + 2) as *const u16),
            core::ptr::read_volatile((at + 5) as *const u8),
        )
    };
    attached.device.configuration = configuration;

    let total = total.min(4096) as u32;
    // SAFETY: as above.
    let read = unsafe { control_in(&mut attached, descriptor::CONFIGURATION, 0, total)? };
    // SAFETY: `read` bytes were just written into the buffer, which is a page.
    let tree = unsafe {
        core::slice::from_raw_parts(
            attached.buffer_at() as *const u8,
            (read as usize).min(4096),
        )
    };
    read_interfaces(&mut attached.device, tree);

    Ok(attached)
}

/// How big endpoint zero's packets are, before the device has said.
///
/// The specification fixes this per speed, which is what makes the first
/// transfer possible at all: a device cannot say how big its packets are until
/// something has asked, and asking needs a packet size.
const fn first_packet_size(speed: u32) -> u16 {
    match speed {
        // SuperSpeed and above: always 512.
        4 | 5 => 512,
        // Low speed: always 8.
        2 => 8,
        // High and full speed: 64 is right for high speed and is the largest a
        // full-speed device may use. A full-speed device with 8-byte packets
        // answers the first descriptor read short, which is handled.
        _ => 64,
    }
}

/// Walk a configuration descriptor and pick out what this driver needs.
///
/// A configuration descriptor is a flat list of variable-length records: each
/// starts with its own length and a type. Walking it by those lengths is the
/// only correct way -- the records are not fixed size and are not in a fixed
/// order.
fn read_interfaces(device: &mut Device, tree: &[u8]) {
    /// Record types.
    const INTERFACE: u8 = 4;
    const ENDPOINT: u8 = 5;

    let mut at = 0usize;
    // Whether the interface being walked is the one that was chosen, so that a
    // second interface's endpoints are not attributed to the first.
    let mut ours = false;

    while at + 2 <= tree.len() {
        let length = tree[at] as usize;
        let kind = tree[at + 1];
        // A zero length would loop for ever, and a length past the end is a
        // record that is not there. Both mean the descriptor is malformed, and
        // stopping is the only safe answer.
        if length < 2 || at + length > tree.len() {
            break;
        }

        match kind {
            INTERFACE if length >= 9 => {
                let class = tree[at + 5];
                let subclass = tree[at + 6];
                let protocol = tree[at + 7];
                // The first mass-storage interface wins. A device with two is
                // rare and choosing the first is a decision rather than an
                // accident.
                ours = class == MASS_STORAGE && device.bulk_in.is_none();
                if ours {
                    device.interface = tree[at + 2];
                    device.class = class;
                    device.subclass = subclass;
                    device.protocol = protocol;
                }
            }
            ENDPOINT if length >= 7 && ours => {
                let address = tree[at + 2];
                let attributes = tree[at + 3];
                let packet = u16::from(tree[at + 4]) | (u16::from(tree[at + 5]) << 8);
                // Attribute bits 0 and 1 say the transfer type; two is bulk.
                if attributes & 0b11 == 2 {
                    if address & 0x80 != 0 {
                        device.bulk_in = Some((address & 0x0F, packet));
                    } else {
                        device.bulk_out = Some((address & 0x0F, packet));
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
}

/// A control transfer that reads from the device into its buffer.
///
/// # Safety
///
/// The device must be addressed and its control ring live.
unsafe fn control_in(
    attached: &mut Attached,
    kind: u16,
    index: u16,
    length: u32,
) -> Result<u32, xhci::Trouble> {
    let value = (kind << 8) | index;
    // SAFETY: upheld by the caller.
    unsafe {
        control(
            attached,
            direction::IN,
            request::GET_DESCRIPTOR,
            value,
            0,
            length,
        )
    }
}

/// Choose a configuration, which is what makes a device's endpoints usable.
///
/// # Safety
///
/// As [`control_in`].
pub unsafe fn set_configuration(attached: &mut Attached) -> Result<(), xhci::Trouble> {
    let value = u16::from(attached.device.configuration);
    // SAFETY: upheld by the caller.
    unsafe {
        control(
            attached,
            direction::OUT,
            request::SET_CONFIGURATION,
            value,
            0,
            0,
        )?;
    }
    Ok(())
}

/// One control transfer, all three stages.
///
/// # Safety
///
/// The device must be addressed and its control ring live. `length` must not be
/// more than the buffer is, which is a page.
unsafe fn control(
    attached: &mut Attached,
    direction: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u32,
) -> Result<u32, xhci::Trouble> {
    if length > 4096 {
        return Err(xhci::Trouble::NoMemory);
    }
    let reading = direction & direction::IN != 0;

    // The eight-byte setup packet, carried *inside* the TRB rather than pointed
    // at by it -- which is what the immediate-data bit means and is why a setup
    // stage needs no buffer.
    let setup = u64::from(direction)
        | (u64::from(request) << 8)
        | (u64::from(value) << 16)
        | (u64::from(index) << 32)
        | (u64::from(length as u16) << 48);

    // Transfer type: 0 for no data, 2 for writing to the device, 3 for reading
    // from it.
    let transfer_type: u32 = if length == 0 {
        0
    } else if reading {
        3
    } else {
        2
    };

    {
        let ring = &mut attached.control;
        // Setup: immediate data, eight bytes of it.
        ring.push(
            setup,
            8,
            (trb::SETUP << 10) | (1 << 6) | (transfer_type << 16),
        );
        if length > 0 {
            // Data: where it goes, how much, and which way.
            ring.push(
                attached.buffer,
                length,
                (trb::DATA << 10) | (u32::from(reading) << 16),
            );
        }
        // Status, the other way round from the data, and the only stage that
        // asks for an interrupt on completion -- so one event comes back for
        // the whole transfer rather than one per stage.
        ring.push(
            0,
            0,
            (trb::STATUS << 10) | (1 << 5) | (u32::from(!reading) << 16),
        );
    }

    // Endpoint zero's doorbell target is 1. The numbering is the endpoint's
    // context index, and endpoint zero's context is the first after the slot's.
    // SAFETY: upheld by the caller.
    unsafe { xhci::ring_doorbell(attached.device.slot, 1) };

    // SAFETY: a transfer was just started on this slot.
    unsafe { xhci::await_transfer(length) }
}

/// The input context frame, for a later Configure Endpoint.
impl Attached {
    #[must_use]
    pub const fn input(&self) -> u64 {
        self.input
    }
}

/// Bring up every device on every enabled port, and say what each one is.
///
/// This is where the layers meet: the controller says which ports have
/// something on them, and this asks each of those things what it is.
///
/// # Safety
///
/// The controller must be running and its ports surveyed.
pub unsafe fn enumerate() {
    if !xhci::is_present() {
        return;
    }
    for index in 0..xhci::port_count() {
        // SAFETY: the controller is up and the index is below its port count.
        let value = unsafe { xhci::read_port(index) };
        if value & xhci::port::CONNECTED == 0 || value & xhci::port::ENABLED == 0 {
            continue;
        }
        let speed = (value >> xhci::port::SPEED_SHIFT) & xhci::port::SPEED_MASK;

        // Ports are numbered from one everywhere the controller talks about
        // them, and from zero in this loop. The conversion happens here, once.
        // SAFETY: the port is enabled and the controller is running.
        let attached = match unsafe { attach(index + 1, speed) } {
            Ok(attached) => attached,
            Err(trouble) => {
                kprintln!("[usb ] the device on port {} would not come up: {trouble}", index + 1);
                continue;
            }
        };

        let device = &attached.device;
        kprintln!(
            "[usb ] port {}: {:04x}:{:04x}, class {:02x}.{:02x}.{:02x} in slot {}",
            device.port,
            device.vendor,
            device.product,
            device.class,
            device.subclass,
            device.protocol,
            device.slot
        );

        if device.class == MASS_STORAGE && device.subclass == SCSI && device.protocol == BULK_ONLY
        {
            let mut attached = attached;
            // SAFETY: the device is addressed and its control ring is live.
            match unsafe { crate::drivers::usb_storage::open(&mut attached) } {
                Ok(mut disk) => {
                    crate::drivers::usb_storage::describe(&disk);
                    // SAFETY: the disk was just opened.
                    unsafe { crate::drivers::usb_storage::verify(&mut attached, &mut disk) };
                    let index = ATTACHED.lock().len();
                    DISKS.lock().push((index, disk));
                }
                Err(trouble) => kprintln!("[usb ] it says it is a disk but would not open: {trouble}"),
            }
            // Held whether or not it opened: its rings are live either way, and
            // freeing them would hand the controller's memory back while it is
            // still reading it.
            ATTACHED.lock().push(attached);
            continue;
        }
        if device.class == MASS_STORAGE {
            // Named rather than driven. There are other mass-storage protocols
            // -- the old control/bulk/interrupt one, and UAS, which is SCSI
            // over a different shape of transfer -- and each is its own driver.
            kprintln!(
                "[usb ] it is storage, but subclass {:02x} protocol {:02x}, which this does not speak",
                device.subclass,
                device.protocol
            );
        }

        // Held for the life of the machine. The controller reads its rings and
        // contexts for as long as the device exists, and this driver has no
        // way to detach one yet.
        ATTACHED.lock().push(attached);
    }
}

/// Every device brought up, kept so its rings are never freed.
pub static ATTACHED: crate::sync::IrqSpinLock<alloc::vec::Vec<Attached>> =
    crate::sync::IrqSpinLock::new(alloc::vec::Vec::new());

/// The disks among them, paired with their index in [`ATTACHED`].
pub static DISKS: crate::sync::IrqSpinLock<
    alloc::vec::Vec<(usize, crate::drivers::usb_storage::Disk)>,
> = crate::sync::IrqSpinLock::new(alloc::vec::Vec::new());
