//! `wall`: what is behind the windows.
//!
//! An ordinary client. It is handed a surface and draws into it exactly as a
//! window does; what differs is only where the compositor puts it, which is
//! underneath everything else.
//!
//! # Why the wallpaper is a program
//!
//! Because the alternative is the compositor deciding what a desktop looks
//! like, and the compositor is the one program on this machine whose decisions
//! nothing else can replace. A background is *taste* — a colour, a pattern,
//! one day a picture or a video — and taste in the program that owns the
//! framebuffer is the same mistake as policy in the kernel, one layer up.
//!
//! So this draws, and the compositor composites a surface it knows nothing
//! about. What that buys is concrete: replacing the wallpaper is replacing one
//! program, and a wallpaper that crashes is a black rectangle rather than a
//! machine with no display.
//!
//! # What it cannot do yet
//!
//! Pictures and video. A picture needs a decoder for whatever format it is in;
//! a video needs one that runs thirty times a second. Neither is here, and the
//! patterns below are drawn rather than loaded — which is why they cost a few
//! hundred lines instead of a few hundred thousand.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::panic::PanicInfo;

use nexus_look::{Colour, Look, Style};
use nexus_ui::Canvas;
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_4800_0000;

/// How much heap: the settings file, and nothing else.
const HEAP: usize = 256 * 1024;

/// What the settings file is called.
const SETTINGS_NAME: &str = "settings.txt";

/// The longest settings file this will read.
const SETTINGS_MAX: usize = 16 * 1024;

/// How often a moving pattern draws a new frame.
///
/// Four a second, and slow on purpose. A background fills the whole area
/// windows may occupy, so every frame of it is a full-screen composite -- two
/// million pixels copied by the compositor, which is the cost that matters and
/// not the drawing. Four is enough for drift to read as drift and few enough
/// that a moving background is a small fraction of a processor rather than a
/// steady load.
///
/// The way past this is damage rectangles on a client's frame, so a program can
/// say *which* part of its surface changed. Nothing has them yet, and inventing
/// them for a decoration would be inventing them in the wrong place: the first
/// program that needs them is a text editor redrawing one line.
const FRAME_MS: u64 = 250;

/// How often the settings are looked at again, in frames of the above.
///
/// Two seconds. A wallpaper that re-read the file every frame would be a
/// wallpaper opening a file four times a second for an answer that changes when
/// somebody types a command.
const RECHECK: u32 = 8;

/// And how often a still pattern looks, since it has no frames of its own.
const STILL_RECHECK_MS: u64 = 2_000;

/// What the compositor says.
mod wire {
    pub const SHOWN: &[u8] = b"shown";
    pub const RESIZED: &[u8] = b"size";
    pub const DAMAGED: &[u8] = b"damaged";
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
    if !nexus_user::heap::init(HEAP) {
        failed("wall: FAILED: could not get a heap");
        finish();
    }

