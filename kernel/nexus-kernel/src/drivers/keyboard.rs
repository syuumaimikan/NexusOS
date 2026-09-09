//! The PS/2 keyboard.
//!
//! NexusOS's first input device. USB is what a modern machine actually has, but
//! every PC still emulates a PS/2 keyboard for firmware and early boot, and it
//! needs no USB stack, no PCI enumeration and no DMA — two port addresses and an
//! interrupt. It is the shortest path from "the system runs" to "a person can
//! interact with it", and it stays useful afterwards as the fallback when the
//! USB stack is not up.
//!
//! # Scancodes
//!
//! The controller reports *scancodes*, not characters: which key changed and
//! whether it went down or up. Translating those into text is the kernel's job,
//! and it is a layout question — the table here is US QWERTY. A real system
//! chooses the layout from settings, and the Japanese layout in particular
//! differs enough that it cannot be an afterthought. That belongs with the input
//! stack proper, above the driver.
//!
//! # What the handler does and does not do
//!
//! The interrupt handler reads one byte and pushes it into a queue. Everything
//! else — decoding, modifiers, delivering to whoever is listening — happens on a
//! thread. An interrupt handler runs with interrupts masked on this processor,
//! so the less it does the better, and decoding needs state that a handler
//! should not be taking a lock to reach.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::io::{inb, outb};
use crate::sync::IrqSpinLock;

/// Data port: scancodes are read here.
const DATA_PORT: u16 = 0x60;
/// Status and command port.
const STATUS_PORT: u16 = 0x64;

/// Status bit: the output buffer holds a byte to read.
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
/// The byte waiting came from the auxiliary device rather than this one.
const STATUS_FROM_MOUSE: u8 = 1 << 5;

/// The legacy IRQ a PS/2 keyboard raises.
pub const KEYBOARD_IRQ: u8 = 1;

/// How many scancodes can be waiting before the oldest is dropped.
///
/// Small on purpose. If decoding has fallen this far behind, the useful thing is
/// the *recent* keys, not a backlog; and a queue that grows without bound turns
/// a stuck consumer into an out-of-memory failure.
const QUEUE_CAPACITY: usize = 64;

/// A fixed-size ring of scancodes, written by the interrupt handler and read by
/// a thread.
struct ScancodeQueue {
    bytes: [u8; QUEUE_CAPACITY],
    read: usize,
    write: usize,
    length: usize,
}

impl ScancodeQueue {
    const fn new() -> Self {
        Self {
            bytes: [0; QUEUE_CAPACITY],
            read: 0,
            write: 0,
            length: 0,
        }
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.length == QUEUE_CAPACITY {
            // Drop the oldest rather than the newest: what someone just typed
            // matters more than what they typed while the queue was blocked.
            self.read = (self.read + 1) % QUEUE_CAPACITY;
            self.length -= 1;
            self.bytes[self.write] = byte;
            self.write = (self.write + 1) % QUEUE_CAPACITY;
            self.length += 1;
            return false;
        }

        self.bytes[self.write] = byte;
        self.write = (self.write + 1) % QUEUE_CAPACITY;
        self.length += 1;
        true
    }

    fn pop(&mut self) -> Option<u8> {
        if self.length == 0 {
            return None;
        }
        let byte = self.bytes[self.read];
        self.read = (self.read + 1) % QUEUE_CAPACITY;
        self.length -= 1;
        Some(byte)
    }
}

static QUEUE: IrqSpinLock<ScancodeQueue> = IrqSpinLock::new(ScancodeQueue::new());

/// Threads waiting for a scancode to arrive.
///
/// This is what lets the input thread cost nothing between keystrokes. It used
/// to wake fifty times a second to find an empty queue, on a system where a key
/// arrives a few times a minute.
static ARRIVALS: crate::sched::wait::WaitQueue = crate::sched::wait::WaitQueue::new();

