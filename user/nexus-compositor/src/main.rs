//! `compositor`: the process that owns the display.
//!
//! Everything drawn before this was drawn by whoever could reach the
//! framebuffer. The kernel drew the banner and the status panel because it had
//! the framebuffer; `paint` drew a rectangle because it was handed the
//! framebuffer. Both are the same arrangement — draw by having the display —
//! and it does not survive a second program wanting to draw.
//!
//! This is the other arrangement. One process holds the display. Everyone else
//! holds a *surface*: memory of their own, of a size they were told, that they
//! draw into and never see the destination of. A client cannot scribble over
//! another client's window because it cannot reach one; cannot read what
//! another client is showing for the same reason; and cannot be broken by this
//! program moving things around, because it was never told where it was.
//!
//! # What it does
//!
//! Asks for two clients, gives each a surface, and lays them out side by side
//! in the rectangle the kernel reserved for it. Then it waits — on every client
//! channel and every client process at once, in one wait set — and does one of
//! two things with whatever it hears:
//!
//! * a client says it has drawn, so its surface is copied to the display and
//!   it is told it may draw again;
//! * a client has ended, so its tile is cleared and it is forgotten.
//!
//! Both arrive through the same wait, which is why a wait set had to exist
//! before this program could. A compositor that blocked reading one client
//! would stop compositing for everyone else the moment that client stopped
//! talking, and one that could not hear a client *end* would hold a dead
//! client's tile on screen forever.
//!
//! # What it is not
//!
//! There are no windows, no stacking, no input routing and no resizing. Tiles
//! are laid out once and never move. Those are all worth having and none of
//! them is what this establishes, which is that the path from a client's pixel
//! to the display runs through a process rather than through the kernel.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel the kernel hands the display over on.
const KERNEL: Handle = Handle(1);
/// The channel to whoever is allowed to start programs.
const SPAWNER: Handle = Handle(2);

/// Where this program maps the framebuffer.
const FRAMEBUFFER_AT: usize = 0x0000_0000_2000_0000;
/// Where it maps the first client surface; the second goes a stride further on.
const SURFACES_AT: usize = 0x0000_0000_3000_0000;
/// Address space set aside per surface, which bounds how large a tile may be.
const SURFACE_STRIDE: usize = 0x0010_0000;

/// The program this one gives surfaces to.
const CLIENT: &[u8] = b"BIN/CLIENT.ELF";
/// How many of them.
const CLIENTS: usize = 2;
/// Pixels between two tiles, and around them.
const GAP: u32 = 8;

/// What the kernel says about the display.
struct Screen {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// Pixels per scanline, which is not always the width.
    stride: u32,
    bytes_per_pixel: u32,
    screen_width: u32,
    screen_height: u32,
}

/// One client, and everything this program knows about it.
struct Tile {
    /// The channel it talks on, and the process, so its ending can be heard.
    channel: Handle,
    process: Handle,
    /// The memory both this program and the client map.
    surface: Handle,
    /// Where this program mapped it.
    mapped_at: usize,
    /// Where it goes on the display, and how large it is.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// Still worth listening to.
    live: bool,
    /// Frames composited from it, for the report at the end.
    frames: u32,
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
    let Some(screen) = take_the_display() else {
        finish();
    };

    // Two tiles side by side, with a gap around and between them. Laid out
    // before any client exists, because a client is told the size of its
    // surface and cannot be told twice.
    let tile_width = (screen.width - GAP * (CLIENTS as u32 + 1)) / CLIENTS as u32;
    let tile_height = screen.height - GAP * 2;
    if tile_width == 0 || tile_height == 0 {
        failed("compositor: FAILED: the rectangle it was given is too small to divide");
        finish();
    }

    let mut tiles: [Option<Tile>; CLIENTS] = [const { None }; CLIENTS];
    for index in 0..CLIENTS {
        let x = screen.x + GAP + (tile_width + GAP) * index as u32;
        let y = screen.y + GAP;
        match start_client(index, x, y, tile_width, tile_height) {
            Some(tile) => tiles[index] = Some(tile),
            None => finish(),
        }
    }

    serve(&screen, &mut tiles);
    finish()
}

