//! xHCI: the controller a USB device is plugged into.
//!
//! This is the first of four layers between "there is a stick in the port" and
//! "the file on it can be read", and it is worth naming all four up front
//! because each is a driver in its own right:
//!
//! | this file | the host controller: rings, slots, transfers |
//! | `usb/mod.rs` | what any USB device is: descriptors, endpoints, configuration |
//! | `usb/storage.rs` | bulk-only transport, which is SCSI in USB packets |
//! | `usb/scsi.rs` | the commands a disk actually understands |
//!
//! The disk this machine already drives is virtio, which is one layer and a
//! ring of descriptors. That is the difference, and it is why the roadmap has
//! said "four layers, each of which is a driver on its own" since before any of
//! this existed.
//!
//! # Why xHCI and not the older ones
//!
//! UHCI and EHCI are simpler and are what a machine from 2005 has. They are not
//! a *simpler version* of this -- they are a different controller with a
//! different data structure, so writing one would be writing a driver that has
//! to be written again. Every machine built this decade has xHCI, and it is
//! what QEMU's `qemu-xhci` models.
//!
//! # The shape of it
//!
//! Everything the controller does is a **TRB**: a sixteen-byte Transfer Request
//! Block, with a type, some parameters and a cycle bit. They live in rings the
//! driver writes and the controller reads, and events come back on a ring the
//! controller writes and the driver reads.
//!
//! The cycle bit is how both sides know where the producer has got to without a
//! head pointer: the producer writes each TRB with the current cycle value and
//! flips that value every time it wraps, so the consumer reads until it sees
//! the wrong one. It is the whole synchronisation protocol and it is one bit.
//!
//! Three rings matter here:
//!
//! * the **command ring**, which the driver writes: enable a slot, address a
//!   device, configure an endpoint;
//! * the **event ring**, which the controller writes: what became of each of
//!   those, and what became of each transfer;
//! * a **transfer ring per endpoint**, which is where the actual data goes.
//!
//! # No interrupts yet
//!
//! Every wait here is a poll with a deadline. Interrupts would be better and
//! are a separate piece of work: this driver runs at start-up and during
//! explicit reads, not in a hot path, and a polled driver that works is worth
//! more than an interrupt-driven one that is half written. Every poll is
//! bounded, so a controller that stops answering costs a timeout and a line in
//! the log rather than a machine that stops.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::drivers::pci::{BaseAddress, Device};
use crate::kprintln;

/// The PCI class, subclass and interface of an xHCI controller.
///
/// Serial bus controller, USB, and the programming interface that says which
/// generation. `0x30` is xHCI; `0x20` is EHCI and `0x00` is UHCI, and this
/// refuses those by name rather than trying to drive them.
const CLASS: u8 = 0x0C;
const SUBCLASS: u8 = 0x03;
const XHCI: u8 = 0x30;

/// How long to wait for the controller to do anything, in microseconds.
///
/// The specification allows a reset half a second. This is generous on top of
/// that, because it is a bound on a broken controller rather than a schedule
/// for a working one.
const PATIENCE_US: u64 = 1_000_000;

/// Capability registers, at the start of the window.
mod capability {
    /// How many bytes of capability registers there are; the operational
    /// registers start after them.
    pub const LENGTH: u64 = 0x00;
    /// Structural parameters: how many slots, interrupters and ports.
    pub const PARAMS1: u64 = 0x04;
    /// Capability parameters: context size, and where the extended ones are.
    pub const CAPPARAMS1: u64 = 0x10;
    /// Offset of the doorbell array.
    pub const DOORBELL_OFFSET: u64 = 0x14;
    /// Offset of the runtime registers.
    pub const RUNTIME_OFFSET: u64 = 0x18;
}

/// Operational registers, relative to the operational base.
mod operational {
    pub const COMMAND: u64 = 0x00;
    pub const STATUS: u64 = 0x04;
    /// Page size the controller wants, as a bitmap of supported sizes.
    pub const PAGE_SIZE: u64 = 0x08;
    /// The first port's registers; each port has four words.
    pub const PORTS: u64 = 0x400;
    pub const PORT_STRIDE: u64 = 0x10;
}

/// Bits in the USB command register.
mod command {
    /// Run/Stop.
    pub const RUN: u32 = 1 << 0;
    /// Host controller reset.
    pub const RESET: u32 = 1 << 1;
}

/// Bits in the USB status register.
mod status {
    /// The controller has stopped.
    pub const HALTED: u32 = 1 << 0;
    /// Host system error.
    pub const HOST_ERROR: u32 = 1 << 2;
    /// The controller is not ready to be written to.
    pub const NOT_READY: u32 = 1 << 11;
}

