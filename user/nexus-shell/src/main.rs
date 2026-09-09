//! `shell`: the desktop.
//!
//! The strip along the bottom of the screen, and the one place on the machine
//! where a person can start a program or bring one back. Until this existed the
//! compositor drew that strip itself and decided what a click in it meant, and
//! the only programs that ever ran were the two it started at boot.
//!
//! # Why it is a program and not part of the compositor
//!
//! The compositor owns the display. Everything it does is therefore something
//! nothing else can check: a bug in it is pixels anywhere on screen, and a
//! decision in it is a decision no other program can replace. A dock is
//! *policy* -- what a strip looks like, what a click in it means, which
//! programs are worth launching -- and policy in the one program that owns the
//! framebuffer is the same mistake as policy in the kernel, one layer up.
//!
//! So the desktop is a client. It is handed a surface exactly as any other
//! client is, it draws into memory, and it has no idea where that memory ends
//! up. What it is given that other clients are not is a *list*: which windows
//! exist and what state each is in, and a channel to say what should happen to
//! them. It cannot draw a window, move one, read one, or reach the display.
//!
//! # What it draws
//!
//! A button that starts a program, a tab per window, and a line saying what is
//! running -- in the interface language, which arrives from the kernel through
//! the compositor. Pressing F1 changes the kernel's panel and this strip at the
//! same moment, because both read the same table and neither counts keys.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString as _};
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the strip is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// What the compositor says, and what this program says back.
///
/// Distinguished by a word rather than by a length: a length is a thing two
/// messages can share by accident.
mod wire {
    /// A frame is on screen.
    pub const SHOWN: &[u8] = b"shown";
    /// Here is every window and what state it is in.
    pub const WINDOWS: &[u8] = b"win";
    /// Somebody clicked in the strip, at these coordinates within it.
    pub const CLICK: &[u8] = b"clk";
    /// The interface language is now this.
    pub const LANGUAGE: &[u8] = b"lng";

    /// This program has drawn.
    pub const DAMAGED: &[u8] = b"damaged";
    /// Bring this window back and give it focus.
    pub const SHOW: &[u8] = b"show";
    /// Put this window away.
    pub const HIDE: &[u8] = b"hide";
    /// Start another program.
    pub const OPEN: &[u8] = b"open";
}

/// What state a window can be in, as the compositor reports it.
mod state {
    /// No such window, or it has ended.
    pub const GONE: u8 = 0;
    /// On screen.
    pub const SHOWN: u8 = 1;
    /// Put away, reachable only by its tab.
    pub const AWAY: u8 = 2;
    /// On screen and holding focus.
    pub const FOCUSED: u8 = 3;
}

/// The most windows this program will ever be told about.
///
/// A fixed ceiling because the strip has a fixed width: a dock that accepted
/// any number of windows would draw tabs one pixel wide and then none at all.
/// The compositor has its own limit and it is the smaller of the two that
/// decides; neither trusts the other's.
const MAX_WINDOWS: usize = 8;

/// How wide the button that starts a program is.
const LAUNCH_WIDTH: u32 = 72;

/// Pixels between things in the strip.
const GAP: u32 = 4;

/// Everything this program knows about the desktop.
struct Desktop {
    /// How many window slots the compositor has.
    slots: usize,
    /// What state each is in.
    windows: [u8; MAX_WINDOWS],
    /// Where the strip is, and how large.
    width: u32,
    height: u32,
}

impl Desktop {
    /// Where the button that starts a program is.
    fn launcher(&self) -> Rect {
        Rect::new(GAP, GAP / 2, LAUNCH_WIDTH, self.height.saturating_sub(GAP))
    }

    /// Where a window's tab is.
    ///
    /// By slot, so a tab does not move when another window is put away or
    /// brought back. A tab that shuffled sideways under the pointer would be a
    /// tab somebody clicked and missed.
    fn tab(&self, slot: usize) -> Rect {
        let left = GAP * 2 + LAUNCH_WIDTH;
        let available = self.width.saturating_sub(left + GAP);
        let each = (available / self.slots.max(1) as u32).saturating_sub(GAP);
        Rect::new(
            left + slot as u32 * (each + GAP),
            GAP / 2,
            each,
            self.height.saturating_sub(GAP),
        )
    }

    /// How many windows are put away.
    fn away(&self) -> usize {
        self.windows[..self.slots.min(MAX_WINDOWS)]
            .iter()
            .filter(|state| **state == state::AWAY)
            .count()
    }

