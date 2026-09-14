//! The PS/2 mouse.
//!
//! It shares a controller with the keyboard and almost nothing else. The same
//! two ports carry both, and which device a byte came from is told by a bit in
//! the status register rather than by where it was read — so this driver and
//! the keyboard's are two readers of one wire, and the interrupt is what says
//! whose turn it is.
//!
//! # Three bytes at a time
//!
//! The mouse sends packets of three: a byte of flags, then how far it moved
//! horizontally, then vertically, both as nine-bit signed numbers whose top bit
//! lives back in the flags. Movement is *relative*, always — a mouse does not
//! know where it is, only that it moved, and turning that into a position on a
//! screen is somebody else's job. That somebody is the compositor, because
//! where the pointer is depends on how large the screen is and what is on it,
//! and neither is the kernel's business.
//!
//! The packet has no framing. If a byte is dropped the next three bytes are
//! read as a packet that begins in the middle of the last one, and the pointer
//! flies off. The only defence the protocol offers is bit 3 of the first byte,
//! which is always set — so a first byte without it is treated as garbage and
//! thrown away rather than accepted, which is how the stream resynchronises.
//!
//! # What is absent
//!
//! No scroll wheel and no fourth and fifth buttons. Both need the device put
//! into a different mode by a magic sequence of sample rates, and both change
//! the packet length — so adding them is a change to how every packet is read,
//! not an extra field, and there is nothing yet that would use them.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::io::{inb, outb};
use crate::sync::IrqSpinLock;

/// Shared with the keyboard, which is the whole complication.
const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const COMMAND_PORT: u16 = 0x64;

/// The output buffer holds a byte.
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
/// The input buffer is still full, so the controller is not ready to be told.
const STATUS_INPUT_FULL: u8 = 1 << 1;
/// The byte waiting came from the auxiliary device rather than the keyboard.
///
/// This bit is the only thing that separates the two devices, and reading the
/// data port without checking it is how a mouse packet ends up decoded as a
/// scancode.
const STATUS_FROM_MOUSE: u8 = 1 << 5;

/// The interrupt firmware wires the auxiliary device to.
pub const MOUSE_IRQ: u8 = 12;

/// Controller commands.
mod command {
    /// Turn the auxiliary device on.
    pub const ENABLE_AUXILIARY: u8 = 0xA8;
    /// Read the controller's configuration byte.
    pub const READ_CONFIGURATION: u8 = 0x20;
    /// Write it back.
    pub const WRITE_CONFIGURATION: u8 = 0x60;
    /// The next byte written to the data port goes to the mouse, not the
    /// keyboard.
    pub const TO_MOUSE: u8 = 0xD4;
}

/// Bits of the controller's configuration byte.
mod configuration {
    /// Raise an interrupt when the auxiliary device has a byte.
    pub const MOUSE_INTERRUPT: u8 = 1 << 1;
    /// Stop the controller ignoring the auxiliary device entirely.
    pub const MOUSE_DISABLED: u8 = 1 << 5;
}

/// Commands to the mouse itself.
mod device {
    /// Back to a known state: 100 samples a second, no scaling, reporting off.
    pub const SET_DEFAULTS: u8 = 0xF6;
    /// Start sending packets.
    pub const ENABLE_REPORTING: u8 = 0xF4;
    /// What the device answers a command with.
    pub const ACKNOWLEDGE: u8 = 0xFA;
}

/// Bits of a packet's first byte.
mod flags {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;
    /// Always set. A first byte without it is not a first byte.
    pub const ALWAYS: u8 = 1 << 3;
    pub const X_SIGN: u8 = 1 << 4;
    pub const Y_SIGN: u8 = 1 << 5;
    pub const X_OVERFLOW: u8 = 1 << 6;
    pub const Y_OVERFLOW: u8 = 1 << 7;
}

/// One thing the mouse did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Movement {
    /// How far it moved, in its own units, with up positive.
    ///
    /// The mouse reports Y increasing upwards and every screen has it
    /// increasing downwards. The flip happens where the pointer is turned into
    /// a position, not here, because here there is no screen to be upside down
    /// with respect to.
    pub dx: i32,
    pub dy: i32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

/// A packet being assembled.
struct Packet {
    bytes: [u8; 3],
    have: usize,
}

impl Packet {
    const fn new() -> Self {
        Self {
            bytes: [0; 3],
            have: 0,
        }
    }

