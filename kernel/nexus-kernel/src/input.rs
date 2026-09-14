//! Input handling.
//!
//! Turns decoded keys into something the system does, and passes a copy of each
//! one to whoever is routing.
//!
//! The kernel still acts on keys itself, because the panel is still the
//! kernel's: what was typed appears there, and F1 changes the interface
//! language. What it does *not* do is decide which program a key is for. A copy
//! of every key crosses a channel, and whoever holds the other end -- the
//! compositor -- knows which window has focus and sends it on. That is policy,
//! and policy in the kernel is the thing this system is trying not to have.
//!
//! It exists mainly so that the system is *interactive*. Until a key could
//! change something, the language had to cycle on a timer to show that switching
//! worked at all; now it switches because someone asked.

use alloc::string::String;

use crate::drivers::keyboard::{self, Key};
use crate::sync::IrqSpinLock;
use crate::{i18n, kprintln, sched};

/// Longest line kept, in characters.
///
/// The line is a demonstration surface, not a text editor. Capping it keeps a
/// held-down key from growing an allocation without bound.
const MAX_LINE: usize = 48;

/// What has been typed since the last Enter or Escape.
static LINE: IrqSpinLock<String> = IrqSpinLock::new(String::new());

/// Keys acted on since boot.
static HANDLED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The current line.
#[must_use]
pub fn line() -> String {
    LINE.lock().clone()
}

/// Keys acted on since boot.
#[must_use]
pub fn handled_count() -> u64 {
    HANDLED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Where keys go besides the panel, once something has asked for them.
///
/// The compositor, in practice. The kernel goes on decoding scancodes and
/// showing what was typed, because the panel is still the kernel's; what
/// changes is that a copy of every key also crosses a channel, and whoever
/// holds the other end decides which program it is for. Routing is not the
/// kernel's business -- knowing which window has focus is exactly the kind of
/// policy a process should own.
static ROUTE: crate::sync::IrqSpinLock<Option<alloc::sync::Arc<crate::ipc::Endpoint>>> =
    crate::sync::IrqSpinLock::new(None);

/// Send a copy of every key down `endpoint` from now on.
pub fn route_to(endpoint: alloc::sync::Arc<crate::ipc::Endpoint>) {
    *ROUTE.lock() = Some(endpoint);
    // What the language is, before anything has changed it. A program that
    // waited for a change would draw its first frame in whatever it guessed.
    announce_language();
}

/// Say what the interface language is now.
fn announce_language() {
    let Some(endpoint) = ROUTE.lock().clone() else {
        return;
    };
    let mut message = [0u8; wire::SIZE];
    message[0] = wire::LANGUAGE;
    message[1..5].copy_from_slice(&(i18n::current_index() as u32).to_le_bytes());
    endpoint.send(&message, alloc::vec::Vec::new()).ok();
}

/// What a key looks like on the wire.
///
/// Five bytes: what kind of key it was, and a number whose meaning depends on
/// the kind -- a Unicode code point for a character, a number for a function
/// key, nothing for the rest. Fixed width because the reader is a program and
/// not a person, and a length that never varies is one fewer thing for it to
/// get wrong.
pub mod wire {
    pub const CHARACTER: u8 = 1;
    pub const BACKSPACE: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const ESCAPE: u8 = 4;
    pub const TAB: u8 = 5;
    pub const FUNCTION: u8 = 6;
    /// A key that moves rather than types, with which way as its number.
    pub const MOVE: u8 = 8;
    /// Not a key: the interface language, as an index into the locale table.
    ///
    /// Sent when routing begins and again whenever it changes, so that a
    /// program showing translated text switches with the kernel's own panel
    /// rather than keeping its own count of how many times F1 has been pressed.
    /// Two counters would agree until the first key one of them missed.
    pub const LANGUAGE: u8 = 7;
    /// Bytes one key takes.
    pub const SIZE: usize = 5;
}

/// Pass a key on to whoever is routing.
///
/// A failure is dropped rather than reported. The receiver's queue being full
/// means it is not keeping up with the keyboard, which is a thing that happens;
/// logging a line per dropped key would turn a slow program into a flooded
/// serial console, and the key is gone either way.
fn route(key: Key) {
    let Some(endpoint) = ROUTE.lock().clone() else {
        return;
    };

    let (kind, value) = match key {
        Key::Character(character) => (wire::CHARACTER, character as u32),
        Key::Backspace => (wire::BACKSPACE, 0),
        Key::Enter => (wire::ENTER, 0),
        Key::Escape => (wire::ESCAPE, 0),
        Key::Tab => (wire::TAB, 0),
        Key::Function(number) => (wire::FUNCTION, u32::from(number)),
        Key::Move(movement) => (wire::MOVE, movement as u32),
        // Nothing downstream can do anything with a scancode the kernel could
        // not name, and passing it on would be passing on a problem.
        Key::Unknown(_) => return,
    };

    let mut message = [0u8; wire::SIZE];
    message[0] = kind;
    message[1..5].copy_from_slice(&value.to_le_bytes());
    endpoint.send(&message, alloc::vec::Vec::new()).ok();
}

/// Act on one key.
fn handle(key: Key) {
    HANDLED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    route(key);

    match key {
        Key::Character(character) => {
            let mut line = LINE.lock();
            if line.chars().count() < MAX_LINE {
                line.push(character);
            }
        }
        Key::Backspace => {
            LINE.lock().pop();
        }
        Key::Enter | Key::Escape => {
            LINE.lock().clear();
        }
        Key::Tab => {
            LINE.lock().push(' ');
        }
        // F1 switches the interface language. This is where a settings screen
        // will take over; until there is one, a key is a great deal more honest
        // than a timer that changes the language on its own.
        Key::Function(1) => {
            let locale = i18n::next_locale();
            kprintln!("[input] F1: interface language is now {}", locale.tag);
            announce_language();
        }
        Key::Function(number) => {
            kprintln!("[input] F{number} is not bound to anything yet");
        }
        // Passed on and not acted on here. What "down" means is a question
        // about whatever has the keyboard -- a page, a list, a field -- and the
        // kernel's own panel is not scrollable.
        Key::Move(_) => {}
        Key::Unknown(code) => {
            kprintln!("[input] unrecognised scancode {code:#04x}");
        }
    }
}

/// The thread that drains the keyboard.
///
/// It blocks. Between keystrokes it is not on a run queue, does not take a time
/// slice, and does not appear in a scheduling decision at all; the interrupt
/// that receives a scancode is what makes it runnable again. It used to poll at
/// fifty hertz, which cost a wake-up every twenty milliseconds to find nothing,
/// on a system where a key arrives a few times a minute.
fn input_thread(_argument: usize) {
    loop {
        // Drain everything queued, not one key per wake: a fast typist or a
        // burst from an autorepeat would otherwise fall progressively further
        // behind, and one wake-up can cover a whole burst.
        while let Some(key) = keyboard::next_key() {
            handle(key);
        }
        keyboard::wait_for_scancode();
    }
}

/// Start the input thread.
pub fn start_thread() {
    match sched::spawn(
        "input",
        // Interactive: someone is waiting on every key this thread handles.
        sched::thread::Priority::Interactive,
        input_thread,
        0,
    ) {
        Ok(id) => kprintln!("[input] input thread {id} started; F1 switches language"),
        Err(error) => kprintln!("[input] could not start the input thread: {error}"),
    }
}
