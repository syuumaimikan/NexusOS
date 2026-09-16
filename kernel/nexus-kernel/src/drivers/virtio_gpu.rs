//! A GPU driver: virtio-gpu, in two dimensions.
//!
//! # What this is, and what it is not
//!
//! "GPU driver" covers two very different things, and running them together is
//! what kept this box unticked for too long.
//!
//! One is a **display driver**: find the device, negotiate with it, allocate a
//! framebuffer, tell the device that framebuffer is a scanout, and hand it
//! rectangles that have changed. That is a driver, it is what this file is, and
//! there was never a good reason not to have one.
//!
//! The other is **3D**: shaders, a command stream in the device's own
//! instruction set, a memory manager, and an implementation of OpenGL or Vulkan
//! above it. On real hardware that is a per-vendor instruction set and a
//! firmware blob; on virtio it is `virgl`, which is a full GL implementation on
//! the guest side. Either is millions of lines and neither is close. Saying
//! "GPU driver" for the first and meaning the second is how one honest "no"
//! turned into a wrong one.
//!
//! So: this displays. It does not accelerate anything, and nothing here will
//! ever make a Vulkan call go faster, because there is nothing here that knows
//! what one is.
//!
//! # The modern transport
//!
//! The block and network drivers on this machine use virtio's *legacy*
//! transport: a window of I/O ports at a fixed layout. virtio-gpu has no legacy
//! form -- it was defined after the 1.0 specification -- so this is the first
//! driver here to speak the modern one, and most of the length of this file is
//! that rather than graphics.
//!
//! Modern virtio puts nothing at a fixed place. The device's registers are
//! described by *capabilities* in its PCI configuration space: each says which
//! base address register a structure lives in, at what offset, and how long it
//! is. There are four that matter -- the common configuration, the notify
//! window, the interrupt status, and the device's own configuration -- and
//! finding them is the first thing below.
//!
//! # How it is known to work
//!
//! Every command is answered by the device, and the answer is a type code the
//! driver did not write. `VIRTIO_GPU_RESP_OK_NODATA` coming back from a scanout
//! that was never set is not something this code can produce on its own, and
//! `GET_DISPLAY_INFO` returns a width and a height that came from the host.
//! A driver that sent nothing and reported success would have to invent those,
//! and would get them wrong.

use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use nexus_abi::layout;

use crate::drivers::pci::{BaseAddress, Device};
use crate::kprintln;
use crate::memory;
use crate::sync::IrqSpinLock;

/// virtio's vendor, and the GPU's device identifier.
///
/// Modern virtio numbers its PCI devices 0x1040 plus the virtio device type,
/// and the GPU is type 16.
const VIRTIO_VENDOR: u16 = 0x1AF4;
const GPU_DEVICE: u16 = 0x1040 + 16;

/// The capability identifier every virtio structure uses: vendor specific.
const PCI_CAP_VENDOR: u8 = 0x09;

/// Which structure a virtio capability describes.
mod structure {
    pub const COMMON: u8 = 1;
    pub const NOTIFY: u8 = 2;
    pub const ISR: u8 = 3;
    pub const DEVICE: u8 = 4;
}

/// Offsets inside the common configuration structure.
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0C;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_ENABLE: u64 = 0x1C;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1E;
    pub const QUEUE_DESCRIPTORS: u64 = 0x20;
    pub const QUEUE_AVAILABLE: u64 = 0x28;
    pub const QUEUE_USED: u64 = 0x30;
}

/// The status ladder, which is a protocol and not a set of flags.
mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const FAILED: u8 = 128;
}

/// Bit 32: "this device speaks the 1.0 specification". Not optional here --
/// a device that does not offer it is one this driver cannot talk to at all.
const VERSION_1: u32 = 1; // bit 0 of the second word of features

/// What the control queue carries.
mod command {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
}

/// What comes back.
mod answer {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
}

/// Eight-bit blue, green, red and an ignored byte -- the order the framebuffer
/// on this machine is already in, so nothing has to be swizzled on the way.
const FORMAT_BGRX: u32 = 2;

/// Flags in the first field of an available ring.
mod available_flag {
    /// The driver does not want an interrupt when this queue is used.
    ///
    /// Advice rather than a command -- the specification lets a device ignore
    /// it -- but QEMU honours it, and a device that ignores it leaves a driver
    /// no worse off than not setting it at all.
    pub const NO_INTERRUPT: u16 = 1;
}

