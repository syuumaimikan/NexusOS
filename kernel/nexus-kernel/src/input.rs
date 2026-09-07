//! Input handling.
//!
//! Turns decoded keys into something the system does. This is the whole of the
//! input stack for now: no focus, no windows, no applications to deliver to, so
//! the kernel acts on keys itself.
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

/// Act on one key.
fn handle(key: Key) {
    HANDLED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

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
        }
        Key::Function(number) => {
            kprintln!("[input] F{number} is not bound to anything yet");
        }
        Key::Unknown(code) => {
            kprintln!("[input] unrecognised scancode {code:#04x}");
        }
    }
}

/// The thread that drains the keyboard.
///
/// It polls. Blocking on a wait queue would be better — a thread that sleeps
/// until a key arrives costs nothing, where this wakes fifty times a second to
/// find nothing — but wait queues need the IPC layer, which does not exist yet.
/// Twenty milliseconds is below the threshold where typing feels delayed.
fn input_thread(_argument: usize) {
    loop {
        // Drain everything queued, not one key per wake: a fast typist or a
        // burst from an autorepeat would otherwise fall progressively further
        // behind.
        while let Some(key) = keyboard::next_key() {
            handle(key);
        }
        sched::sleep_ms(20);
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
