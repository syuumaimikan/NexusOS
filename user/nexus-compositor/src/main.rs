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
//! # Windows
//!
//! A window can be picked up by its title bar and carried, and one that is
//! clicked comes to the front. Both are this program's alone: the client is
//! never told where it is, so it cannot notice being moved, and the bar is a
//! decoration -- drawn over the client's own pixels, after its surface has been
//! copied out, so a client can neither draw one nor remove the one it has.
//!
//! Once windows can move they can overlap, and once they overlap there has to
//! be an order. It is kept back-to-front and a click raises what was clicked.
//! Painting is then a full recomposite of the rectangle this program owns:
//! clear it, and draw every window in order. That is more work than repainting
//! what changed, and it is what makes overlap simply work -- a compositor that
//! repainted only the damaged tile would leave the window above it with a hole
//! in it, and getting that right needs the damage arithmetic that this does not
//! have and does not yet need. The rectangle is ninety thousand pixels.
//!
//! # Resizing
//!
//! A window has a grip in its bottom-right corner, and dragging it makes the
//! window bigger or smaller. That needs a *new surface*: a client's surface is
//! tightly packed, so its size is its shape, and there is no changing one
//! without replacing the other. So the compositor makes a new memory object,
//! copies across what still fits, hands the client a handle to it, and unmaps
//! and drops the old one.
//!
//! The client is told a size and given a handle and nothing else. It is not
//! told why, or where the window is, or that anyone can see it -- a client that
//! had to be told why its window changed size would be a client that knew it
//! had a window.
//!
//! Copying across what still fits is not necessary and it is what keeps a
//! resize from flashing: without it the window is blank until the client draws
//! its next frame, which at two frames a second is half a second of black.
//!
//! # What it is not
//!
//! No minimising, and no window that is not a rectangle.

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
    /// The window being carried, and where it was grabbed within it.
    ///
    /// The offset matters: without it a window would jump so that its corner
    /// met the pointer the instant it was picked up, which is not what picking
    /// something up looks like.
    dragging: Option<Drag>,
}

/// What dragging is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Doing {
    /// Carrying the window.
    Moving,
    /// Changing its size from the bottom-right corner.
    Resizing,
}

/// A window being carried or resized.
struct Drag {
    tile: usize,
    doing: Doing,
    grab_x: u32,
    grab_y: u32,
    /// Whether this drag has already said so.
    ///
    /// Said when the window first *moves*, not when it is taken hold of: a
    /// press on a title bar that never moves anywhere is not a window being
    /// carried, and a line per movement would be a hundred lines a second.
    announced: bool,
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
/// Address space set aside per surface.
///
/// Twice what one surface may be, because a resize has the old and the new
/// mapped at once for the length of the copy between them.
const SURFACE_STRIDE: usize = 0x0010_0000;
/// The largest a surface may be, which is what bounds how large a window is.
const MAX_SURFACE: usize = SURFACE_STRIDE / 2;

/// The program this one gives surfaces to.
const CLIENT: &[u8] = b"BIN/CLIENT.ELF";
/// How many of them.
const CLIENTS: usize = 2;
/// Pixels between two tiles, and around them.
const GAP: u32 = 8;
/// How tall a window's title bar is.
///
/// Drawn over the top of the client's own surface rather than taking space away
/// from it. Taking space would mean the client's surface and its window were
/// different sizes, which is a second rectangle to keep in step for the sake of
/// fourteen pixels a client was going to fill with its own border anyway.
const TITLE: u32 = 14;
/// How large the corner is that resizes a window.
const GRIP: u32 = 12;
/// The smallest a window may be made.
///
/// Small enough to be a real constraint and large enough that a window can
/// still be grabbed: below the height of a title bar plus a grip there would be
/// nothing left to take hold of, and a window that cannot be picked up cannot
/// be made bigger again.
const MIN_SIZE: u32 = 48;

/// The background of the rectangle this program owns.
///
/// Its own, not the kernel's. What is behind the windows is the compositor's
/// business, and matching the kernel's gradient exactly would mean knowing how
/// the kernel draws -- which is the coupling this whole arrangement removes.
const BACKGROUND: u32 = 0x0009_1428;

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

impl Tile {
    /// Whether a point is inside this window.
    fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Whether a point is in the strip that can be taken hold of.
    fn title_contains(&self, x: u32, y: u32) -> bool {
        self.contains(x, y) && y < self.y + TITLE
    }