/// Bytes in a control header: type, flags, fence, context, padding.
const HEADER: usize = 24;
/// The one resource this driver makes.
const SCREEN: u32 = 1;

/// Descriptors in the control queue.
///
/// Three at a time are used -- the command, the response, and nothing else --
/// so this is generous. It has to be a power of two and no larger than what the
/// device offers, both of which are checked at bring-up.
const QUEUE_SIZE: u16 = 64;

/// Flags on a descriptor.
mod descriptor {
    pub const NEXT: u16 = 1;
    pub const WRITE: u16 = 2;
}

/// One descriptor, as the device reads it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

/// Where everything is, once the device has been brought up.
#[derive(Clone, Copy)]
struct Gpu {
    /// The common configuration structure, mapped.
    common: u64,
    /// Where to write to tell the device a queue has something in it.
    notify: u64,
    /// Descriptor table, available ring and used ring, as this kernel sees
    /// them. Their physical addresses are given to the device once, at
    /// bring-up, and never wanted again -- so they are not kept.
    queue: u64,
    available: u64,
    used: u64,
    /// One page for a command and its answer.
    scratch: u64,
    scratch_physical: u64,
    /// The framebuffer the device scans out of.
    frame: u64,
    frame_physical: u64,
    width: u32,
    height: u32,
    /// The last used-ring index this driver saw.
    seen: u16,
    /// The interrupt status register, read only to acknowledge.
    isr: Option<u64>,
}

// SAFETY: every field is a plain number, and all access goes through the lock
// below; the device is driven from one place at a time on purpose.
unsafe impl Send for Gpu {}

static GPU: IrqSpinLock<Option<Gpu>> = IrqSpinLock::new(None);
/// Whether a flush is in progress, so two callers cannot share the queue.
static BUSY: AtomicBool = AtomicBool::new(false);
/// Commands sent and rectangles flushed, for the monitor.
static COMMANDS: AtomicU64 = AtomicU64::new(0);
static FLUSHES: AtomicU64 = AtomicU64::new(0);

/// Whether a GPU is present, and whether the device still says it is running.
///
/// Asked of the device rather than remembered. A device that has faulted sets
/// `NEEDS_RESET` in its own status register, and a driver that only remembered
/// having brought one up would go on reporting a working display.
#[must_use]
pub fn is_present() -> bool {
    let Some(gpu) = *GPU.lock() else {
        return false;
    };
    /// Bit 6: the device has had a failure and wants resetting.
    const NEEDS_RESET: u8 = 64;
    // SAFETY: the common structure was mapped at bring-up and this reads one
    // byte of it.
    let state = unsafe { get8(gpu.common + common::DEVICE_STATUS) };
    state & status::DRIVER_OK != 0 && state & NEEDS_RESET == 0
}

/// Commands the device answered, and rectangles flushed.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        COMMANDS.load(Ordering::Relaxed),
        FLUSHES.load(Ordering::Relaxed),
    )
}

/// Read and write a mapped register.
///
/// Volatile, because these are not memory: a read has a side effect on the
/// device and the compiler must not decide it already knows the answer.
///
/// # Safety
///
/// `at` must be inside a structure this driver has mapped.
unsafe fn get8(at: u64) -> u8 {
    unsafe { core::ptr::read_volatile(at as *const u8) }
}
unsafe fn put8(at: u64, value: u8) {
    unsafe { core::ptr::write_volatile(at as *mut u8, value) }
}
unsafe fn get16(at: u64) -> u16 {
    unsafe { core::ptr::read_volatile(at as *const u16) }
}
unsafe fn put16(at: u64, value: u16) {
    unsafe { core::ptr::write_volatile(at as *mut u16, value) }
}
unsafe fn get32(at: u64) -> u32 {
    unsafe { core::ptr::read_volatile(at as *const u32) }
}
unsafe fn put32(at: u64, value: u32) {
    unsafe { core::ptr::write_volatile(at as *mut u32, value) }
}
unsafe fn put64(at: u64, value: u64) {
    // In two halves, low first. The specification requires it: a device may
    // latch the high half when the low one is written, and one 64-bit store is
    // not guaranteed to reach the device as two ordered 32-bit ones.
    unsafe {
        put32(at, value as u32);
        put32(at + 4, (value >> 32) as u32);
    }
}