/// Take the display from the kernel.
///
/// One message: the rectangle this program may use and the shape of the
/// framebuffer, as eight little-endian numbers, and a handle to the memory.
/// Nothing is discovered and nothing is assumed — a program that guessed the
/// stride would draw a diagonal smear on the first machine whose scanlines are
/// padded.
fn take_the_display() -> Option<Screen> {
    let mut buffer = [0u8; 64];
    let mut handles = [Handle(0); 1];

    let received = match nexus_user::receive(KERNEL, &mut buffer, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("compositor: FAILED: the kernel never handed over the display");
            return None;
        }
    };
    if received.handles != 1 || received.bytes < 32 {
        failed("compositor: FAILED: no framebuffer came with the message");
        return None;
    }

    let screen = Screen {
        x: read_u32(&buffer, 0),
        y: read_u32(&buffer, 4),
        width: read_u32(&buffer, 8),
        height: read_u32(&buffer, 12),
        stride: read_u32(&buffer, 16),
        bytes_per_pixel: read_u32(&buffer, 20),
        screen_width: read_u32(&buffer, 24),
        screen_height: read_u32(&buffer, 28),
    };

    if screen.bytes_per_pixel != 4 {
        failed("compositor: FAILED: this program only understands 32-bit pixels");
        return None;
    }
    // Checked against the screen rather than trusted, even though the kernel
    // sent it. A compositor that drew outside what it was given is the one
    // program on the machine that must not.
    if screen.x + screen.width > screen.screen_width
        || screen.y + screen.height > screen.screen_height
    {
        failed("compositor: FAILED: the rectangle is not on the screen");
        return None;
    }

    let framebuffer = handles[0];
    let Ok(size) = nexus_user::memory_size(framebuffer) else {
        failed("compositor: FAILED: could not ask how large the framebuffer is");
        return None;
    };
    if nexus_user::memory_map(framebuffer, FRAMEBUFFER_AT, true) != Ok(size) {
        failed("compositor: FAILED: could not map the framebuffer");
        return None;
    }

    Some(screen)
}

/// Start one client and give it a surface.
///
/// The surface is made here, mapped here, and *duplicated* before it is sent:
/// handles move when they cross a channel, so sending the only one would hand
/// the memory away — and the client ending would free the frames out from under
/// this program's own mapping of them.
fn start_client(index: usize, x: u32, y: u32, width: u32, height: u32) -> Option<Tile> {
    let bytes = width as usize * height as usize * 4;
    if bytes > SURFACE_STRIDE {
        failed("compositor: FAILED: a tile is larger than the space set aside for it");
        return None;
    }

    let Ok(surface) = nexus_user::memory_create(bytes) else {
        failed("compositor: FAILED: could not make a surface");
        return None;
    };
    let mapped_at = SURFACES_AT + index * SURFACE_STRIDE;
    if nexus_user::memory_map(surface, mapped_at, true).is_err() {
        failed("compositor: FAILED: could not map a surface");
        return None;
    }

    // Ask for the program. There is no call that starts one: there is a
    // channel, and holding an end of it is the authority to ask.
    if nexus_user::send(SPAWNER, CLIENT, &[]).is_err() {
        failed("compositor: FAILED: could not reach the spawn service");
        return None;
    }
    let mut reply = [0u8; 64];
    let mut handles = [Handle(0); 2];
    let received = match nexus_user::receive(SPAWNER, &mut reply, &mut handles) {
        Ok(received) => received,
        Err(_) => {
            failed("compositor: FAILED: the spawn service did not answer");
            return None;
        }
    };
    if received.handles != 2 {
        let text = core::str::from_utf8(&reply[..received.bytes]).unwrap_or("<not text>");
        nexus_user::log(text).ok();
        failed("compositor: FAILED: no client came back");
        return None;
    }
    let channel = handles[0];
    let process = handles[1];

    // Read and write, so the client can draw into the surface and ask how large
    // it is. And transfer, because that is the right a handle needs to *cross a
    // channel at all* -- without it this could not be given away, which is the
    // one thing it exists to do.
    //
    // Not close. A compositor whose client could close the surface out from
    // under it would be a compositor compositing from freed memory.
    let Ok(theirs) = nexus_user::duplicate(
        surface,
        nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
    ) else {
        failed("compositor: FAILED: could not duplicate a surface handle");
        return None;
    };

    let mut message = [0u8; 12];
    message[0..4].copy_from_slice(&width.to_le_bytes());
    message[4..8].copy_from_slice(&height.to_le_bytes());
    // A different tint per client, so two tiles that are the same colour mean
    // one buffer reached both and not that compositing worked.
    message[8..12].copy_from_slice(&(0x40u32 + index as u32 * 0x70).to_le_bytes());

    if nexus_user::send(channel, &message, &[theirs]).is_err() {
        failed("compositor: FAILED: could not give a client its surface");
        return None;
    }

    Some(Tile {
        channel,
        process,
        surface,
        mapped_at,
        x,
        y,
        width,
        height,
        live: true,
        frames: 0,
    })
}