    /// Whether a point is in the corner that resizes.
    ///
    /// Checked before the title bar is, because on a window small enough for
    /// the two to overlap the grip is the one that has to win: a window can
    /// always be moved by the rest of its bar, and a window too small to resize
    /// is a window that can never be made bigger.
    fn grip_contains(&self, x: u32, y: u32) -> bool {
        self.contains(x, y) && x + GRIP >= self.x + self.width && y + GRIP >= self.y + self.height
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
    if bytes > MAX_SURFACE {
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
        dragging: None,
    };

    let mut composited = 0u32;
    let mut moved = 0u32;
    let mut forwarded = 0u32;
    let mut ended = 0usize;
    // Which client keys go to. A compositor without this would have to send
    // every key to everyone, which is not routing -- it is broadcasting, and it
    // is how a password ends up in a program that was only ever on screen.
    let mut focus = 0usize;
    // Back to front. Windows can overlap now, so there has to be an order, and
    // a click raises what was clicked.
    let mut order: [usize; CLIENTS] = core::array::from_fn(|index| index);

    repaint(screen, tiles, &order, focus, &mut cursor, pointing);

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
                match read_key(set, tiles, &mut focus) {
                    Some(sent) => {
                        forwarded += sent;
                        repaint(screen, tiles, &order, focus, &mut cursor, pointing);
                    }
                    None => return,
                }
                continue;
            }