/// Where one of the four structures lives, from its capability.
#[derive(Clone, Copy)]
struct Window {
    bar: u8,
    offset: u32,
    length: u32,
    /// Only the notify capability has this: how far apart two queues' notify
    /// addresses are.
    multiplier: u32,
}

/// Find the GPU, bring it up, and give it a screen to scan out of.
///
/// Returns whether one was found and started.
///
/// # Safety
///
/// Call once, after PCI enumeration, with the frame allocator and the kernel's
/// page tables running.
pub unsafe fn init(devices: &[Device]) -> bool {
    let Some(device) = devices
        .iter()
        .find(|device| device.vendor == VIRTIO_VENDOR && device.device == GPU_DEVICE)
    else {
        return false;
    };

    // SAFETY: the device answered enumeration, so its configuration space is
    // readable, and this is the only driver touching it. `enable` turns on bus
    // mastering, without which the device cannot read the rings.
    unsafe { device.enable() };
    // And do not take its interrupt.
    //
    // This driver polls its control queue. `NO_INTERRUPT` in the available ring
    // asks the device not to interrupt and the specification lets it decline;
    // this does not ask.
    //
    // SAFETY: this is that device's driver, and nothing here waits on its
    // interrupt.
    unsafe { device.disable_interrupts() };

    // The four structures, from the capability list. Modern virtio puts nothing
    // at a fixed place, so a device whose capabilities are missing one of these
    // is one this cannot drive -- said rather than guessed at.
    let mut common = None;
    let mut notify = None;
    let mut isr = None;
    let mut config = None;
    // SAFETY: as above.
    unsafe {
        device.capabilities::<()>(|at, identifier| {
            if identifier != PCI_CAP_VENDOR {
                return None;
            }
            let kind = (crate::drivers::pci::read_config(device.address, at) >> 24) as u8;
            let bar = (crate::drivers::pci::read_config(device.address, at + 4) & 0xFF) as u8;
            let offset = crate::drivers::pci::read_config(device.address, at + 8);
            let length = crate::drivers::pci::read_config(device.address, at + 12);
            let multiplier = if kind == structure::NOTIFY {
                crate::drivers::pci::read_config(device.address, at + 16)
            } else {
                0
            };
            let window = Window {
                bar,
                offset,
                length,
                multiplier,
            };
            match kind {
                structure::COMMON => common = Some(window),
                structure::NOTIFY => notify = Some(window),
                structure::ISR => isr = Some(window),
                structure::DEVICE => config = Some(window),
                _ => {}
            }
            // Never stops early: all four are wanted, and the list is short.
            None
        });
    }
    let (Some(common_window), Some(notify_window)) = (common, notify) else {
        kprintln!("[gpu ] the virtio GPU has no common or notify capability; not driving it");
        return false;
    };
    let _ = config;

    // The interrupt status register, which this driver needs for one reason
    // only: to acknowledge.
    //
    // Nothing here waits on an interrupt -- the control queue is polled. But a
    // device that raises one on a level-triggered pin holds that pin until
    // somebody reads this register, and the driver sharing the pin reads its
    // *own* device's register, not this one's, so the line is never released.
    // Telling the device not to interrupt (`NO_INTERRUPT`, at the queue) stops
    // new causes; it cannot clear a cause that is already there, and a
    // configuration change at bring-up is one.
    //
    // Optional, because a device without this capability cannot raise an
    // interrupt through it either.
    let isr = isr.and_then(|window| {
        // SAFETY: the window named belongs to this device.
        unsafe { map_window(device, window) }
    });
    if isr.is_none() {
        // Worth saying out loud rather than carrying on quietly. Without this
        // register the driver cannot release the interrupt line, and if the
        // device ever raises one the machine will take it for ever.
        kprintln!("[gpu ] no interrupt status register; the GPU cannot release its line");
    }

    // SAFETY: as above; the windows named belong to this device.
    let Some(common) = (unsafe { map_window(device, common_window) }) else {
        kprintln!("[gpu ] could not map the virtio GPU's common configuration");
        return false;
    };
    let Some(notify) = (unsafe { map_window(device, notify_window) }) else {
        kprintln!("[gpu ] could not map the virtio GPU's notify window");
        return false;
    };

    // SAFETY: the structures are mapped and this driver is the only thing
    // touching the device.
    let started = unsafe { bring_up(common, notify, notify_window.multiplier, isr) };
    let Some(mut gpu) = started else {
        // SAFETY: as above. Telling the device the driver gave up is the last
        // thing the protocol asks for, and a device left half-configured is one
        // the next boot may find in a state it does not expect.
        unsafe { put8(common + common::DEVICE_STATUS, status::FAILED) };
        return false;
    };

    // SAFETY: the queue is live and the device answered.
    if !unsafe { make_screen(&mut gpu) } {
        kprintln!("[gpu ] the virtio GPU would not take a scanout");
        return false;
    }

    kprintln!(
        "[gpu ] virtio GPU at {}: {}x{} scanout, {} KiB framebuffer, resource {SCREEN} attached",
        device.address,
        gpu.width,
        gpu.height,
        (gpu.width as usize * gpu.height as usize * 4) / 1024
    );
    *GPU.lock() = Some(gpu);
    true
}