/// Block until a scancode is waiting.
///
/// Returns as soon as the queue is non-empty. Spurious wake-ups are handled by
/// the queue itself, which re-checks rather than trusting that a wake-up means
/// what it hoped.
pub fn wait_for_scancode() {
    ARRIVALS.wait_until(|| QUEUE.lock().length > 0);
}

/// Threads currently waiting for a key.
#[must_use]
pub fn waiting_threads() -> usize {
    ARRIVALS.len()
}

/// Scancodes received since boot.
static RECEIVED: AtomicU64 = AtomicU64::new(0);
/// Scancodes dropped because the queue was full.
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Keys decoded since boot.
static DECODED: AtomicUsize = AtomicUsize::new(0);

/// A key, as the decoder understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character, with modifiers already applied.
    Character(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    /// A function key, numbered from 1.
    Function(u8),
    /// A key this decoder does not name, kept so nothing is silently lost.
    Unknown(u8),
}

/// Modifier state, which is what makes decoding stateful.
#[derive(Default)]
struct Modifiers {
    shift: bool,
    control: bool,
    alt: bool,
    caps_lock: bool,
    /// The previous byte was 0xE0, so this one is from the extended set.
    extended: bool,
}

static MODIFIERS: IrqSpinLock<Modifiers> = IrqSpinLock::new(Modifiers {
    shift: false,
    control: false,
    alt: false,
    caps_lock: false,
    extended: false,
});

/// Unshifted characters for scancode set 1, indexed by scancode.
///
/// Zero means the scancode is not a character key; those are handled by name.
#[rustfmt::skip]
const UNSHIFTED: [u8; 0x40] = [
    0,    0,    b'1', b'2', b'3', b'4', b'5', b'6',
    b'7', b'8', b'9', b'0', b'-', b'=', 0,    0,
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i',
    b'o', b'p', b'[', b']', 0,    0,    b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';',
    b'\'', b'`', 0,   b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0,    b'*',
    0,    b' ', 0,    0,    0,    0,    0,    0,
];

/// Shifted characters, in the same order.
#[rustfmt::skip]
const SHIFTED: [u8; 0x40] = [
    0,    0,    b'!', b'@', b'#', b'$', b'%', b'^',
    b'&', b'*', b'(', b')', b'_', b'+', 0,    0,
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I',
    b'O', b'P', b'{', b'}', 0,    0,    b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':',
    b'"', b'~', 0,    b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0,    b'*',
    0,    b' ', 0,    0,    0,    0,    0,    0,
];

/// Read one scancode and queue it. Called from the interrupt handler.
///
/// # Safety
///
/// Call only from the keyboard interrupt handler: it reads the controller's
/// output buffer, which is a destructive read.
pub unsafe fn on_interrupt() {
    // SAFETY: the PS/2 status and data ports; reading data is what clears the
    // controller's interrupt.
    let byte = unsafe {
        let status = inb(STATUS_PORT);
        if status & STATUS_OUTPUT_FULL == 0 {
            // Nothing to read. Some controllers share the line, so this is not
            // an error.
            return;
        }
        // And the byte may not be this device's. One controller carries both
        // the keyboard and the mouse, and bit 5 is the only thing that says
        // which -- so a handler that read without checking would take the
        // other driver's packet and decode it as a scancode. It also would not
        // be there when that driver looked, which is the harder half of the
        // bug to see.
        if status & STATUS_FROM_MOUSE != 0 {
            return;
        }
        inb(DATA_PORT)
    };

    RECEIVED.fetch_add(1, Ordering::Relaxed);
    if !QUEUE.lock().push(byte) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }

    // Waking from an interrupt handler is safe here: it takes two short locks
    // and never switches. It must also happen after the push and outside the
    // queue's lock, so that a thread woken by it finds the scancode already
    // there rather than deciding it was a spurious wake-up.
    ARRIVALS.wake_one();
}