/// Bits in a port's status and control register.
pub mod port {
    /// Something is plugged in.
    pub const CONNECTED: u32 = 1 << 0;
    /// The port is enabled, which for USB 3 happens by itself after a connect
    /// and for USB 2 happens after a reset.
    pub const ENABLED: u32 = 1 << 1;
    /// Reset this port.
    pub const RESET: u32 = 1 << 4;
    /// Port power.
    pub const POWER: u32 = 1 << 9;
    /// Connect status changed.
    pub const CONNECT_CHANGE: u32 = 1 << 17;
    /// Reset finished.
    pub const RESET_CHANGE: u32 = 1 << 21;
    /// The bits that are cleared by writing one to them.
    ///
    /// Named because writing the register back unchanged would clear every
    /// change bit that happened to be set, which is how a driver loses a
    /// connect event it never noticed.
    pub const CHANGES: u32 = CONNECT_CHANGE | RESET_CHANGE | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 22);

    /// Where the speed lives in the register, and how wide it is.
    pub const SPEED_SHIFT: u32 = 10;
    pub const SPEED_MASK: u32 = 0b1111;
}

/// What a port's speed field means.
#[must_use]
pub fn speed_name(speed: u32) -> &'static str {
    match speed {
        1 => "full speed",
        2 => "low speed",
        3 => "high speed",
        4 => "SuperSpeed",
        5 => "SuperSpeed+",
        _ => "an unknown speed",
    }
}

/// Why the controller could not be brought up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// There is no xHCI controller on this machine.
    NotPresent,
    /// Its registers are in I/O space, which xHCI never uses.
    NotMemoryMapped,
    /// The window could not be mapped.
    WouldNotMap,
    /// It did not halt, or did not come out of reset, in time.
    Stuck(&'static str),
    /// It wants a page size this driver does not use.
    PageSize(u32),
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPresent => out.write_str("there is no xHCI controller on this machine"),
            Self::NotMemoryMapped => {
                out.write_str("the controller's registers are in I/O space, which xHCI never uses")
            }
            Self::WouldNotMap => out.write_str("its registers could not be mapped"),
            Self::Stuck(what) => write!(out, "the controller {what} in time"),
            Self::PageSize(bitmap) => write!(
                out,
                "the controller wants a page size this driver does not use ({bitmap:#x})"
            ),
        }
    }
}

/// Whether a controller has been found and started.
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Where its operational registers are, in the direct map.
static OPERATIONAL: AtomicU64 = AtomicU64::new(0);
/// And its runtime and doorbell registers.
static RUNTIME: AtomicU64 = AtomicU64::new(0);
static DOORBELLS: AtomicU64 = AtomicU64::new(0);
/// How many ports it has, and how many device slots.
static PORTS: AtomicU64 = AtomicU64::new(0);
static SLOTS: AtomicU64 = AtomicU64::new(0);
/// Whether its contexts are 64 bytes rather than 32.
static BIG_CONTEXTS: AtomicBool = AtomicBool::new(false);

/// Whether this machine has a working xHCI controller.
#[must_use]
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// How many ports it has.
#[must_use]
pub fn port_count() -> u64 {
    PORTS.load(Ordering::Relaxed)
}

/// Read an operational register.
///
/// # Safety
///
/// The controller must have been started, so the window is mapped.
pub unsafe fn read_operational(offset: u64) -> u32 {
    // SAFETY: the window is mapped uncached and the offset is inside it.
    unsafe { core::ptr::read_volatile((OPERATIONAL.load(Ordering::Relaxed) + offset) as *const u32) }
}

/// Write one.
///
/// # Safety
///
/// As [`read_operational`], and the value must mean what the register expects.
pub unsafe fn write_operational(offset: u64, value: u32) {
    // SAFETY: as above.
    unsafe {
        core::ptr::write_volatile(
            (OPERATIONAL.load(Ordering::Relaxed) + offset) as *mut u32,
            value,
        );
    }
}

/// Read a port's status and control register.
///
/// # Safety
///
/// The controller must have been started and `index` must be below
/// [`port_count`].
pub unsafe fn read_port(index: u64) -> u32 {
    // SAFETY: upheld by the caller.
    unsafe { read_operational(operational::PORTS + index * operational::PORT_STRIDE) }
}

/// Write one, without clearing the change bits by accident.
///
/// The change bits are "write one to clear". A driver that read the register,
/// set a bit and wrote it back would clear every change that had happened since
/// -- including a connect it has not looked at yet. So they are masked out here
/// and a caller that wants to acknowledge one says so.
///
/// # Safety
///
/// As [`read_port`].
pub unsafe fn write_port(index: u64, value: u32) {
    // SAFETY: upheld by the caller.
    unsafe {
        write_operational(
            operational::PORTS + index * operational::PORT_STRIDE,
            value & !port::CHANGES,
        );
    }
}