/// Map one capability's window, and say where it landed.
///
/// # Safety
///
/// The device must be one enumeration found, and the window must be its own.
unsafe fn map_window(device: &Device, window: Window) -> Option<u64> {
    // SAFETY: upheld by the caller.
    let base = match unsafe { device.base_address(window.bar) } {
        BaseAddress::Memory { base, .. } => base,
        // A virtio structure in an I/O window is legal in the specification and
        // is not what QEMU produces. Refused by name rather than mapped as
        // though it were memory, which would read rubbish.
        BaseAddress::Port(port) => {
            kprintln!("[gpu ] bar {} is an I/O window at {port:#x}", window.bar);
            return None;
        }
    };
    let at = base.checked_add(u64::from(window.offset))?;
    // SAFETY: the window belongs to this device, which nothing else drives.
    unsafe { memory::map_device_registers(at, u64::from(window.length).max(4096)) }
}

/// Walk the status ladder, negotiate, and set up the control queue.
///
/// # Safety
///
/// `common` and `notify` must be mapped structures of a virtio device nothing
/// else is driving.
unsafe fn bring_up(common: u64, notify: u64, multiplier: u32, isr: Option<u64>) -> Option<Gpu> {
    // SAFETY: upheld by the caller. The order below is the protocol: a driver
    // may not read features before saying DRIVER, may not touch a queue before
    // FEATURES_OK, and may not expect the device to work before DRIVER_OK.
    unsafe {
        put8(common + common::DEVICE_STATUS, 0);
        // Reset is complete when the status reads back as zero. Bounded,
        // because a device that never finishes resetting must be reported
        // rather than waited on for ever.
        let mut reset = false;
        for _ in 0..100_000 {
            if get8(common + common::DEVICE_STATUS) == 0 {
                reset = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !reset {
            kprintln!("[gpu ] the virtio GPU never came out of reset");
            return None;
        }

        put8(common + common::DEVICE_STATUS, status::ACKNOWLEDGE);
        put8(
            common + common::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER,
        );

        // Features come in 32-bit words chosen by a selector. Word one holds
        // bit 32, which is VERSION_1 -- the only feature this driver claims,
        // and the one without which a modern device refuses to work at all.
        put32(common + common::DEVICE_FEATURE_SELECT, 1);
        let offered = get32(common + common::DEVICE_FEATURE);
        if offered & VERSION_1 == 0 {
            kprintln!("[gpu ] the virtio GPU does not offer VERSION_1, so this cannot drive it");
            return None;
        }
        // Word nought: no optional feature is claimed. Every one of them
        // changes the layout or the meaning of something, and accepting one
        // this does not implement would be agreeing to a protocol it does not
        // speak.
        put32(common + common::DRIVER_FEATURE_SELECT, 0);
        put32(common + common::DRIVER_FEATURE, 0);
        put32(common + common::DRIVER_FEATURE_SELECT, 1);
        put32(common + common::DRIVER_FEATURE, VERSION_1);

        put8(
            common + common::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        // Read back, because the device may refuse. This check is the whole
        // reason FEATURES_OK exists as a separate step.
        if get8(common + common::DEVICE_STATUS) & status::FEATURES_OK == 0 {
            kprintln!("[gpu ] the virtio GPU would not accept the features asked for");
            return None;
        }

        if get16(common + common::NUM_QUEUES) < 1 {
            kprintln!("[gpu ] the virtio GPU has no queues");
            return None;
        }

        // The control queue, which is queue nought.
        put16(common + common::QUEUE_SELECT, 0);
        let offered_size = get16(common + common::QUEUE_SIZE);
        if offered_size == 0 {
            kprintln!("[gpu ] the virtio GPU's control queue has no room");
            return None;
        }
        let size = offered_size.min(QUEUE_SIZE);

        // Three regions, because the modern transport addresses them
        // separately -- which is the one simplification it makes over the
        // legacy layout's single block with padding rules.
        let (queue_physical, queue) = frame_pair()?;
        let (available_physical, available) = frame_pair()?;
        let (used_physical, used) = frame_pair()?;
        let (scratch_physical, scratch) = frame_pair()?;

        put64(common + common::QUEUE_DESCRIPTORS, queue_physical);
        put64(common + common::QUEUE_AVAILABLE, available_physical);
        put64(common + common::QUEUE_USED, used_physical);
        put16(common + common::QUEUE_SIZE, size);

        // Tell the device not to interrupt on this queue, before enabling it.
        //
        // This driver polls: a control command is answered in microseconds and
        // waiting for an interrupt to say so would cost more than the command.
        // Polling is the right choice here and it is not the whole choice --
        // **a driver that does not take a device's interrupt has to say so**,
        // and this one did not.
        //
        // What that cost: the line stays asserted, because nothing reads the
        // device's status register to acknowledge it, and a level-triggered pin
        // that is never acknowledged raises again immediately. Whichever
        // *other* driver shares that pin then takes the interrupt over and over
        // for a device it does not own.
        //
        // It was invisible while a VGA device sat in the first PCI slot, which
        // pushed the GPU onto a line nothing else used. Take the VGA away --
        // which is exactly what moving the desktop onto this GPU requires --
        // and every device moves up a slot, the GPU lands on the disk's line,
        // and a machine that was formatting a filesystem in three seconds does
        // not finish in four hundred. Measured: 883 interrupts from ring three
        // without the GPU on the bus, 141204 with it.
        //
        // SAFETY: the available ring was allocated above and this is its first
        // field. Written before `QUEUE_ENABLE`, so the device cannot have
        // looked at the ring yet. (The enclosing block is already unsafe.)
        (available as *mut u16).write_volatile(available_flag::NO_INTERRUPT);
        fence(Ordering::SeqCst);

        put16(common + common::QUEUE_ENABLE, 1);

        // Where to poke for this queue. The offset is in units the device
        // chooses, which is what the multiplier is for -- a driver that assumed
        // one unit was one byte would notify the wrong queue on a device with
        // more than one.
        let slot = u64::from(get16(common + common::QUEUE_NOTIFY_OFF));
        let notify = notify + slot * u64::from(multiplier);

        put8(
            common + common::DEVICE_STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );

        Some(Gpu {
            common,
            notify,
            queue,
            available,
            used,
            scratch,
            scratch_physical,
            isr,
            frame: 0,
            frame_physical: 0,
            width: 0,
            height: 0,
            seen: 0,
        })
    }
}

/// A zeroed frame, as both addresses.
fn frame_pair() -> Option<(u64, u64)> {
    let physical = memory::allocate_frame()?;
    let virt = layout::phys_to_virt(physical);
    // SAFETY: the frame was just allocated to this driver and nothing else
    // holds it. A ring the device reads must not start as whatever was there.
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, 4096) };
    Some((physical, virt))
}

/// Ask the device how big the display is, make a resource that size, give it
/// memory, and make it the scanout.
///
/// # Safety
///
/// The queue must be live and the device must have been told `DRIVER_OK`.
unsafe fn make_screen(gpu: &mut Gpu) -> bool {
    // What the host says the display is. This number is the first thing in this
    // file that this driver could not have invented.
    let mut reply = [0u8; 64];
    // SAFETY: upheld by the caller.
    let Some(kind) = (unsafe { ask(gpu, &header(command::GET_DISPLAY_INFO), &mut reply) }) else {
        return false;
    };
    if kind != answer::OK_DISPLAY_INFO {
        kprintln!("[gpu ] the GPU answered {kind:#x} to a request for the display size");
        return false;
    }
    // `virtio_gpu_resp_display_info`: the header, then sixteen displays of a
    // rectangle, an enabled flag and some flags. The first one is the screen.
    let width = u32::from_le_bytes([reply[32], reply[33], reply[34], reply[35]]);
    let height = u32::from_le_bytes([reply[36], reply[37], reply[38], reply[39]]);
    let enabled = u32::from_le_bytes([reply[40], reply[41], reply[42], reply[43]]);
    if width == 0 || height == 0 || enabled == 0 {
        kprintln!("[gpu ] the GPU reports no enabled display ({width}x{height})");
        return false;
    }
    gpu.width = width;
    gpu.height = height;

    // Memory for the pixels, contiguous because the device is given one address
    // and a length. Four bytes a pixel.
    let bytes = width as usize * height as usize * 4;
    let order = order_for(bytes);
    let Some(frame_physical) = memory::allocate_block(order) else {
        kprintln!("[gpu ] no memory for a {width}x{height} framebuffer");
        return false;
    };
    gpu.frame_physical = frame_physical;
    gpu.frame = layout::phys_to_virt(frame_physical);
    // SAFETY: the block was just allocated to this driver.
    unsafe { core::ptr::write_bytes(gpu.frame as *mut u8, 0, 4096 << order) };

    // A resource of that size, in the format the framebuffer is already in.
    let mut create = header(command::RESOURCE_CREATE_2D);
    create.extend_from_slice(&SCREEN.to_le_bytes());
    create.extend_from_slice(&FORMAT_BGRX.to_le_bytes());
    create.extend_from_slice(&width.to_le_bytes());
    create.extend_from_slice(&height.to_le_bytes());
    // SAFETY: as above.
    if !unsafe { expect_ok(gpu, &create, "creating the screen resource") } {
        return false;
    }

    // And the memory behind it: one entry, because the block is contiguous.
    let mut attach = header(command::RESOURCE_ATTACH_BACKING);
    attach.extend_from_slice(&SCREEN.to_le_bytes());
    attach.extend_from_slice(&1u32.to_le_bytes());
    attach.extend_from_slice(&frame_physical.to_le_bytes());
    attach.extend_from_slice(&(bytes as u32).to_le_bytes());
    attach.extend_from_slice(&0u32.to_le_bytes());
    // SAFETY: as above.
    if !unsafe { expect_ok(gpu, &attach, "attaching memory to the screen resource") } {
        return false;
    }

    // Then make it the thing the display shows.
    let mut scanout = header(command::SET_SCANOUT);
    scanout.extend_from_slice(&rectangle(0, 0, width, height));
    scanout.extend_from_slice(&0u32.to_le_bytes()); // the first display
    scanout.extend_from_slice(&SCREEN.to_le_bytes());
    // SAFETY: as above.
    unsafe { expect_ok(gpu, &scanout, "setting the scanout") }
}

/// Send everything in a rectangle to the display.
///
/// Two commands, and both are needed: the transfer copies the guest's memory
/// into the host's copy of the resource, and the flush tells the host that copy
/// has changed. A driver that sent only the first writes into a buffer nobody
/// looks at; one that sent only the second shows the previous frame again.
///
/// Returns whether the device took both.
/// Let go of the interrupt line.
///
/// Reading the interrupt status register is what acknowledges at the device.
/// This driver never wants the interrupt -- it polls -- but a level-triggered
/// pin that nobody reads stays asserted, and then whichever driver shares that
/// pin takes the same interrupt for ever. Measured before this existed: a
/// hundred and sixteen thousand interrupts a second on a machine whose timer
/// ticks a thousand times a second.
///
/// The value is discarded. There is no question to ask it: nothing here is
/// waiting for anything, and the read is the entire point.
///
/// # Safety
///
/// Call only from an interrupt handler for a vector this device's line is
/// routed to.
pub unsafe fn acknowledge() {
    let Some(isr) = GPU.lock().as_ref().and_then(|gpu| gpu.isr) else {
        return;
    };
    // SAFETY: the window belongs to this device and was mapped at bring-up;
    // reading this register is how the protocol says to acknowledge.
    unsafe {
        core::ptr::read_volatile(isr as *const u8);
    }
}

/// The GPU's scanout, described the way the firmware describes its own.
///
/// So that nothing above this has to know which it got. `display::init` takes a
/// [`FramebufferInfo`](nexus_abi::boot::FramebufferInfo) and paints into it;
/// the compositor is handed the same description and maps the same memory.
/// Neither of them has ever heard of virtio.
///
/// The stride is the width, because this driver allocated the memory and chose
/// not to pad it. A firmware framebuffer often is padded, which is why the
/// field exists at all.
#[must_use]
pub fn framebuffer() -> Option<nexus_abi::boot::FramebufferInfo> {
    let gpu = (*GPU.lock())?;
    Some(nexus_abi::boot::FramebufferInfo {
        phys_addr: gpu.frame_physical,
        size: u64::from(gpu.width) * u64::from(gpu.height) * 4,
        width: gpu.width,
        height: gpu.height,
        stride: gpu.width,
        bytes_per_pixel: 4,
        // What the resource was created with, and what everything on this
        // machine already draws in, so nothing is swizzled on the way.
        format: nexus_abi::boot::PixelFormat::Bgrx8888,
        _reserved: 0,
    })
}

pub fn flush(x: u32, y: u32, width: u32, height: u32) -> bool {
    let Some(mut gpu) = *GPU.lock() else {
        return false;
    };
    if width == 0 || height == 0 {
        return false;
    }
    // One at a time: there is one queue and one scratch page.
    if BUSY.swap(true, Ordering::Acquire) {
        return false;
    }

    let offset = (u64::from(y) * u64::from(gpu.width) + u64::from(x)) * 4;
    let mut transfer = header(command::TRANSFER_TO_HOST_2D);
    transfer.extend_from_slice(&rectangle(x, y, width, height));
    transfer.extend_from_slice(&offset.to_le_bytes());
    transfer.extend_from_slice(&SCREEN.to_le_bytes());
    transfer.extend_from_slice(&0u32.to_le_bytes());

    let mut show = header(command::RESOURCE_FLUSH);
    show.extend_from_slice(&rectangle(x, y, width, height));
    show.extend_from_slice(&SCREEN.to_le_bytes());
    show.extend_from_slice(&0u32.to_le_bytes());

    // SAFETY: the queue is live, the gate above is held, and both commands are
    // built here with the lengths the specification gives.
    let done = unsafe {
        expect_ok(&mut gpu, &transfer, "transferring a rectangle")
            && expect_ok(&mut gpu, &show, "flushing a rectangle")
    };

    // The index this driver has seen moves, so it has to go back.
    if let Some(held) = GPU.lock().as_mut() {
        held.seen = gpu.seen;
    }
    BUSY.store(false, Ordering::Release);
    if done {
        FLUSHES.fetch_add(1, Ordering::Relaxed);
    }
    done
}

/// A control header with nothing else in it.
fn header(kind: u32) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(HEADER);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flags: no fence wanted
    out.extend_from_slice(&0u64.to_le_bytes()); // fence identifier
    out.extend_from_slice(&0u32.to_le_bytes()); // context, which 2D does not use
    out.extend_from_slice(&0u32.to_le_bytes()); // padding
    debug_assert_eq!(out.len(), HEADER);
    out
}