/// Take the next decoded key, if one is ready.
///
/// Returns `None` when no scancode is waiting, and also for the scancodes that
/// carry no key of their own: key releases, and the 0xE0 prefix.
pub fn next_key() -> Option<Key> {
    let byte = QUEUE.lock().pop()?;
    let mut modifiers = MODIFIERS.lock();

    // 0xE0 introduces the extended set; it names no key by itself.
    if byte == 0xE0 {
        modifiers.extended = true;
        return None;
    }
    let extended = core::mem::replace(&mut modifiers.extended, false);

    // The top bit distinguishes a release from a press.
    let released = byte & 0x80 != 0;
    let code = byte & 0x7F;

    // Modifiers are tracked on both press and release; everything else is
    // reported on press only, because a key that produced a character on the
    // way down would otherwise produce it again on the way up.
    match code {
        0x2A | 0x36 => {
            modifiers.shift = !released;
            return None;
        }
        0x1D => {
            modifiers.control = !released;
            return None;
        }
        0x38 => {
            modifiers.alt = !released;
            return None;
        }
        0x3A => {
            if !released {
                modifiers.caps_lock = !modifiers.caps_lock;
            }
            return None;
        }
        _ => {}
    }

    if released {
        return None;
    }

    let key = match code {
        0x01 => Key::Escape,
        0x0E => Key::Backspace,
        0x0F => Key::Tab,
        0x1C => Key::Enter,
        0x3B..=0x44 => Key::Function(code - 0x3B + 1),
        0x57 => Key::Function(11),
        0x58 => Key::Function(12),
        _ if extended => Key::Unknown(code),
        _ => {
            let index = code as usize;
            if index >= UNSHIFTED.len() {
                return Some(Key::Unknown(code));
            }

            let base = if modifiers.shift {
                SHIFTED[index]
            } else {
                UNSHIFTED[index]
            };
            if base == 0 {
                return Some(Key::Unknown(code));
            }

            // Caps lock applies to letters only, and inverts rather than
            // overrides the shift state.
            let character = if modifiers.caps_lock && base.is_ascii_alphabetic() {
                if modifiers.shift {
                    base.to_ascii_lowercase()
                } else {
                    base.to_ascii_uppercase()
                }
            } else {
                base
            };
            Key::Character(character as char)
        }
    };

    DECODED.fetch_add(1, Ordering::Relaxed);
    Some(key)
}

/// Drain and discard whatever the controller has buffered.
///
/// Firmware often leaves a byte or two in the output buffer. Until it is read
/// the controller will not raise another interrupt, so a keyboard that seems
/// dead from the first keypress is the usual symptom of skipping this.
///
/// # Safety
///
/// Call during initialisation, before the interrupt is unmasked.
pub unsafe fn drain_controller() {
    // Bounded: a controller that always reports data would otherwise spin here
    // forever.
    for _ in 0..16 {
        // SAFETY: reading the PS/2 status and data ports.
        unsafe {
            if inb(STATUS_PORT) & STATUS_OUTPUT_FULL == 0 {
                return;
            }
            let _ = inb(DATA_PORT);
        }
    }
}

/// Enable scanning on the keyboard itself.
///
/// # Safety
///
/// Call during initialisation.
pub unsafe fn enable_scanning() {
    /// Tell the keyboard to start sending scancodes.
    const COMMAND_ENABLE_SCANNING: u8 = 0xF4;

    // SAFETY: writing a keyboard command to the data port, having waited for
    // the controller's input buffer to drain.
    unsafe {
        for _ in 0..1000 {
            // Bit 1 of the status: the input buffer is still full.
            if inb(STATUS_PORT) & (1 << 1) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(DATA_PORT, COMMAND_ENABLE_SCANNING);
    }
}

/// Scancodes received, scancodes dropped, and keys decoded.
#[must_use]
pub fn statistics() -> (u64, u64, usize) {
    (
        RECEIVED.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
        DECODED.load(Ordering::Relaxed),
    )
}
