//! `client`: a program that draws, and never touches the screen.
//!
//! It is handed a surface: a rectangle of memory, its size, and nothing else.
//! It does not know where on the display that surface ends up, whether it is on
//! the display at all, what else is on screen, or who else is running. It draws
//! into the memory it was given and says it has finished.
//!
//! That ignorance is the whole point. Everything drawn before this was drawn by
//! something that could reach the framebuffer — the kernel, or `paint`, which
//! was handed the framebuffer itself. A client that cannot reach the
//! framebuffer cannot scribble over another client's window, cannot read what
//! another client is showing, and cannot be broken by the compositor moving
//! things around underneath it.
//!
//! # What it is told
//!
//! One message: the width and height of its surface and a number to shade it
//! by, as three little-endian 32-bit values, and a handle to the memory. The
//! surface is tightly packed — width times four bytes per row, no padding —
//! because it is memory made for this and not a window onto hardware someone
//! else chose the shape of.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where this program maps its surface. Its own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// How many times it redraws before it is done.
///
/// More than one, because a compositor that composited once could be a
/// compositor that runs its loop once. Each frame looks different, so a
/// composite that used a stale buffer shows the wrong one.
const FRAMES: u32 = 24;

/// How long it waits between frames.
///
/// Sleeping rather than drawing as fast as it can, for two reasons. A client
/// that redrew flat out would be a client using a whole processor to animate a
/// rectangle, which is what a frame rate exists to avoid. And it makes the
/// thing visible: composition that finishes in three milliseconds is
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

extern "C" fn main() -> ! {
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 1];

    let received = match nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("client: FAILED: nothing arrived to draw on");
            finish();
        }
    };
    if received.handles != 1 || received.bytes < 12 {
        failed("client: FAILED: no surface came with the message");
        finish();
    }

    let width = read_u32(&buffer, 0);
    let height = read_u32(&buffer, 4);
    let tint = read_u32(&buffer, 8);
    let surface = handles[0];

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
        let mut reply = [0u8; 16];
        let mut none = [Handle(0); 1];
        if nexus_user::receive(COMPOSITOR, &mut reply, &mut none).is_err() {
            failed("client: FAILED: the compositor stopped answering");
            finish();
        }

        nexus_user::sleep(FRAME_MS).ok();
    }

    nexus_user::log("client: drew every frame into a surface it was given").ok();
    finish()
}

/// Fill the surface: a border, and a gradient shaded by `tint` and `frame`.
///
/// A gradient rather than a flat colour, because a flat rectangle is what a
/// single stuck write looks like too. One that runs the right way in both
/// directions says every pixel was addressed correctly, and one that changes
/// with the frame says the compositor is showing this frame and not the last.
fn draw(width: u32, height: u32, tint: u32, frame: u32) {
    let shift = frame * 60;

    for row in 0..height {
        for column in 0..width {
            let border = row < 2 || column < 2 || row + 2 >= height || column + 2 >= width;

            let colour = if border {
                0x00E8_ECF5
            } else {
                let red = ((column * 255 / width.max(1)) + shift) & 0xFF;
                let green = ((row * 255 / height.max(1)) + shift) & 0xFF;
                (red << 16) | (green << 8) | (tint & 0xFF)
            };

            let offset = (row as usize * width as usize + column as usize) * 4;

            // SAFETY: the surface is mapped here, writable, and the offset was
            // computed from the size checked against the mapping above.
            unsafe {
                core::ptr::write_volatile((SURFACE_AT + offset) as *mut u32, colour);
            }
        }
    }
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
