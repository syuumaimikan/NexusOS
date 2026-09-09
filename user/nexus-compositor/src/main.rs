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
//! * a client has ended, so its tile is cleared and it is forgotten;
//! * a key was pressed, so it goes to whichever client has focus.
//!
//! All three arrive through the same wait, which is why a wait set had to exist
//! before this program could. A compositor that blocked reading one client
//! would stop compositing for everyone else the moment that client stopped
//! talking; one that could not hear a client *end* would hold a dead client's
//! tile on screen forever; and one that had to choose between waiting for a
//! client and waiting for the keyboard would be deaf to one of them.
//!
//! # Focus
//!
//! Keys go to one client and not to all of them, and tab moves which. That is
//! the first piece of policy this program owns rather than the kernel: the
//! kernel knows a key was pressed and has no idea what a window is, let alone
//! which one someone is looking at. A compositor that sent every key to every
//! client would be broadcasting rather than routing, and that is how what
//! someone types into one window arrives in another.
//!
//! Which client has focus is drawn as a ring around its tile — by this program,
//! over the client's own pixels, after its surface has been copied out. That is
//! what a decoration is: something the client did not draw, cannot draw and
//! cannot remove. A client that could paint its own focus ring could claim a
//! focus it does not have.
//!
//! # What it is not
//!
//! There are no windows, no stacking and no resizing. Tiles are laid out once
//! and never move. Those are worth having and none of them is what this
//! establishes, which is that the path from a client's pixel to the display,
//! and from a keystroke to a client, runs through a process rather than through
//! the kernel.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nexus_user::Handle;

/// The channel the kernel hands the display over on.
const KERNEL: Handle = Handle(1);
/// The channel to whoever is allowed to start programs.
const SPAWNER: Handle = Handle(2);
/// The channel keys arrive on.
const KEYS: Handle = Handle(3);
/// The channel pointer movements arrive on.
const POINTER: Handle = Handle(4);

/// The keys this program gives the keyboard and the pointer in its wait set.
///
/// Above anything a client can be given, since client keys are an index shifted
/// left with a bit for which of its two things became ready.
const KEY_KEYBOARD: u64 = 0xFFFF;
const KEY_POINTER: u64 = 0xFFFE;

/// The pointer, and what it is over.
///
/// Where it is on the screen is this program's business and nobody else's. The
/// mouse reports that it *moved*, never where it is -- it cannot know, because
/// where a pointer is depends on how large the screen is and what is on it.
struct Pointer {
    x: u32,
    y: u32,
    /// Whether a button was down at the last report, so that a press can be
    /// told from being held. A compositor that acted on "down" rather than on
    /// "went down" would refocus a window forty times a second while somebody
    /// held the button.
    held: bool,
    /// What is under it, saved before the cursor was drawn over it.
    beneath: [u32; CURSOR * CURSOR],
    /// Whether `beneath` holds anything, and where it was taken from.
    drawn: Option<(u32, u32)>,
}

/// How large the pointer is, in pixels.
const CURSOR: usize = 10;

/// What a movement looks like on the wire: two signed numbers and the buttons.
mod pointer {
    pub const SIZE: usize = 12;
    pub const LEFT: u32 = 1 << 0;
}

/// What a key looks like on the wire: a kind, then a number.
///
/// Only tab is named here, because it is the only one this program acts on
/// itself. Everything else is forwarded without being looked at -- a compositor
/// that inspected the keys it routes would be a compositor that could read what
/// someone typed into a window.
mod key {
    pub const TAB: u8 = 5;
    /// Bytes one key takes.
    pub const SIZE: usize = 5;
}

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
    /// Keys forwarded to it, likewise.
    keys: u32,
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

    let mut message = [0u8; 16];
    message[0..4].copy_from_slice(&width.to_le_bytes());
    message[4..8].copy_from_slice(&height.to_le_bytes());
    // A different tint per client, so two tiles that are the same colour mean
    // one buffer reached both and not that compositing worked.
    message[8..12].copy_from_slice(&(0x40u32 + index as u32 * 0x70).to_le_bytes());
    // And which client it is, only so that it can say so. A client has no use
    // for the number beyond naming itself in a log -- it cannot address another
    // client, and there is nothing for it to index into.
    message[12..16].copy_from_slice(&(index as u32).to_le_bytes());

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
        keys: 0,
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

    if nexus_user::watch(set, KEYS, KEY_KEYBOARD).is_err() {
        failed("compositor: FAILED: could not watch the keyboard");
        return;
    }
    // A machine with no mouse has no pointer channel worth watching, and
    // watching one that will never carry anything is a member that is never
    // ready. It is not an error, so it is not reported as one.
    let pointing = nexus_user::watch(set, POINTER, KEY_POINTER).is_ok();

    // Started in the middle of the rectangle this program owns, because there
    // is nowhere else meaningful for it to be before anyone has moved it.
    let mut cursor = Pointer {
        x: screen.x + screen.width / 2,
        y: screen.y + screen.height / 2,
        held: false,
        beneath: [0; CURSOR * CURSOR],
        drawn: None,
    };
    if pointing {
        draw_cursor(screen, &mut cursor);
    }

    let mut composited = 0u32;
    let mut moved = 0u32;
    let mut forwarded = 0u32;
    let mut ended = 0usize;
    // Which client keys go to. A compositor without this would have to send
    // every key to everyone, which is not routing -- it is broadcasting, and it
    // is how a password ends up in a program that was only ever on screen.
    let mut focus = 0usize;

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
            if *key == KEY_KEYBOARD {
                match read_key(screen, set, tiles, &mut focus) {
                    Some(sent) => forwarded += sent,
                    None => return,
                }
                continue;
            }

            if *key == KEY_POINTER {
                match read_pointer(screen, set, tiles, &mut cursor, &mut focus) {
                    Some(steps) => moved += steps,
                    None => return,
                }
                continue;
            }

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
                        // Lifted before the tile is painted and put back after.
                        // Compositing writes over whatever was there, and what
                        // was there includes the pointer -- a compositor that
                        // forgot this leaves a trail of cursors behind it.
                        lift_cursor(screen, &mut cursor);
                        composite(screen, tile);
                        outline(screen, tile, index == focus);
                        if pointing {
                            draw_cursor(screen, &mut cursor);
                        }
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
    if moved > 0 {
        nexus_user::log("compositor: moved a pointer of its own across the display").ok();
    }
    if forwarded > 0 {
        nexus_user::log("compositor: routed keys to the client that had focus").ok();
    }
}