    /// Whether anything is running at all.
    fn empty(&self) -> bool {
        self.windows[..self.slots.min(MAX_WINDOWS)]
            .iter()
            .all(|state| *state == state::GONE)
    }
}

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

extern "C" fn main() -> ! {
    // Before anything allocates, and both the toolkit and the translations do.
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("shell: FAILED: could not get a heap");
        finish();
    }

    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("shell: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles != 1 || received.bytes < 16 {
        failed("shell: FAILED: no surface came with the message");
        finish();
    }

    let mut desktop = Desktop {
        slots: (read_u32(&buffer, 8) as usize).min(MAX_WINDOWS),
        windows: [state::GONE; MAX_WINDOWS],
        width: read_u32(&buffer, 0),
        height: read_u32(&buffer, 4),
    };
    let surface = handles[0];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("shell: FAILED: could not map the strip");
        finish();
    };
    // Checked against the mapping rather than against the message, because
    // memory is handed out in whole pages and the two need not agree.
    if desktop.width as usize * desktop.height as usize * 4 > mapped {
        failed("shell: FAILED: the strip is smaller than the size it was given");
        finish();
    }
    if desktop.slots == 0 {
        failed("shell: FAILED: a desktop with no window slots");
        finish();
    }

    nexus_user::log("shell: took the strip along the bottom of the screen").ok();
    run(&mut desktop);
    finish()
}

/// Draw, say so, and act on whatever comes back.
///
/// Event-driven rather than timed: this program redraws when something it shows
/// has changed and at no other moment. A dock on a frame timer would be a dock
/// costing a repaint a frame to show a strip nobody has touched in an hour.
fn run(desktop: &mut Desktop) {
    let mut drawn = 0u32;
    let mut launched = 0u32;
    let mut acted = 0u32;
    let mut announced = false;

    // Bounded, so a desktop whose compositor stops answering cannot spin. The
    // bound is a backstop and not a schedule.
    for _ in 0..512 {
        draw(desktop);
        if nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).is_err() {
            break;
        }
        drawn += 1;

        // Read until the frame is acknowledged *and* something has changed what
        // is on the strip. Both, in either order: the compositor sends the
        // first list of windows as soon as it has one, which may well be before
        // it has acknowledged this program's first frame. A loop that only
        // broke on a change arriving *after* the acknowledgement would sit
        // there holding a strip drawn before it knew there were any windows --
        // which is exactly what it did, and what made the tabs appear or not
        // depending on which message won a race.
        //
        // A click is answered inside this loop rather than redrawn for, because
        // what a click does comes back as a new list of windows.
        let mut shown = false;
        let mut changed = false;
        loop {
            let mut message = [0u8; 64];
            let mut none = [Handle(0); 1];
            let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut none) else {
                report(drawn);
                return;
            };
            let message = &message[..received.bytes];

            if message == wire::SHOWN {
                shown = true;
                if changed {
                    break;
                }
                continue;
            }

            if message.starts_with(wire::WINDOWS) && message.len() >= 4 {
                let count = (message.len() - 3).min(MAX_WINDOWS);
                desktop.windows = [state::GONE; MAX_WINDOWS];
                desktop.windows[..count].copy_from_slice(&message[3..3 + count]);
                changed = true;
                if shown {
                    break;
                }
                continue;
            }

            if message.starts_with(wire::LANGUAGE) && message.len() >= 7 {
                let index = read_u32(message, 3) as usize;
                // Set by tag rather than by index, because an index is a
                // position in a table this program did not build. The tag is
                // what both sides actually agree on.
                if let Some(locale) = nexus_i18n::LOCALES.get(index) {
                    nexus_i18n::set_locale(locale.tag);
                    if !announced {
                        announced = true;
                        nexus_user::log("shell: drew its strip in the language it was told").ok();
                    }
                }
                changed = true;
                if shown {
                    break;
                }
                continue;
            }

            if message.starts_with(wire::CLICK) && message.len() >= 11 {
                let x = read_u32(message, 3);
                let y = read_u32(message, 7);
                if let Some(sent) = clicked(desktop, x, y) {
                    // Logged the first time rather than counted up and reported
                    // at the end: this program ends when the compositor does,
                    // and a claim that only appears at shutdown is a claim
                    // nothing can check while the machine is running.
                    if acted == 0 {
                        nexus_user::log("shell: turned a click in the strip into a command").ok();
                    }
                    acted += 1;
                    if sent {
                        if launched == 0 {
                            nexus_user::log("shell: asked for a program to be started").ok();
                        }
                        launched += 1;
                    }
                }
                // No repaint yet: the compositor answers with a new list, and
                // drawing a state this program merely expected would be a strip
                // that lies for as long as the compositor takes to disagree.
                continue;
            }
        }
    }

    report(drawn);
}

