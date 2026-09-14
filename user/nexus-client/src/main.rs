//! `client`: a program that draws, and never touches the screen.
//!
//! It is handed a surface: a rectangle of memory, its size, and nothing else.
//! It does not know where on the display that surface ends up, whether it is on
//! the display at all, what else is on screen, or who else is running. It draws
//! into the memory it was given and says it has finished.
//!
//! That ignorance is the whole point. A client that cannot reach the
//! framebuffer cannot scribble over another client's window, cannot read what
//! another client is showing, and cannot be broken by the compositor moving
//! things around underneath it.
//!
//! # What it draws
//!
//! A panel with text in it, through [`nexus_ui`] — the same face the kernel
//! draws its own panel with, so a string is the same width whoever drew it, and
//! the same crate a real application would use. Before NexusUI existed this
//! program drew a gradient, because a gradient is what you can draw with no
//! font, no allocator and no layout.
//!
//! It shows both languages, which is not decoration: a toolkit that only ever
//! laid out half-width glyphs would advance the cursor wrongly for the other
//! half and nobody would find out until there was Japanese on screen.
//!
//! # What it is told
//!
//! One message to begin with: the width and height of its surface and a number
//! to shade it by, as little-endian 32-bit values, and a handle to the memory.
//! The surface is tightly packed — width times four bytes per row, no padding.
//!
//! And then, possibly, again. A surface can be *replaced*: the compositor sends
//! a new size and a new handle, and this program unmaps what it had and maps
//! what it was given. It still does not know why, or where the thing is, or
//! whether anyone can see it.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// What a key looks like on the wire: a kind, then a number.
///
/// The same shape the kernel sends and the compositor forwards without opening.
/// A client is the first thing on that path that has any business reading it.
mod key {
    pub const CHARACTER: u8 = 1;
    /// Bytes one key takes.
    pub const SIZE: usize = 5;
}

/// What the compositor says when a frame is on screen.
const SHOWN: &[u8] = b"shown";
/// What it says when the surface has been replaced.
///
/// Distinguished by its first bytes rather than by its length, because a length
/// is a thing two messages can share by accident and a word is not.
const RESIZED: &[u8] = b"size";

/// Where this program maps its surface. Its own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// What this program has to say, in both languages.
///
/// Both, because a toolkit that only ever laid out half-width glyphs would
/// advance the cursor wrongly for the other half and nobody would find out
/// until there was Japanese on screen. Every character in it is in
/// `shared/nexus-font/charset.txt`, which is how a program says what the face
/// has to cover -- the translations cover their own, and this sentence is not
/// in a translation.
const BODY: &str = "drawn by a program into memory it was given. 画面には触れていません。";

/// How many times it redraws before it is done.
const FRAMES: u32 = 24;

/// How long it waits between frames.
///
/// Sleeping rather than drawing as fast as it can. A client that redrew flat
/// out would be a client using a whole processor to animate a rectangle, and it
/// makes the thing visible: composition that finishes in three milliseconds is
/// composition nobody ever sees happen.
const FRAME_MS: u64 = 500;

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

/// How many keys this program has been sent.
static KEYS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Which client this is, as the compositor numbered it.
static WHICH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