    // Two handles: the surface, and the settings directory to read what to
    // draw. Without the second it draws the default, which is what this system
    // looked like before it could be changed.
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 2];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("wall: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles < 1 || received.bytes < 16 {
        failed("wall: FAILED: no surface came with the message");
        finish();
    }

    let mut width = read_u32(&buffer, 0);
    let mut height = read_u32(&buffer, 4);
    let mut surface = handles[0];
    let settings = (received.handles >= 2).then(|| handles[1]);

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("wall: FAILED: could not map its surface");
        finish();
    };
    if width as usize * height as usize * 4 > mapped {
        failed("wall: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    let mut look = read_look(settings);
    nexus_user::log(&alloc::format!(
        "wall: {width}x{height} behind the windows, {} on {}",
        look.style.name(),
        look.top.to_text()
    ))
    .ok();

    let Ok(set) = nexus_user::wait_set() else {
        failed("wall: FAILED: could not make a wait set");
        finish();
    };
    const SAID: u64 = 1;
    if nexus_user::watch(set, COMPOSITOR, SAID).is_err() {
        failed("wall: FAILED: could not watch the compositor");
        finish();
    }

    let mut frame = 0u32;
    let mut since_check = 0u32;
    let mut stale = true;
    let mut in_flight = false;

    // Bounded, so a wallpaper whose compositor stops answering cannot spin.
    // Large, because at four frames a second this is most of a day and the
    // thing it draws is meant to be there all of it.
    for _ in 0..16_000_000u64 {
        if stale && !in_flight {
            draw(&look, width, height, frame);
            if nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).is_err() {
                break;
            }
            stale = false;
            in_flight = true;
        }

        // A still pattern waits to be woken and costs nothing; a moving one
        // wakes on its own. What decides is the pattern, which is the only
        // thing that knows whether anything would change.
        let wait = if look.style.moves() {
            FRAME_MS
        } else {
            STILL_RECHECK_MS
        };
        let mut keys = [0u64; 2];
        let Ok(ready) = nexus_user::wait_any_until(set, &mut keys, wait) else {
            break;
        };

        if ready == 0 {
            frame = frame.wrapping_add(1);
            since_check += 1;
            if look.style.moves() {
                stale = true;
            }
            // Looked at again every so often, so that changing a setting shows
            // up without anything having to tell this program.
            if since_check >= RECHECK || !look.style.moves() {
                since_check = 0;
                let now = read_look(settings);
                if now != look {
                    nexus_user::log(&alloc::format!(
                        "wall: the look changed to {} on {}",
                        now.style.name(),
                        now.top.to_text()
                    ))
                    .ok();
                    look = now;
                    stale = true;
                }
            }
            continue;
        }

        let mut message = [0u8; 64];
        let mut incoming = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut incoming) else {
            break;
        };
        let bytes = &message[..received.bytes];

        if bytes == wire::SHOWN {
            in_flight = false;
            continue;
        }

        if received.bytes >= 12 && bytes.starts_with(wire::RESIZED) && received.handles == 1 {
            nexus_user::memory_unmap(surface, SURFACE_AT).ok();
            nexus_user::close(surface).ok();
            surface = incoming[0];
            width = read_u32(bytes, 4);
            height = read_u32(bytes, 8);
            let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
                failed("wall: FAILED: could not map the surface it was given");
                break;
            };
            if width as usize * height as usize * 4 > mapped {
                failed("wall: FAILED: the new surface is smaller than its size");
                break;
            }
            stale = true;
        }
    }

    finish()
}

/// What the settings say this machine should look like.
fn read_look(settings: Option<Handle>) -> Look {
    let Some(directory) = settings else {
        return Look::default();
    };
    let Ok(file) = nexus_user::open(directory, SETTINGS_NAME) else {
        return Look::default();
    };
    let size = nexus_user::size(file).unwrap_or(0).min(SETTINGS_MAX);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    match String::from_utf8(bytes) {
        Ok(text) => Look::parse(&text),
        Err(_) => Look::default(),
    }
}

/// Draw the whole background.
fn draw(look: &Look, width: u32, height: u32, frame: u32) {
    // SAFETY: the surface is mapped here, writable, and at least
    // `width * height * 4` bytes -- checked when it was taken and again after
    // every replacement.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, width, height) };

    let top = nexus_ui::Colour(look.top.packed());
    let bottom = nexus_ui::Colour(look.bottom.packed());

    match look.style {
        Style::Plain => canvas.fill(canvas.bounds(), top),
        Style::Gradient => canvas.gradient(canvas.bounds(), top, bottom),
        Style::Stars => {
            canvas.gradient(canvas.bounds(), top, bottom);
            stars(&mut canvas, look, width, height, frame);
        }
        Style::Rings => {
            canvas.gradient(canvas.bounds(), top, bottom);
            rings(&mut canvas, look, width, height, frame);
        }
        Style::Grid => {
            canvas.gradient(canvas.bounds(), top, bottom);
            grid(&mut canvas, look, width, height);
        }
    }
}

/// A field of drifting points.
///
/// Positions come from a hash of the point's number rather than from a stored
/// list: the pattern is then decided by one number, the same every boot, and
/// there is nothing to keep between frames. Two hundred points at four pixels
/// is a thousand writes a frame, which at eight frames a second is nothing.
fn stars(canvas: &mut Canvas, look: &Look, width: u32, height: u32, frame: u32) {
    const COUNT: u32 = 220;
    for star in 0..COUNT {
        let hash = star.wrapping_mul(2_654_435_761).wrapping_add(0x9E37_79B9);
        // Three fields out of one hash: where it is, how fast it drifts, and
        // how bright it is. Taken from different bits so they are independent.
        let speed = 1 + (hash >> 28) % 4;
        let x = (hash % width.max(1) + frame.wrapping_mul(speed) / 4) % width.max(1);
        let y = (hash >> 8) % height.max(1);
        let brightness = 90 + ((hash >> 20) % 160) as u8;
        let colour = nexus_ui::Colour(
            look.bottom
                .towards(Colour::new(255, 255, 255), brightness)
                .packed(),
        );
        // Twinkling, from the frame and the star's own number, so they are not
        // all bright at once.
        let phase = (frame / 2).wrapping_add(star) % 24;
        let size = if phase < 2 { 2 } else { 1 };
        canvas.fill(nexus_ui::Rect::new(x, y, size, size), colour);
    }
}