/// Decide what a click in the strip meant, and say so.
///
/// Returns `None` if it landed on nothing, and otherwise whether it started a
/// program. Nothing is changed here: this program says what should happen and
/// the compositor decides whether it does.
fn clicked(desktop: &Desktop, x: u32, y: u32) -> Option<bool> {
    if desktop.launcher().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::OPEN, &[])
            .ok()
            .map(|_| true);
    }

    for slot in 0..desktop.slots {
        if !desktop.tab(slot).contains(x, y) {
            continue;
        }
        let command = match desktop.windows[slot] {
            state::AWAY => wire::SHOW,
            state::SHOWN | state::FOCUSED => wire::HIDE,
            // A tab for a window that has ended is a tab that does nothing.
            // Left drawn, because a gap where a tab was is a strip that
            // reshuffles under the pointer.
            _ => return None,
        };
        let mut message = [0u8; 8];
        message[..command.len()].copy_from_slice(command);
        message[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
        return nexus_user::send(COMPOSITOR, &message, &[])
            .ok()
            .map(|_| false);
    }

    None
}

/// Draw the whole strip.
fn draw(desktop: &Desktop) {
    // SAFETY: the strip is mapped here, writable, and at least
    // `width * height * 4` bytes -- checked against the mapping before the
    // first frame.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, desktop.width, desktop.height) };

    let ground = Colour::rgb(0x06, 0x0D, 0x1A);
    let accent = Colour::rgb(0x38, 0x8B, 0xE8);
    let ink = Colour::rgb(0xC8, 0xD4, 0xE8);

    canvas.fill(canvas.bounds(), ground);

    // The launcher, which is the only thing on this machine that starts a
    // program by being pressed.
    let launcher = desktop.launcher();
    canvas.fill(launcher, accent.blend(Colour::rgb(0, 0, 0), 120));
    canvas.outline(launcher, 1, accent);
    canvas.text_centred(
        launcher,
        nexus_i18n::text("shell.launch"),
        Colour::rgb(0xF0, 0xF4, 0xFF),
    );

    for slot in 0..desktop.slots {
        let tab = desktop.tab(slot);
        if tab.width == 0 {
            break;
        }
        let state = desktop.windows[slot];
        if state == state::GONE {
            // Drawn as an outline: the slot is there and empty, which is not
            // the same thing as the strip being shorter.
            canvas.outline(tab, 1, ground.blend(ink, 40));
            continue;
        }

        let colour = match state {
            state::AWAY => accent.blend(Colour::rgb(0, 0, 0), 90),
            state::FOCUSED => Colour::rgb(0x19, 0x33, 0x50),
            _ => Colour::rgb(0x11, 0x1E, 0x30),
        };
        canvas.fill(tab, colour);
        if state == state::FOCUSED {
            canvas.outline(tab, 1, accent);
        }
        let label = nexus_i18n::format("shell.window", &[("number", &(slot + 1))]);
        canvas.text_centred(tab, &label, ink);
    }

    // What is running, at the right, if there is room for it. Cut rather than
    // overlapped: text drawn over a tab is a strip that cannot be read.
    let away = desktop.away();
    let status: String = if desktop.empty() {
        nexus_i18n::text("shell.none").to_string()
    } else if away > 0 {
        nexus_i18n::format("shell.hidden", &[("number", &away)])
    } else {
        return;
    };
    let width = nexus_ui::measure(&status);
    let last = desktop.tab(desktop.slots - 1);
    let right = desktop.width.saturating_sub(GAP + width);
    if right > last.x + last.width {
        canvas.text(right, GAP / 2 + 2, &status, ink.blend(ground, 90));
    }
}

/// Say what this program did, at the end.
///
/// Only the drawing. What it was asked to do is logged as it happens, because
/// this program ends when the compositor does and a claim that appears only at
/// shutdown is a claim nothing can check while the machine is running.
fn report(drawn: u32) {
    if drawn > 0 {
        nexus_user::log("shell: drew the desktop strip for as long as it was asked to").ok();
    }
}

/// Read a little-endian `u32` out of a message.
fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ])
}

/// Whether anything has gone wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit()
    }
}

/// Log a failure and remember it.
fn failed(what: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(what).ok();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("shell: PANIC").ok();
    nexus_user::exit_with(2)
}