extern "C" fn main() -> ! {
    // Before anything allocates, and NexusUI allocates.
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("client: FAILED: could not get a heap");
        finish();
    }

    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 1];

    let received = match nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("client: FAILED: nothing arrived to draw on");
            finish();
        }
    };
    if received.handles != 1 || received.bytes < 16 {
        failed("client: FAILED: no surface came with the message");
        finish();
    }

    let mut width = read_u32(&buffer, 0);
    let mut height = read_u32(&buffer, 4);
    let tint = read_u32(&buffer, 8);
    // Only so that this program can name itself in a log. It cannot address
    // another client and there is nothing for it to index into.
    WHICH.store(read_u32(&buffer, 12), core::sync::atomic::Ordering::Relaxed);
    let mut surface = handles[0];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("client: FAILED: could not map its surface");
        finish();
    };

    // Checked against the mapping rather than against the message. The two come
    // from the same place today, and a client that assumed they agreed would
    // write past the end of its own buffer the day one of them was rounded --
    // which they are: memory is handed out in whole pages, so what is mapped is
    // at least what was asked for and usually more.
    if (width as usize) * (height as usize) * 4 > mapped {
        failed("client: FAILED: the surface is smaller than the size it was given");
        finish();
    }
    let mut resizes = 0u32;

    for frame in 0..FRAMES {
        draw(width, height, tint, frame);

        // One message per frame, and the compositor answers each one. The
        // answer is what keeps this from drawing over a frame that has not
        // been composited yet: with no reply this would race the compositor
        // for the buffer, and tearing is what that looks like.
        if nexus_user::send(COMPOSITOR, b"damaged", &[]).is_err() {
            failed("client: FAILED: could not say it had drawn");
            finish();
        }
        // Read until the acknowledgement. Keys may arrive in between -- they
        // are sent when someone presses one, not when this program asks -- so
        // the loop takes whatever comes and stops at the reply it was waiting
        // for. A client that assumed the next message was its acknowledgement
        // would treat the first keystroke as one and then run a frame ahead.
        loop {
            let mut reply = [0u8; 32];
            let mut incoming = [Handle(0); 1];
            let Ok(received) = nexus_user::receive(COMPOSITOR, &mut reply, &mut incoming) else {
                failed("client: FAILED: the compositor stopped answering");
                finish();
            };
            if &reply[..received.bytes] == SHOWN {
                break;
            }

            // A new surface. The old one is unmapped before the new one is
            // mapped, because they go to the same address -- which is the whole
            // point of being able to unmap: a program that had to put each new
            // surface somewhere else would run out of address space rather than
            // out of memory.
            if received.bytes >= 12 && &reply[..4] == RESIZED && received.handles == 1 {
                nexus_user::memory_unmap(surface, SURFACE_AT).ok();
                nexus_user::close(surface).ok();
                surface = incoming[0];
                width = read_u32(&reply, 4);
                height = read_u32(&reply, 8);
                let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
                    failed("client: FAILED: could not map the surface it was given");
                    finish();
                };
                if (width as usize) * (height as usize) * 4 > mapped {
                    failed("client: FAILED: the new surface is smaller than its size");
                    finish();
                }
                resizes += 1;
                if resizes == 1 {
                    nexus_user::log("client: took a new surface and kept drawing").ok();
                }
                continue;
            }

            if received.bytes >= key::SIZE {
                heard(&reply[..received.bytes]);
            }
        }

        if frame == 0 {
            report(width);
        }
        nexus_user::sleep(FRAME_MS).ok();
    }

    nexus_user::log("client: drew every frame into a surface it was given").ok();

    // And then it stays. A window that closed itself after twelve seconds was
    // fine while this was a demonstration of compositing and wrong the moment
    // it became a desktop: somebody who puts a window away and goes to make tea
    // should find it in the strip when they come back, and somebody driving the
    // pointer at a title bar should find a title bar there.
    //
    // What ends it is the compositor closing the channel, which happens when
    // the session does. So this waits, answers what it is sent, and redraws
    // when there is a reason to -- which is what every other program on this
    // machine does and what this one should have been doing all along.
    live(width, height, tint, &mut surface);
    finish()
}

/// Keep the window, until there is nobody to keep it for.
///
/// Draws on a reason rather than on a timer: a key arrived, or the window was
/// given a new surface. An idle window costs one blocked thread, which is
/// nothing, and a window that repainted while nobody touched it would cost a
/// composite a frame for a picture that had not changed.
fn live(mut width: u32, mut height: u32, tint: u32, surface: &mut Handle) {
    let mut frame = FRAMES;
    // Bounded, because this is a loop around a call that can return without
    // blocking. The bound is a backstop and not a schedule: at one turn per
    // keystroke it is more keys than anybody will press at one window.
    for _ in 0..1_000_000u32 {
        let mut message = [0u8; 32];
        let mut incoming = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut incoming) else {
            // The compositor has gone, which is how a session ends. Not a
            // failure: there is no display to draw on any more.
            return;
        };
        if received.bytes == 0 {
            return;
        }

        if &message[..received.bytes] == SHOWN {
            continue;
        }

        let mut redraw = false;
        if received.bytes >= 12 && &message[..4] == RESIZED && received.handles == 1 {
            nexus_user::memory_unmap(*surface, SURFACE_AT).ok();
            nexus_user::close(*surface).ok();
            *surface = incoming[0];
            width = read_u32(&message, 4);
            height = read_u32(&message, 8);
            let Ok(mapped) = nexus_user::memory_map(*surface, SURFACE_AT, true) else {
                failed("client: FAILED: could not map the surface it was given");
                return;
            };
            if (width as usize) * (height as usize) * 4 > mapped {
                failed("client: FAILED: the new surface is smaller than its size");
                return;
            }
            redraw = true;
        } else if received.bytes >= key::SIZE {
            heard(&message[..received.bytes]);
            redraw = true;
        }

        if redraw {
            frame = frame.wrapping_add(1);
            draw(width, height, tint, frame);
            if nexus_user::send(COMPOSITOR, b"damaged", &[]).is_err() {
                return;
            }
        }
    }
}