/// The loop.
///
/// One wait over every client's channel and every client's process. A client
/// saying it has drawn and a client dying arrive the same way, which is the
/// only arrangement in which neither can be starved by the other.
fn serve(screen: &Screen, tiles: &mut [Option<Tile>; CLIENTS]) {
    let Ok(set) = nexus_user::wait_set() else {
        failed("compositor: FAILED: could not make a wait set");
        return;
    };

    // Keys this program chose. The low bit says which of the two things about a
    // client became ready, and the rest says which client -- so one number
    // names both without a table to look it up in.
    for (index, tile) in tiles.iter().enumerate() {
        let Some(tile) = tile else { continue };
        if nexus_user::watch(set, tile.channel, channel_key(index)).is_err()
            || nexus_user::watch(set, tile.process, process_key(index)).is_err()
        {
            failed("compositor: FAILED: could not watch a client");
            return;
        }
    }

    let mut composited = 0u32;
    let mut ended = 0usize;

    // Bounded, so a client that neither draws nor dies cannot hang the machine.
    // The bound is generous: it is a backstop and not a schedule.
    for _ in 0..256 {
        if ended == CLIENTS {
            break;
        }

        let mut keys = [0u64; CLIENTS * 2];
        let Ok(count) = nexus_user::wait_any(set, &mut keys) else {
            failed("compositor: FAILED: could not wait on its clients");
            return;
        };
        if count == 0 {
            break;
        }

        for key in &keys[..count] {
            let index = (*key >> 1) as usize;
            let Some(Some(tile)) = tiles.get_mut(index) else {
                failed("compositor: FAILED: a wait set returned a key it was never given");
                return;
            };

            if key & 1 == 0 {
                // Something on its channel. A client that has ended shows up
                // here too -- its end of the channel is gone -- so the read is
                // what tells the two apart.
                let mut message = [0u8; 32];
                let mut none = [Handle(0); 1];
                match nexus_user::receive(tile.channel, &mut message, &mut none) {
                    Ok(_) => {
                        composite(screen, tile);
                        tile.frames += 1;
                        composited += 1;
                        // Answered, so the client knows the buffer is free
                        // again. Without this it would redraw while this
                        // program was still reading, which is what tearing is.
                        if nexus_user::send(tile.channel, b"shown", &[]).is_err() {
                            // It has gone between the message and the reply,
                            // which is ordinary and not a failure.
                            stop_listening(set, tile, index);
                        }
                    }
                    Err(_) => stop_listening(set, tile, index),
                }
            } else {
                // It has ended. Its tile is cleared, because a dead client's
                // last frame left on screen is a lie about what is running.
                clear(screen, tile);
                if tile.live {
                    tile.live = false;
                    ended += 1;
                }
                nexus_user::unwatch(set, *key).ok();
                nexus_user::unwatch(set, channel_key(index)).ok();
            }
        }
    }

    for tile in tiles.iter().flatten() {
        nexus_user::close(tile.surface).ok();
        nexus_user::close(tile.channel).ok();
        nexus_user::close(tile.process).ok();
    }
    nexus_user::close(set).ok();

    if composited == 0 {
        failed("compositor: FAILED: no client ever drew anything");
        return;
    }
    nexus_user::log("compositor: composited every frame its clients drew").ok();
}

/// Stop hearing from a client's channel, without deciding it has ended.
///
/// A channel can close before its process does. Forgetting the channel and
/// waiting for the process is what keeps the two facts separate.
fn stop_listening(set: Handle, tile: &Tile, index: usize) {
    let _ = tile;
    nexus_user::unwatch(set, channel_key(index)).ok();
}

/// The key naming a client's channel, and its process.
fn channel_key(index: usize) -> u64 {
    (index as u64) << 1
}
fn process_key(index: usize) -> u64 {
    ((index as u64) << 1) | 1
}

/// Copy a client's surface onto the display.
///
/// Row by row, because the surface is tightly packed and the framebuffer is
/// not: the display's scanlines are as long as the hardware says, which is not
/// always as long as the picture.
fn composite(screen: &Screen, tile: &Tile) {
    for row in 0..tile.height {
        let source = tile.mapped_at + (row as usize * tile.width as usize) * 4;
        let destination = FRAMEBUFFER_AT
            + (((tile.y + row) as usize * screen.stride as usize) + tile.x as usize) * 4;

        for column in 0..tile.width as usize {
            // SAFETY: both mappings are live and writable, the source offset is
            // inside a surface of `width * height * 4` bytes, and the
            // destination was checked against the screen when the display was
            // taken and is bounded by this tile's own rectangle.
            unsafe {
                let pixel = core::ptr::read_volatile((source + column * 4) as *const u32);
                core::ptr::write_volatile((destination + column * 4) as *mut u32, pixel);
            }
        }
    }
}

/// Blank a tile, for a client that is not coming back.
fn clear(screen: &Screen, tile: &Tile) {
    for row in 0..tile.height {
        let destination = FRAMEBUFFER_AT
            + (((tile.y + row) as usize * screen.stride as usize) + tile.x as usize) * 4;
        for column in 0..tile.width as usize {
            // SAFETY: as in `composite`, and writing a constant rather than
            // reading anything.
            unsafe {
                core::ptr::write_volatile((destination + column * 4) as *mut u32, 0x0009_1428);
            }
        }
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
    nexus_user::log("compositor: PANIC").ok();
    nexus_user::exit_with(2)
}