/// Acknowledge a port's change bits.
///
/// # Safety
///
/// As [`read_port`].
pub unsafe fn clear_port_changes(index: u64, which: u32) {
    // SAFETY: upheld by the caller. Only the named change bits are written, so
    // nothing else in the register is disturbed.
    unsafe {
        let value = read_port(index);
        write_operational(
            operational::PORTS + index * operational::PORT_STRIDE,
            (value & !port::CHANGES) | (which & port::CHANGES),
        );
    }
}

/// Find the controller, and bring it up.
///
/// Called once from the boot path. A machine with no controller is the ordinary
/// case and is reported rather than treated as a failure.
///
/// # Errors
///
/// [`Trouble`], every variant of which leaves the controller untouched or
/// halted.
pub fn start(devices: &[Device]) -> Result<(), Trouble> {
    let Some(device) = devices
        .iter()
        .find(|found| found.class == CLASS && found.subclass == SUBCLASS)
    else {
        return Err(Trouble::NotPresent);
    };
    if device.interface != XHCI {
        // An older controller. Named rather than driven: EHCI and UHCI are
        // different data structures, not simpler ones, and a driver that
        // half-understood one would be worse than a machine that says it has
        // nothing it can drive.
        kprintln!(
            "[usb ] the USB controller here is interface {:02x}, not xHCI; this drives xHCI only",
            device.interface
        );
        return Err(Trouble::NotPresent);
    }

    // SAFETY: the device came from this kernel's own PCI scan and nothing else
    // drives it.
    unsafe { device.enable() };

    // SAFETY: as above.
    let base = match unsafe { device.base_address(0) } {
        BaseAddress::Memory { base, .. } => base,
        BaseAddress::Port(_) => return Err(Trouble::NotMemoryMapped),
    };

    // The whole register file: capability, operational, runtime and doorbells.
    // Sixty-four kibibytes covers every layout this driver will meet, and the
    // offsets read below are checked against it.
    const WINDOW: u64 = 64 * 1024;
    // SAFETY: the window belongs to this controller, which nothing else drives.
    let Some(registers) = (unsafe { crate::memory::map_device_registers(base, WINDOW) }) else {
        return Err(Trouble::WouldNotMap);
    };

    // SAFETY: the window is mapped and these offsets are inside it.
    let (length, params1, capparams1, doorbell_offset, runtime_offset) = unsafe {
        (
            core::ptr::read_volatile((registers + capability::LENGTH) as *const u8) as u64,
            core::ptr::read_volatile((registers + capability::PARAMS1) as *const u32),
            core::ptr::read_volatile((registers + capability::CAPPARAMS1) as *const u32),
            core::ptr::read_volatile((registers + capability::DOORBELL_OFFSET) as *const u32),
            core::ptr::read_volatile((registers + capability::RUNTIME_OFFSET) as *const u32),
        )
    };

    let slots = u64::from(params1 & 0xFF);
    let ports = u64::from((params1 >> 24) & 0xFF);
    // Bit 2 of HCCPARAMS1: contexts are 64 bytes rather than 32. It changes the
    // stride of every structure below, so it is read before anything is built.
    let big_contexts = capparams1 & (1 << 2) != 0;

    OPERATIONAL.store(registers + length, Ordering::Relaxed);
    RUNTIME.store(registers + u64::from(runtime_offset & !0x1F), Ordering::Relaxed);
    DOORBELLS.store(registers + u64::from(doorbell_offset & !0x03), Ordering::Relaxed);
    PORTS.store(ports, Ordering::Relaxed);
    SLOTS.store(slots, Ordering::Relaxed);
    BIG_CONTEXTS.store(big_contexts, Ordering::Relaxed);

    kprintln!(
        "[usb ] xHCI at {base:#x}: {slots} device slots, {ports} ports, {}-byte contexts",
        if big_contexts { 64 } else { 32 }
    );

    // SAFETY: the window is mapped and this is the only driver touching it.
    unsafe { reset()? };

    // The page size the controller wants. Bit 0 means 4 KiB, which is the only
    // one this driver builds structures for -- every structure below is aligned
    // and sized in 4 KiB pages, and a controller wanting 64 KiB would need them
    // all rebuilt rather than a flag flipped.
    // SAFETY: the controller is reset and its registers are readable.
    let wanted = unsafe { read_operational(operational::PAGE_SIZE) };
    if wanted & 1 == 0 {
        return Err(Trouble::PageSize(wanted));
    }

    PRESENT.store(true, Ordering::Relaxed);
    Ok(())
}

