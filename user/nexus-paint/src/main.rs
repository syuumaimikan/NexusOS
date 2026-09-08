//! `paint`: a program that draws on the screen.
//!
//! Everything on the display so far was drawn by the kernel. This is drawn by a
//! process, into memory the kernel handed it as a handle, at an address the
//! program chose — the same shared-memory mechanism two programs use to talk to
//! each other, pointed at the framebuffer instead of at pages the allocator
//! made.
//!
//! That is the shape a compositor has: a process holding a handle to the
//! display, not a thing inside the kernel. This one is not a compositor. It has
//! no windows, no clients and no idea what else is on screen; it fills a
//! rectangle the kernel promised to leave alone, and it exists to show that the
//! path from a process to a pixel is real.
//!
//! # What it is told
//!
//! One message: the rectangle it may use and the shape of the framebuffer, as
//! eight little-endian 32-bit numbers, and a handle to the memory. Nothing is
//! discovered and nothing is assumed — a program that guessed the stride would
//! draw a diagonal smear on the first machine whose scanlines are padded.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

/// Where this program maps the framebuffer.
///
/// Its own choice, as every mapping is. Well above its image and stack, and
/// with room for a display far larger than anything this will meet.
const FRAMEBUFFER_AT: usize = 0x0000_0000_1000_0000;

/// The eight numbers the kernel sends.
struct Surface {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
    screen_width: u32,
    screen_height: u32,
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
    let mut buffer = [0u8; 64];
    let mut handles = [Handle(0); 1];

    let received = match nexus_user::receive(PARENT, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            nexus_user::log("paint: FAILED: nothing arrived to draw on").ok();
            nexus_user::exit();
        }
    };
    if received.handles != 1 || received.bytes < 32 {
        nexus_user::log("paint: FAILED: no framebuffer came with the message").ok();
        nexus_user::exit();
    }

    let surface = Surface {
        x: read_u32(&buffer, 0),
        y: read_u32(&buffer, 4),
        width: read_u32(&buffer, 8),
        height: read_u32(&buffer, 12),
        stride: read_u32(&buffer, 16),
        bytes_per_pixel: read_u32(&buffer, 20),
        screen_width: read_u32(&buffer, 24),
        screen_height: read_u32(&buffer, 28),
    };

    if surface.bytes_per_pixel != 4 {
        nexus_user::log("paint: FAILED: this program only understands 32-bit pixels").ok();
        nexus_user::exit();
    }

    let memory = handles[0];
    let Ok(size) = nexus_user::memory_size(memory) else {
        nexus_user::log("paint: FAILED: could not ask how large the framebuffer is").ok();
        nexus_user::exit();
    };
    if nexus_user::memory_map(memory, FRAMEBUFFER_AT, true) != Ok(size) {
        nexus_user::log("paint: FAILED: could not map the framebuffer").ok();
        nexus_user::exit();
    }

    // The rectangle is checked against the screen rather than trusted, even
    // though the kernel sent it. A program that draws outside what it was given
    // is a program that would scribble over someone else's window the day there
    // are windows.
    let right = surface.x + surface.width;
    let bottom = surface.y + surface.height;
    if right > surface.screen_width || bottom > surface.screen_height {
        nexus_user::log("paint: FAILED: the rectangle is not on the screen").ok();
        nexus_user::exit();
    }

    fill(&surface);

    nexus_user::log("paint: filled its rectangle from user space").ok();
    nexus_user::send(PARENT, b"painted", &[]).ok();
    nexus_user::exit()
}

/// Draw the rectangle: a border, and a gradient inside it.
///
/// A gradient rather than a flat colour, because a flat rectangle is what a
/// single stuck write looks like too. A gradient that runs the right way in
/// both directions says every pixel was addressed correctly.
fn fill(surface: &Surface) {
    for row in 0..surface.height {
        for column in 0..surface.width {
            let border =
                row < 2 || column < 2 || row >= surface.height - 2 || column >= surface.width - 2;

            let colour = if border {
                0x0021_8AFF
            } else {
                let red = (column * 255 / surface.width.max(1)) & 0xFF;
                let green = (row * 255 / surface.height.max(1)) & 0xFF;
                (red << 16) | (green << 8) | 0x60
            };

            let x = surface.x + column;
            let y = surface.y + row;
            let offset = (y as usize * surface.stride as usize + x as usize) * 4;

            // SAFETY: the framebuffer is mapped here, writable, and the offset
            // was computed from the geometry the kernel sent and checked
            // against the screen above.
            unsafe {
                core::ptr::write_volatile((FRAMEBUFFER_AT + offset) as *mut u32, colour);
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("paint: PANIC").ok();
    nexus_user::exit()
}