/// Note a key that was sent to this program.
///
/// Only a program with focus is sent one, so this running at all is the whole
/// claim: a keystroke went into the kernel's keyboard driver, crossed a channel
/// to the compositor, was routed to one client and not the other, and arrived
/// here.
fn heard(message: &[u8]) {
    KEYS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if message[0] != key::CHARACTER {
        return;
    }
    // Two fixed strings rather than a formatted one, and which of them appears
    // is the whole point -- a key that reached both clients would print both.
    nexus_user::log(if WHICH.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        "client 0: heard a key from the compositor"
    } else {
        "client 1: heard a key from the compositor"
    })
    .ok();
}

/// Draw the whole surface: a panel, a title, and some text under it.
///
/// Drawn from nothing every frame. At a few tens of thousands of pixels that is
/// cheaper than remembering what changed, and remembering is the optimisation
/// to make when there is something to measure.
fn draw(width: u32, height: u32, tint: u32, frame: u32) {
    // SAFETY: the surface is mapped here, writable, and is at least
    // `width * height * 4` bytes -- checked against the mapping before the
    // first frame and again after every replacement.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, width, height) };

    let keys = KEYS.load(core::sync::atomic::Ordering::Relaxed);
    let accent = Colour::rgb(0x38, 0x8B, 0xE8).blend(Colour::rgb(0xE0, 0x60, 0xA0), tint as u8);

    // The background moves with the frame and with the keys, so a stale buffer
    // shows as a frame that did not change and a client being typed at looks
    // different from one that is not.
    let shift = ((frame * 6 + keys * 12) & 0x3F) as u8;
    canvas.gradient(
        canvas.bounds(),
        Colour::rgb(0x0C, 0x16, 0x28).blend(accent, shift / 4),
        Colour::rgb(0x05, 0x0A, 0x14),
    );

    let panel = canvas.bounds().inset(6);
    canvas.outline(panel, 1, accent);

    let mut column = nexus_ui::Column::new(panel.inset(6), 3);

    // The title, centred, in the accent colour.
    let title = column.row(nexus_ui::LINE_HEIGHT + 4);
    canvas.fill(title, accent.blend(Colour::rgb(0, 0, 0), 200));
    canvas.text_centred(title, "NexusUI", Colour::rgb(0xF0, 0xF4, 0xFF));

    // And some lines under it, wrapped to the width they have. Both languages,
    // because a toolkit that only ever laid out half-width glyphs would advance
    // the cursor wrongly for the other half.
    let ink = Colour::rgb(0xC8, 0xD4, 0xE8);
    for line in nexus_ui::wrap(BODY, column.remaining().min(panel.width)) {
        let row = column.line();
        if row.width == 0 {
            break;
        }
        canvas.text(row.x, row.y, line, ink);
    }

    // A bar that grows with the keys this client has been sent, so routing is
    // something to see and not only something in a log.
    if column.remaining() >= nexus_ui::LINE_HEIGHT {
        let row = column.line();
        if row.width > 0 {
            let filled = (keys * 16).min(row.width);
            canvas.fill(Rect::new(row.x, row.y + 4, filled, 4), accent);
        }
    }
}

/// Say what the layout came out as, once, in a line built at run time.
///
/// Three things at once, and all three are new. The string is *formatted*,
/// which needs a heap, which needs the kernel to have given this program memory
/// it asked for itself. The line count comes from wrapping real text to a real
/// width. And the width in pixels comes from the same face the kernel measures
/// its own panel with, so a claim about it is a claim about both.
fn report(width: u32) {
    let usable = width.saturating_sub(24);
    let lines = nexus_ui::wrap(BODY, usable);
    let widest = lines.iter().map(|line| nexus_ui::measure(line)).max();
    nexus_user::log(&alloc::format!(
        "client: laid out {} lines, widest {} px, in {} px of surface",
        lines.len(),
        widest.unwrap_or(0),
        usable
    ))
    .ok();
}

/// Read a little-endian `u32` out of the message.
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
    nexus_user::log("client: PANIC").ok();
    nexus_user::exit_with(2)
}