/// Halt the controller and reset it.
///
/// # Safety
///
/// The register window must be mapped.
unsafe fn reset() -> Result<(), Trouble> {
    // Stop it first. Resetting a running controller is not defined, and the
    // firmware may well have left it running -- UEFI uses USB for the keyboard.
    // SAFETY: upheld by the caller.
    unsafe {
        let running = read_operational(operational::COMMAND);
        write_operational(operational::COMMAND, running & !command::RUN);
    }

    // SAFETY: as above.
    if !unsafe { wait_for(|| read_operational(operational::STATUS) & status::HALTED != 0) } {
        return Err(Trouble::Stuck("did not halt"));
    }

    // SAFETY: as above.
    unsafe { write_operational(operational::COMMAND, command::RESET) };

    // The reset bit clears itself when the reset is done, and `NOT_READY`
    // clears when the controller will accept writes again. Both, because the
    // first says the reset finished and the second says it is safe to talk to.
    // SAFETY: as above.
    if !unsafe { wait_for(|| read_operational(operational::COMMAND) & command::RESET == 0) } {
        return Err(Trouble::Stuck("did not come out of reset"));
    }
    // SAFETY: as above.
    if !unsafe { wait_for(|| read_operational(operational::STATUS) & status::NOT_READY == 0) } {
        return Err(Trouble::Stuck("did not become ready"));
    }

    // SAFETY: as above.
    let after = unsafe { read_operational(operational::STATUS) };
    if after & status::HOST_ERROR != 0 {
        return Err(Trouble::Stuck("reported a host error rather than resetting"));
    }
    Ok(())
}

/// Poll until `ready` says so, or the patience runs out.
///
/// # Safety
///
/// `ready` reads device registers, so the window must be mapped.
unsafe fn wait_for(ready: impl Fn() -> bool) -> bool {
    // A microsecond at a time, which on this machine is a `pause` and a read of
    // the timer. The controller answers a reset in tens of milliseconds, so the
    // common case spends a few thousand of these and the bad case ends.
    let deadline = crate::arch::time::uptime_us().saturating_add(PATIENCE_US);
    loop {
        if ready() {
            return true;
        }
        if crate::arch::time::uptime_us() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Power every port, reset what is plugged in, and say what came up.
///
/// Three things in one pass because they are one sequence:
///
/// 1. **Power.** A controller out of reset may have its ports unpowered, and a
///    port with no power reports nothing connected however full it is. So power
///    goes on first, and anything read before that is not evidence.
/// 2. **Reset**, for anything connected that is not already enabled. A USB 3
///    port enables itself when something is plugged in; a USB 2 port does not,
///    and stays disabled until it is reset. Resetting one that already enabled
///    itself would be undoing work.
/// 3. **Report**, which is what this milestone claims: the controller is up and
///    the machine can see what is attached, at what speed.
///
/// Returns how many ports have a usable device on them.
///
/// # Safety
///
/// The controller must have been started.
pub unsafe fn survey_ports() -> u64 {
    if !is_present() {
        return 0;
    }
    let ports = port_count();

    for index in 0..ports {
        // SAFETY: the controller is up and the index is below its port count.
        let value = unsafe { read_port(index) };
        if value & port::POWER == 0 {
            // SAFETY: as above. `write_port` masks the change bits, so this
            // cannot clear a connect nobody has looked at.
            unsafe { write_port(index, value | port::POWER) };
        }
    }
    // The specification asks for twenty milliseconds after powering a port
    // before its connect status means anything. Waited once for all of them
    // rather than once each, because they were powered together.
    crate::arch::time::spin_ms(20);

    let mut usable = 0;
    for index in 0..ports {
        // SAFETY: as above.
        let value = unsafe { read_port(index) };
        if value & port::CONNECTED == 0 {
            continue;
        }

        let mut value = value;
        if value & port::ENABLED == 0 {
            // SAFETY: as above.
            unsafe { write_port(index, value | port::RESET) };
            // SAFETY: as above. The reset bit clears itself when the port is
            // through, which is what says the device is ready to be addressed.
            let done = unsafe {
                wait_for(|| read_port(index) & port::RESET == 0)
            };
            if !done {
                kprintln!("[usb ] port {} did not finish resetting", index + 1);
                continue;
            }
            // SAFETY: as above.
            unsafe { clear_port_changes(index, port::RESET_CHANGE | port::CONNECT_CHANGE) };
            // SAFETY: as above.
            value = unsafe { read_port(index) };
        }

        let speed = (value >> port::SPEED_SHIFT) & port::SPEED_MASK;
        if value & port::ENABLED == 0 {
            kprintln!(
                "[usb ] port {} has something plugged in at {} but would not enable",
                index + 1,
                speed_name(speed)
            );
            continue;
        }
        usable += 1;
        kprintln!(
            "[usb ] port {} has a device on it at {}, enabled",
            index + 1,
            speed_name(speed)
        );
    }

    if usable == 0 {
        kprintln!("[usb ] {ports} ports, nothing usable plugged into any of them");
    }
    usable
}