    /// Add a byte, returning a movement when three make a packet.
    fn push(&mut self, byte: u8) -> Option<Movement> {
        // The stream has no framing, so the one bit that is always set in a
        // first byte is what resynchronises it. Without this a single dropped
        // byte would leave every subsequent packet built from the tail of one
        // and the head of the next, and the pointer would never recover.
        if self.have == 0 && byte & flags::ALWAYS == 0 {
            DESYNCHRONISED.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        self.bytes[self.have] = byte;
        self.have += 1;
        if self.have < 3 {
            return None;
        }
        self.have = 0;

        let first = self.bytes[0];
        // A movement too large to fit in nine bits is reported as an overflow
        // and the value is meaningless, so it is dropped rather than believed.
        // A pointer that jumped the width of the screen once is worse than one
        // that missed a fast flick.
        if first & (flags::X_OVERFLOW | flags::Y_OVERFLOW) != 0 {
            OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        Some(Movement {
            dx: sign_extend(self.bytes[1], first & flags::X_SIGN != 0),
            dy: sign_extend(self.bytes[2], first & flags::Y_SIGN != 0),
            left: first & flags::LEFT != 0,
            right: first & flags::RIGHT != 0,
            middle: first & flags::MIDDLE != 0,
        })
    }
}

/// Turn a byte and its sign bit into the nine-bit number they are together.
fn sign_extend(value: u8, negative: bool) -> i32 {
    if negative {
        i32::from(value) - 256
    } else {
        i32::from(value)
    }
}

/// The packet being assembled, and where finished ones go.
static PACKET: IrqSpinLock<Packet> = IrqSpinLock::new(Packet::new());
/// Where movements go once something has asked for them.
static ROUTE: IrqSpinLock<Option<alloc::sync::Arc<crate::ipc::Endpoint>>> = IrqSpinLock::new(None);

/// Bytes taken from the controller.
static BYTES: AtomicU64 = AtomicU64::new(0);
/// Complete packets.
static PACKETS: AtomicU64 = AtomicU64::new(0);
/// First bytes that were not first bytes.
static DESYNCHRONISED: AtomicU64 = AtomicU64::new(0);
/// Packets whose movement did not fit.
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);
/// Whether the device answered at bring-up.
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// What a movement looks like on the wire.
///
/// Eight bytes: two little-endian signed 32-bit numbers with the buttons folded
/// into the top of the second, so a movement is one fixed-size message. Fixed
/// because the reader is a program, and a length that never varies is one fewer
/// thing for it to get wrong.
pub mod wire {
    /// Bytes one movement takes.
    pub const SIZE: usize = 12;
    pub const LEFT: u32 = 1 << 0;
    pub const RIGHT: u32 = 1 << 1;
    pub const MIDDLE: u32 = 1 << 2;
}

/// Send movements down `endpoint` from now on.
pub fn route_to(endpoint: alloc::sync::Arc<crate::ipc::Endpoint>) {
    *ROUTE.lock() = Some(endpoint);
}

/// Wait for the controller to be ready to be written to.
///
/// Bounded, because a controller that never drains would otherwise hang the
/// boot -- and a machine with no mouse is a machine that should still start.
fn wait_to_write() -> bool {
    for _ in 0..100_000 {
        // SAFETY: reading the PS/2 status port.
        if unsafe { inb(STATUS_PORT) } & STATUS_INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Wait for a byte to be there, and take it.
fn read_byte() -> Option<u8> {
    for _ in 0..100_000 {
        // SAFETY: reading the PS/2 status and data ports.
        unsafe {
            if inb(STATUS_PORT) & STATUS_OUTPUT_FULL != 0 {
                return Some(inb(DATA_PORT));
            }
        }
        core::hint::spin_loop();
    }
    None
}

/// Send one command to the mouse and wait for it to acknowledge.
///
/// # Safety
///
/// Call during initialisation, before the interrupt is unmasked.
unsafe fn tell_mouse(byte: u8) -> bool {
    if !wait_to_write() {
        return false;
    }
    // SAFETY: the prefix tells the controller the next data byte is for the
    // auxiliary device rather than the keyboard.
    unsafe { outb(COMMAND_PORT, command::TO_MOUSE) };
    if !wait_to_write() {
        return false;
    }
    // SAFETY: writing the command byte itself.
    unsafe { outb(DATA_PORT, byte) };
    read_byte() == Some(device::ACKNOWLEDGE)
}

/// Bring the mouse up.
///
/// Reported and tolerated on failure. A machine with no pointing device is a
/// machine that still boots, and saying so beats refusing to start.
///
/// # Safety
///
/// Call once, during initialisation, before the interrupt is unmasked.
pub unsafe fn init() {
    // SAFETY: the standard 8042 sequence, on the controller's own ports.
    unsafe {
        // Turn the auxiliary port on before anything is asked of it.
        if !wait_to_write() {
            crate::kprintln!("[mouse] the controller never became ready; no pointer");
            return;
        }
        outb(COMMAND_PORT, command::ENABLE_AUXILIARY);

        // The configuration byte decides whether the controller raises an
        // interrupt for the mouse at all. Read, changed and written back rather
        // than assumed, because the other bits in it are the keyboard's and
        // writing a guess would turn the keyboard off.
        if !wait_to_write() {
            return;
        }
        outb(COMMAND_PORT, command::READ_CONFIGURATION);
        let Some(existing) = read_byte() else {
            crate::kprintln!("[mouse] the controller would not say how it was configured");
            return;
        };
        let wanted = (existing | configuration::MOUSE_INTERRUPT) & !configuration::MOUSE_DISABLED;
        if !wait_to_write() {
            return;
        }
        outb(COMMAND_PORT, command::WRITE_CONFIGURATION);
        if !wait_to_write() {
            return;
        }
        outb(DATA_PORT, wanted);

        // And the device. Defaults first, so that whatever state the firmware
        // left it in is not inherited.
        if !tell_mouse(device::SET_DEFAULTS) {
            crate::kprintln!("[mouse] no device answered; no pointer");
            return;
        }
        if !tell_mouse(device::ENABLE_REPORTING) {
            crate::kprintln!("[mouse] the device would not start reporting; no pointer");
            return;
        }
    }

    PRESENT.store(true, Ordering::Release);
    crate::kprintln!("[mouse] PS/2 mouse reporting on IRQ {MOUSE_IRQ}");
}

/// A byte arrived from the auxiliary device.
///
/// # Safety
///
/// Call only as the handler for the mouse's vector.
pub unsafe fn on_interrupt() {
    // SAFETY: reading the PS/2 status and data ports.
    let byte = unsafe {
        let status = inb(STATUS_PORT);
        if status & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // The controller is shared. A byte from the keyboard arriving on this
        // vector is not this driver's, and reading it would take it away from
        // the driver whose it is.
        if status & STATUS_FROM_MOUSE == 0 {
            return;
        }
        inb(DATA_PORT)
    };

    BYTES.fetch_add(1, Ordering::Relaxed);
    let Some(movement) = PACKET.lock().push(byte) else {
        return;
    };
    PACKETS.fetch_add(1, Ordering::Relaxed);

    // Sent from the handler rather than handed to a thread, because a movement
    // is twelve bytes and a channel send is a lock and a copy. A packet arrives
    // at most a hundred times a second.
    let Some(endpoint) = ROUTE.lock().clone() else {
        return;
    };

    let mut buttons = 0u32;
    if movement.left {
        buttons |= wire::LEFT;
    }
    if movement.right {
        buttons |= wire::RIGHT;
    }
    if movement.middle {
        buttons |= wire::MIDDLE;
    }

    let mut message = [0u8; wire::SIZE];
    message[0..4].copy_from_slice(&movement.dx.to_le_bytes());
    message[4..8].copy_from_slice(&movement.dy.to_le_bytes());
    message[8..12].copy_from_slice(&buttons.to_le_bytes());

    // A failure is dropped. A full queue means whoever is drawing the pointer
    // is not keeping up with the hand moving it, which is a thing that happens;
    // the movement is stale by the time it would be complained about.
    endpoint.send(&message, alloc::vec::Vec::new()).ok();
}

/// Whether a mouse answered at bring-up.
#[must_use]
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// Bytes, packets, resynchronisations and overflows.
#[must_use]
pub fn statistics() -> (u64, u64, u64, u64) {
    (
        BYTES.load(Ordering::Relaxed),
        PACKETS.load(Ordering::Relaxed),
        DESYNCHRONISED.load(Ordering::Relaxed),
        OVERFLOWS.load(Ordering::Relaxed),
    )
}