/// `virtio_gpu_rect`: four little-endian words.
fn rectangle(x: u32, y: u32, width: u32, height: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&x.to_le_bytes());
    out[4..8].copy_from_slice(&y.to_le_bytes());
    out[8..12].copy_from_slice(&width.to_le_bytes());
    out[12..16].copy_from_slice(&height.to_le_bytes());
    out
}

/// Send a command and require the device to answer that it worked.
///
/// # Safety
///
/// As [`ask`].
unsafe fn expect_ok(gpu: &mut Gpu, request: &[u8], what: &str) -> bool {
    let mut reply = [0u8; 64];
    // SAFETY: upheld by the caller.
    match unsafe { ask(gpu, request, &mut reply) } {
        Some(answer::OK_NODATA) => true,
        Some(other) => {
            kprintln!("[gpu ] the GPU answered {other:#x} to {what}");
            false
        }
        None => {
            kprintln!("[gpu ] the GPU never answered {what}");
            false
        }
    }
}

/// Put one command on the queue, wait for the answer, and say what kind it was.
///
/// # Safety
///
/// The queue must be live, the device must have been told `DRIVER_OK`, and the
/// caller must hold the gate so that nothing else is using the scratch page.
unsafe fn ask(gpu: &mut Gpu, request: &[u8], reply: &mut [u8]) -> Option<u32> {
    /// Where in the scratch page the answer goes. The command is at the start,
    /// and nothing this driver sends is anywhere near this long.
    const ANSWER_AT: u64 = 2048;

    if request.len() > ANSWER_AT as usize || reply.len() < 4 {
        return None;
    }

    // SAFETY: upheld by the caller. The scratch page is this driver's and the
    // gate means nobody else has started with it.
    unsafe {
        core::ptr::copy_nonoverlapping(
            request.as_ptr(),
            gpu.scratch as *mut u8,
            request.len(),
        );
        core::ptr::write_bytes((gpu.scratch + ANSWER_AT) as *mut u8, 0, reply.len());

        // Two descriptors: what to do, and where to put the answer. Chained,
        // because the device reads the first and writes the second.
        let descriptors = gpu.queue as *mut Descriptor;
        descriptors.write_volatile(Descriptor {
            address: gpu.scratch_physical,
            length: request.len() as u32,
            flags: descriptor::NEXT,
            next: 1,
        });
        descriptors.add(1).write_volatile(Descriptor {
            address: gpu.scratch_physical + ANSWER_AT,
            length: reply.len() as u32,
            flags: descriptor::WRITE,
            next: 0,
        });

        // The available ring: flags, index, then the ring itself.
        let available = gpu.available as *mut u16;
        let index = available.add(1).read_volatile();
        available
            .add(2 + (index as usize % QUEUE_SIZE as usize))
            .write_volatile(0);

        // The device must see the descriptors and the ring entry before it sees
        // the new index, or it will follow a chain that is not there yet.
        fence(Ordering::SeqCst);
        available.add(1).write_volatile(index.wrapping_add(1));
        fence(Ordering::SeqCst);

        // And tell it. The queue number goes in the notify window, which for
        // this device and this queue is the address worked out at bring-up.
        put16(gpu.notify, 0);

        // Wait for the used ring's index to move past what was last seen.
        // Spun rather than blocked: this is a display command to an emulated
        // device and it answers in microseconds, and the first one of these
        // happens before there is a scheduler to block under.
        let used = gpu.used as *const u16;
        let mut answered = false;
        for _ in 0..50_000_000u32 {
            if used.add(1).read_volatile() != gpu.seen {
                answered = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !answered {
            return None;
        }
        gpu.seen = used.add(1).read_volatile();
        fence(Ordering::SeqCst);

        core::ptr::copy_nonoverlapping(
            (gpu.scratch + ANSWER_AT) as *const u8,
            reply.as_mut_ptr(),
            reply.len(),
        );
    }

    COMMANDS.fetch_add(1, Ordering::Relaxed);
    Some(u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]))
}