/// Read whatever the keyboard sent and decide who it is for.
///
/// Tab moves the focus and is not passed on, which is the first piece of policy
/// this program owns rather than the kernel: the kernel knows a key was pressed
/// and has no idea what a window is. Everything else goes to the focused
/// client, and to nobody else -- a compositor that sent every key to every
/// client would be broadcasting, not routing, and that is how what someone
/// types into one window ends up in another.
///
/// Returns how many keys were passed on, or `None` if something went wrong
/// badly enough to stop.
fn read_key(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    focus: &mut usize,
) -> Option<u32> {
    let mut message = [0u8; 32];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(KEYS, &mut message, &mut none) else {
        // The kernel has stopped sending. Not a failure: it means there is no
        // keyboard any more, and there is still a screen to composite.
        nexus_user::unwatch(set, KEY_KEYBOARD).ok();
        return Some(0);
    };
    if received.bytes < key::SIZE {
        failed("compositor: FAILED: a key arrived in the wrong shape");
        return None;
    }

    if message[0] == key::TAB {
        // Round-robin over the clients that are still alive. A focus that could
        // land on a dead client would send its keys nowhere.
        for step in 1..=CLIENTS {
            let candidate = (*focus + step) % CLIENTS;
            if matches!(tiles.get(candidate), Some(Some(tile)) if tile.live) {
                *focus = candidate;
                break;
            }
        }
        nexus_user::log(if *focus == 0 {
            "compositor: focus moved to the first client"
        } else {
            "compositor: focus moved to the second client"
        })
        .ok();

        // Redrawn now rather than at the next frame, so the ring follows the
        // focus rather than the focus following a client's drawing.
        for (index, tile) in tiles.iter().enumerate() {
            if let Some(tile) = tile {
                if tile.live {
                    outline(screen, tile, index == *focus);
                }
            }
        }
        return Some(0);
    }

    let Some(Some(tile)) = tiles.get_mut(*focus) else {
        return Some(0);
    };
    if !tile.live {
        return Some(0);
    }
    if nexus_user::send(tile.channel, &message[..key::SIZE], &[]).is_err() {
        // It has gone. Ordinary, and the process key will say so.
        return Some(0);
    }
    tile.keys += 1;
    Some(1)
}