/// Rings spreading from the middle.
///
/// Drawn with the midpoint circle algorithm, which is integers all the way
/// down: a decision variable, two increments, and eight points per step from
/// one octant's worth of work. There is no floating point here and none is
/// wanted -- a circle made of twenty-four sampled directions is twenty-four
/// dots, and one made of trigonometry is a table nobody can check.
fn rings(canvas: &mut Canvas, look: &Look, width: u32, height: u32, frame: u32) {
    let centre_x = width as i32 / 2;
    let centre_y = height as i32 / 2;
    let spacing = 90u32;
    // The rings move outwards by moving where the first one starts.
    let drift = (frame * 3) % spacing;
    let furthest = width.max(height);

    let mut radius = drift.max(4);
    while radius < furthest {
        // Fading with distance, so a ring arrives and leaves rather than
        // stopping at the edge of the screen.
        let fade = 44u32.saturating_sub(radius * 44 / furthest.max(1)) as u8;
        let colour = nexus_ui::Colour(look.bottom.towards(look.accent, fade.max(5)).packed());
        circle(
            canvas,
            centre_x,
            centre_y,
            radius as i32,
            colour,
            width,
            height,
        );
        radius += spacing;
    }
}

/// One circle, by the midpoint algorithm.
fn circle(
    canvas: &mut Canvas,
    centre_x: i32,
    centre_y: i32,
    radius: i32,
    colour: nexus_ui::Colour,
    width: u32,
    height: u32,
) {
    let mut x = radius;
    let mut y = 0;
    // The decision variable: positive means the next step goes inwards.
    let mut error = 1 - radius;

    while x >= y {
        // Eight points, one per octant, from the one pair this has computed.
        for (dx, dy) in [
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            let px = centre_x + dx;
            let py = centre_y + dy;
            if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                canvas.set(px as u32, py as u32, colour);
            }
        }
        y += 1;
        if error < 0 {
            error += 2 * y + 1;
        } else {
            x -= 1;
            error += 2 * (y - x) + 1;
        }
    }
}

/// A grid, fading downwards.
fn grid(canvas: &mut Canvas, look: &Look, width: u32, height: u32) {
    const SPACING: u32 = 48;
    let line = look.bottom.towards(look.accent, 28);

    let mut x = 0;
    while x < width {
        for y in 0..height {
            // Fainter towards the bottom, which is where the strip is and where
            // a pattern competing with it would be most in the way.
            let fade = 255u32.saturating_sub(y * 200 / height.max(1)) as u8;
            let colour = nexus_ui::Colour(
                nexus_look::Colour::from_packed(canvas_pixel(look, y, height))
                    .towards(line, fade / 3)
                    .packed(),
            );
            canvas.set(x, y, colour);
        }
        x += SPACING;
    }

    let mut y = 0;
    while y < height {
        let fade = 255u32.saturating_sub(y * 200 / height.max(1)) as u8;
        let colour = nexus_ui::Colour(
            nexus_look::Colour::from_packed(canvas_pixel(look, y, height))
                .towards(line, fade / 3)
                .packed(),
        );
        for x in 0..width {
            canvas.set(x, y, colour);
        }
        y += SPACING;
    }
}

/// What the gradient is at this row.
///
/// Worked out rather than read back, because reading the surface would be a
/// read from mapped memory per pixel for a number this can compute.
fn canvas_pixel(look: &Look, y: u32, height: u32) -> u32 {
    let amount = (y * 255 / height.max(1)) as u8;
    look.top.towards(look.bottom, amount).packed()
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
    nexus_user::log("wall: PANIC").ok();
    nexus_user::exit_with(2)
}