/// Draw a pattern into the scanout and show it.
///
/// Three bands of known colour, in the order red, green, blue from the top.
/// Known because that is what makes this checkable: the host can be asked for a
/// picture of what the GPU is displaying, and those three colours at those
/// three heights are not something that appears by accident. A driver that
/// created the resource, set the scanout and never transferred anything shows
/// black, and black is what an unconfigured display shows too -- so a blank
/// screen proves nothing and a red band proves the whole path.
///
/// Returns whether the device took the flush.
pub fn self_test() -> bool {
    let Some(gpu) = *GPU.lock() else {
        return false;
    };

    /// Opaque, in the format the resource was created with: blue, green, red,
    /// and a byte the device ignores. Written as one word, so the order in the
    /// literal is the reverse of the order in memory.
    const RED: u32 = 0x00FF_0000;
    const GREEN: u32 = 0x0000_FF00;
    const BLUE: u32 = 0x0000_00FF;

    let band = gpu.height / 3;
    // SAFETY: the framebuffer was allocated to this driver at bring-up and is
    // width * height * 4 bytes long; every index below is inside it.
    unsafe {
        let pixels = gpu.frame as *mut u32;
        for y in 0..gpu.height {
            let colour = if y < band {
                RED
            } else if y < band * 2 {
                GREEN
            } else {
                BLUE
            };
            for x in 0..gpu.width {
                pixels
                    .add((y as usize) * (gpu.width as usize) + x as usize)
                    .write_volatile(colour);
            }
        }
    }

    let shown = flush(0, 0, gpu.width, gpu.height);
    if shown {
        kprintln!(
            "[gpu ] drew three bands into the scanout and the GPU took them: {}x{}, band {band}",
            gpu.width,
            gpu.height
        );
    } else {
        kprintln!("[gpu ] the GPU would not show what was drawn into the scanout");
    }
    shown
}

/// The smallest order of pages that holds `bytes`.
fn order_for(bytes: usize) -> usize {
    let pages = bytes.div_ceil(4096).max(1);
    // `ceil(log2(pages))`, which for a power of two is its trailing zeros and
    // for anything else is one more than the highest bit set.
    usize::BITS as usize - (pages - 1).leading_zeros() as usize
}