/// Read whatever the mouse sent and move the pointer.
///
/// Movement is relative and arrives with no idea of where anything is. Turning
/// it into a position is this program's job, because a position only means
/// something against a screen and a set of windows -- and the kernel has
/// neither.
///
/// A press inside a tile focuses it. That is the second piece of policy this
/// program owns: the kernel knows a button went down and has no idea what it
/// went down *on*.
///
/// Returns how many movements were handled, or `None` to stop.
fn read_pointer(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    cursor: &mut Pointer,
    focus: &mut usize,
) -> Option<u32> {
    let mut message = [0u8; 32];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(POINTER, &mut message, &mut none) else {
        // The kernel has stopped sending; there is still a screen to composite.
        nexus_user::unwatch(set, KEY_POINTER).ok();
        return Some(0);
    };
    if received.bytes < pointer::SIZE {
        failed("compositor: FAILED: a pointer movement arrived in the wrong shape");
        return None;
    }

    let dx = read_i32(&message, 0);
    let dy = read_i32(&message, 4);
    let buttons = read_u32(&message, 8);

    // Clamped to the rectangle this program owns. A pointer that could leave it
    // would be drawn over the kernel's panel, which this program does not own
    // and must not touch.
    lift_cursor(screen, cursor);
    cursor.x = clamp(
        i64::from(cursor.x) + i64::from(dx),
        screen.x,
        screen.x + screen.width - CURSOR as u32,
    );
    // The mouse reports Y increasing upwards and the screen has it increasing
    // downwards, so this is where the two are reconciled -- not in the kernel,
    // which has no screen to be upside down with respect to.
    cursor.y = clamp(
        i64::from(cursor.y) - i64::from(dy),
        screen.y,
        screen.y + screen.height - CURSOR as u32,
    );

    let pressed = buttons & pointer::LEFT != 0;
    if pressed && !cursor.held {
        for (index, tile) in tiles.iter().enumerate() {
            let Some(tile) = tile else { continue };
            if !tile.live {
                continue;
            }
            if cursor.x >= tile.x
                && cursor.x < tile.x + tile.width
                && cursor.y >= tile.y
                && cursor.y < tile.y + tile.height
                && *focus != index
            {
                *focus = index;
                nexus_user::log(if index == 0 {
                    "compositor: the pointer gave focus to the first client"
                } else {
                    "compositor: the pointer gave focus to the second client"
                })
                .ok();
            }
        }
        // Redrawn now, so the ring follows the click rather than the next frame.
        for (index, tile) in tiles.iter().enumerate() {
            if let Some(tile) = tile {
                if tile.live {
                    outline(screen, tile, index == *focus);
                }
            }
        }
    }
    cursor.held = pressed;

    draw_cursor(screen, cursor);
    Some(1)
}

/// Keep a number inside a range, from a wider one that may have gone outside it.
fn clamp(value: i64, low: u32, high: u32) -> u32 {
    if value < i64::from(low) {
        low
    } else if value > i64::from(high) {
        high
    } else {
        value as u32
    }
}

/// Put back what the pointer was covering.
fn lift_cursor(screen: &Screen, cursor: &mut Pointer) {
    let Some((x, y)) = cursor.drawn.take() else {
        return;
    };
    for row in 0..CURSOR {
        for column in 0..CURSOR {
            let offset = (((y as usize + row) * screen.stride as usize) + x as usize + column) * 4;
            // SAFETY: the framebuffer is mapped writable and this address is
            // inside the rectangle this program owns, which the cursor is
            // clamped to.
            unsafe {
                core::ptr::write_volatile(
                    (FRAMEBUFFER_AT + offset) as *mut u32,
                    cursor.beneath[row * CURSOR + column],
                );
            }
        }
    }
}

/// Save what is under the pointer and draw it.
///
/// An arrow, of a sort: a triangle with a light edge, so that it is visible
/// over a client's gradient and over the background alike. Saving first is what
/// makes moving it cheap -- there is no second buffer to composite from, so the
/// pixels it covers have nowhere else to be kept.
fn draw_cursor(screen: &Screen, cursor: &mut Pointer) {
    for row in 0..CURSOR {
        for column in 0..CURSOR {
            let offset =
                (((cursor.y as usize + row) * screen.stride as usize) + cursor.x as usize + column)
                    * 4;
            // SAFETY: as in `lift_cursor`.
            let under =
                unsafe { core::ptr::read_volatile((FRAMEBUFFER_AT + offset) as *const u32) };
            cursor.beneath[row * CURSOR + column] = under;

            // The arrow: filled where the column is inside the row, edged on
            // the diagonal, and nothing outside it.
            let colour = if column > row {
                continue;
            } else if column == row || column == 0 || row == CURSOR - 1 {
                0x00F2_F6FF
            } else {
                0x0012_1A2A
            };

            // SAFETY: as above.
            unsafe {
                core::ptr::write_volatile((FRAMEBUFFER_AT + offset) as *mut u32, colour);
            }
        }
    }
    cursor.drawn = Some((cursor.x, cursor.y));
}

/// Read a little-endian signed `i32` out of a message.
fn read_i32(buffer: &[u8], offset: usize) -> i32 {
    read_u32(buffer, offset) as i32
}

/// Draw a ring around a tile saying whether it has focus.
///
/// Drawn by this program, over the client's own pixels, after its surface has
/// been copied out. That is what a decoration is: something the client did not
/// draw, cannot draw, and cannot remove -- a client that could paint its own
/// focus ring could claim focus it does not have.
fn outline(screen: &Screen, tile: &Tile, focused: bool) {
    let colour = if focused { 0x0046_C8FF } else { 0x0020_2C3C };

    for row in 0..tile.height {
        let edge = row < 2 || row + 2 >= tile.height;
        let destination = FRAMEBUFFER_AT
            + (((tile.y + row) as usize * screen.stride as usize) + tile.x as usize) * 4;

        for column in 0..tile.width as usize {
            if !edge && column >= 2 && column + 2 < tile.width as usize {
                continue;
            }
            // SAFETY: as in `composite` -- the framebuffer is mapped writable
            // and this address is inside this tile's own rectangle, which was
            // checked against the screen when the display was taken.
            unsafe {
                core::ptr::write_volatile((destination + column * 4) as *mut u32, colour);
            }
        }
    }
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