            if *key == KEY_POINTER {
                match read_pointer(screen, set, tiles, &mut cursor, &mut focus, &mut order) {
                    Some(_) => {
                        moved += 1;
                        repaint(screen, tiles, &order, focus, &mut cursor, pointing);
                    }
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
                        repaint(screen, tiles, &order, focus, &mut cursor, pointing);
                    }
                    Err(_) => stop_listening(set, tile, index),
                }
            } else {
                // It has ended. Its window goes, because a dead client's last
                // frame left on screen is a lie about what is running.
                if tile.live {
                    tile.live = false;
                    ended += 1;
                }
                nexus_user::unwatch(set, *key).ok();
                nexus_user::unwatch(set, channel_key(index)).ok();
                repaint(screen, tiles, &order, focus, &mut cursor, pointing);
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
fn read_key(set: Handle, tiles: &mut [Option<Tile>; CLIENTS], focus: &mut usize) -> Option<u32> {
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
        // The caller repaints; the ring follows the focus rather than waiting
        // for a client to draw.
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

/// Read whatever the mouse sent, and act on it.
///
/// Movement is relative and arrives with no idea of where anything is. Turning
/// it into a position is this program's job, because a position only means
/// something against a screen and a set of windows -- and the kernel has
/// neither.
///
/// A press in a window raises it and gives it focus; a press in its *title bar*
/// also picks it up. Both are policy this program owns: the kernel knows a
/// button went down and has no idea what it went down on.
///
/// Returns 1 if a window was carried this step, 0 otherwise, or `None` to stop.
fn read_pointer(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    cursor: &mut Pointer,
    focus: &mut usize,
    order: &mut [usize; CLIENTS],
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
    let mut carried = 0;

    if pressed && !cursor.held {
        // Front to back, so a press lands on what is visible rather than on
        // whatever happens to be first in the array. A compositor that searched
        // the other way would give focus to the window *under* the one that was
        // clicked, which looks like the click going through it.
        for &index in order.iter().rev() {
            let Some(tile) = tiles.get(index).and_then(Option::as_ref) else {
                continue;
            };
            if !tile.live || !tile.contains(cursor.x, cursor.y) {
                continue;
            }

            if tile.grip_contains(cursor.x, cursor.y) {
                cursor.dragging = Some(Drag {
                    tile: index,
                    doing: Doing::Resizing,
                    // How far the pointer is from the corner it is dragging, so
                    // the corner follows the pointer rather than jumping to it.
                    grab_x: (tile.x + tile.width).saturating_sub(cursor.x),
                    grab_y: (tile.y + tile.height).saturating_sub(cursor.y),
                    announced: false,
                });
            } else if tile.title_contains(cursor.x, cursor.y) {
                cursor.dragging = Some(Drag {
                    tile: index,
                    doing: Doing::Moving,
                    grab_x: cursor.x - tile.x,
                    grab_y: cursor.y - tile.y,
                    announced: false,
                });
            }

            raise(order, index);
            if *focus != index {
                *focus = index;
                nexus_user::log(if index == 0 {
                    "compositor: the pointer gave focus to the first client"
                } else {
                    "compositor: the pointer gave focus to the second client"
                })
                .ok();
            }
            break;
        }
    }

    if !pressed {
        cursor.dragging = None;
    }

    if let Some(drag) = &mut cursor.dragging {
        let index = drag.tile;
        let doing = drag.doing;
        let (grab_x, grab_y) = (drag.grab_x, drag.grab_y);
        let announced = drag.announced;
        let mut announce = None;

        if let Some(Some(tile)) = tiles.get_mut(index) {
            match doing {
                Doing::Moving => {
                    // Where the window would go if the point that was grabbed
                    // stayed under the pointer -- clamped so no part of it
                    // leaves the rectangle this program owns.
                    let wanted_x = i64::from(cursor.x) - i64::from(grab_x);
                    let wanted_y = i64::from(cursor.y) - i64::from(grab_y);
                    let moved_x = clamp(wanted_x, screen.x, screen.x + screen.width - tile.width);
                    let moved_y = clamp(wanted_y, screen.y, screen.y + screen.height - tile.height);
                    if moved_x != tile.x || moved_y != tile.y {
                        tile.x = moved_x;
                        tile.y = moved_y;
                        carried = 1;
                        if !announced {
                            announce = Some("compositor: carried a window by its title bar");
                        }
                    }
                }
                Doing::Resizing => {
                    // The far corner follows the pointer; the near one stays.
                    let corner_x = i64::from(cursor.x) + i64::from(grab_x);
                    let corner_y = i64::from(cursor.y) + i64::from(grab_y);
                    let wanted_width = clamp(
                        corner_x - i64::from(tile.x),
                        MIN_SIZE,
                        screen.x + screen.width - tile.x,
                    );
                    let wanted_height = clamp(
                        corner_y - i64::from(tile.y),
                        MIN_SIZE,
                        screen.y + screen.height - tile.y,
                    );

                    if wanted_width != tile.width || wanted_height != tile.height {
                        match resize(index, tile, wanted_width, wanted_height) {
                            Ok(()) => {
                                carried = 1;
                                if !announced {
                                    announce = Some("compositor: resized a window by its corner");
                                }
                            }
                            // A resize that could not be done leaves the window
                            // as it was, which is what a client that is still
                            // drawing into its old surface needs.
                            Err(()) => return None,
                        }
                    }
                }
            }
        }

        if let Some(text) = announce {
            drag.announced = true;
            nexus_user::log(text).ok();
        }
    }

    cursor.held = pressed;
    Some(carried)
}

/// Give a window a new surface of a new size.
///
/// A surface is tightly packed, so its size *is* its shape: there is no
/// changing one without replacing the other. So a new memory object is made,
/// what still fits is copied across, the client is handed a handle to it, and
/// the old one is unmapped and dropped.
///
/// The copy is not necessary and it is what keeps a resize from flashing.
/// Without it the window is blank until the client's next frame, which at two
/// frames a second is half a second of black.
///
/// `Err` means the client has gone or the memory could not be had, and the
/// window is left exactly as it was -- which is what a client still drawing
/// into its old surface needs.
fn resize(index: usize, tile: &mut Tile, width: u32, height: u32) -> Result<(), ()> {
    let bytes = width as usize * height as usize * 4;
    if bytes > MAX_SURFACE {
        // Larger than the space set aside per surface. Refused rather than
        // clamped, because a window that quietly stopped growing would be a
        // window whose size did not match what its client was told.
        return Ok(());
    }

    let Ok(fresh) = nexus_user::memory_create(bytes) else {
        return Ok(());
    };
    // Two surfaces are mapped at once for the length of the copy, which is why
    // the space set aside per surface is twice what one needs.
    let staging = SURFACES_AT + index * SURFACE_STRIDE + SURFACE_STRIDE / 2;
    if nexus_user::memory_map(fresh, staging, true).is_err() {
        nexus_user::close(fresh).ok();
        return Ok(());
    }

    // What still fits, row by row. The two are packed to different widths, so
    // there is no copying them as one run.
    let keep_width = width.min(tile.width) as usize;
    let keep_height = height.min(tile.height) as usize;
    for row in 0..keep_height {
        let from = tile.mapped_at + row * tile.width as usize * 4;
        let to = staging + row * width as usize * 4;
        for column in 0..keep_width {
            // SAFETY: both surfaces are mapped and writable here, and the
            // offsets are inside the smaller of the two in each direction.
            unsafe {
                let pixel = core::ptr::read_volatile((from + column * 4) as *const u32);
                core::ptr::write_volatile((to + column * 4) as *mut u32, pixel);
            }
        }
    }

    // The client's copy, before the old one goes: a handle moves when it
    // crosses a channel, so this has to be a duplicate, and it needs transfer
    // because crossing is the one thing it is for.
    let Ok(theirs) = nexus_user::duplicate(
        fresh,
        nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
    ) else {
        nexus_user::memory_unmap(fresh, staging).ok();
        nexus_user::close(fresh).ok();
        return Ok(());
    };

    let mut message = [0u8; 12];
    message[0..4].copy_from_slice(b"size");
    message[4..8].copy_from_slice(&width.to_le_bytes());
    message[8..12].copy_from_slice(&height.to_le_bytes());
    if nexus_user::send(tile.channel, &message, &[theirs]).is_err() {
        // The client has gone. Its window goes with it, and the surface just
        // made goes back rather than being left mapped.
        nexus_user::memory_unmap(fresh, staging).ok();
        nexus_user::close(fresh).ok();
        return Err(());
    }

    // And out with the old. Unmapped from both places and closed, so the object
    // dies with the last handle to it -- which is this one, since the client
    // closes its own on the way past.
    nexus_user::memory_unmap(tile.surface, tile.mapped_at).ok();
    nexus_user::close(tile.surface).ok();

    // The new one moves to where the old one was, so that every other part of
    // this program goes on reading a surface from one place.
    nexus_user::memory_unmap(fresh, staging).ok();
    if nexus_user::memory_map(fresh, tile.mapped_at, true).is_err() {
        nexus_user::close(fresh).ok();
        return Err(());
    }

    tile.surface = fresh;
    tile.width = width;
    tile.height = height;
    Ok(())
}

/// Bring a window to the front of the order.
fn raise(order: &mut [usize; CLIENTS], index: usize) {
    let Some(at) = order.iter().position(|&which| which == index) else {
        return;
    };
    // Rotate rather than swap. Swapping with the last would put whatever was in
    // front behind everything, which is not raising one window -- it is
    // exchanging two.
    order[at..].rotate_left(1);
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

/// Draw the whole of the rectangle this program owns.
///
/// Clear it, then every window back to front, then the pointer on top. More
/// work than repainting what changed, and it is what makes overlap simply work:
/// a compositor that repainted only the damaged window would leave a hole in
/// whatever was above it, and getting that right needs damage arithmetic this
/// does not have and does not yet need.
fn repaint(
    screen: &Screen,
    tiles: &[Option<Tile>; CLIENTS],
    order: &[usize; CLIENTS],
    focus: usize,
    cursor: &mut Pointer,
    pointing: bool,
) {
    // The saved pixels are from before this repaint and mean nothing after it,
    // so the cursor is forgotten rather than restored.
    cursor.drawn = None;

    fill(
        screen,
        screen.x,
        screen.y,
        screen.width,
        screen.height,
        BACKGROUND,
    );

    for &index in order {
        let Some(tile) = tiles.get(index).and_then(Option::as_ref) else {
            continue;
        };
        if !tile.live {
            continue;
        }
        composite(screen, tile);
        title_bar(screen, tile, index == focus);
        grip(screen, tile, index == focus);
        outline(screen, tile, index == focus);
    }

    if pointing {
        draw_cursor(screen, cursor);
    }
}

/// Draw the corner that resizes a window.
///
/// Three short diagonals, which is what a grip has looked like for thirty
/// years. Visible for the same reason the title bar is: a corner that resizes
/// and does not say so is a corner nobody grabs, and one that is grabbed by
/// accident is worse.
fn grip(screen: &Screen, tile: &Tile, focused: bool) {
    let colour = if focused { 0x0088_B4E8 } else { 0x0038_4A60 };
    for line in 1..4u32 {
        let inset = line * 3;
        if inset + 1 >= tile.width || inset + 1 >= tile.height {
            break;
        }
        for step in 0..inset {
            let x = tile.x + tile.width - 1 - step;
            let y = tile.y + tile.height - 1 - (inset - 1 - step);
            fill(screen, x, y, 1, 1, colour);
        }
    }
}

/// Fill a rectangle with one colour.
fn fill(screen: &Screen, x: u32, y: u32, width: u32, height: u32, colour: u32) {
    for row in 0..height {
        let destination =
            FRAMEBUFFER_AT + (((y + row) as usize * screen.stride as usize) + x as usize) * 4;
        for column in 0..width as usize {
            // SAFETY: the framebuffer is mapped writable, and the rectangle was
            // checked against the screen when the display was taken.
            unsafe {
                core::ptr::write_volatile((destination + column * 4) as *mut u32, colour);
            }
        }
    }
}

/// Draw a window's title bar.
///
/// A decoration: over the client's own pixels, after its surface has been
/// copied out, so a client can neither draw one nor remove the one it has. It
/// is also what there is to take hold of -- a window with no bar is a window
/// that cannot be picked up without picking up whatever is inside it.
fn title_bar(screen: &Screen, tile: &Tile, focused: bool) {
    let colour = if focused { 0x0021_5C99 } else { 0x0018_2334 };
    fill(
        screen,
        tile.x,
        tile.y,
        tile.width,
        TITLE.min(tile.height),
        colour,
    );

    // Three lines at the left, so the bar reads as something to grab rather
    // than as a band of colour. There is no text: drawing it would need a font,
    // which is the kernel's and not this program's.
    let mark = if focused { 0x00CFE4FF } else { 0x004A5A70 };
    for line in 0..3u32 {
        let row = tile.y + 4 + line * 3;
        if row >= tile.y + TITLE.min(tile.height) {
            break;
        }
        fill(screen, tile.x + 6, row, (tile.width / 4).max(1), 1, mark);
    }
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

/// Draw the pointer, saving what it covers.
///
/// An arrow, of a sort: a triangle with a light edge, so that it shows over a
/// client's gradient and over the background alike. There is no second buffer
/// to composite from, so the pixels it covers have nowhere to be kept but here
/// -- and a repaint discards them, because pixels saved before a repaint mean
/// nothing after one.
fn draw_cursor(screen: &Screen, cursor: &mut Pointer) {
    for row in 0..CURSOR {
        for column in 0..CURSOR {
            // The arrow: filled where the column is inside the row, edged on
            // the diagonal, and nothing outside it.
            if column > row {
                continue;
            }
            let colour = if column == row || column == 0 || row == CURSOR - 1 {
                0x00F2_F6FF
            } else {
                0x0012_1A2A
            };

            let offset =
                (((cursor.y as usize + row) * screen.stride as usize) + cursor.x as usize + column)
                    * 4;
            // SAFETY: the framebuffer is mapped writable and this address is
            // inside the rectangle this program owns, which the cursor is
            // clamped to.
            unsafe {
                let under = core::ptr::read_volatile((FRAMEBUFFER_AT + offset) as *const u32);
                cursor.beneath[row * CURSOR + column] = under;
                core::ptr::write_volatile((FRAMEBUFFER_AT + offset) as *mut u32, colour);
            }
        }
    }
    cursor.drawn = Some((cursor.x, cursor.y));
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
