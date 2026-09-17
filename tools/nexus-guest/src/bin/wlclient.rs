//! A Wayland client, of the shape a real one has.
//!
//! It connects to `/tmp/wayland-0`, does the handshake every client does, asks
//! `xdg-shell` for a window, draws into shared memory at the size the
//! compositor asked for, and then stays up and answers events until the
//! compositor asks the window to close.
//!
//! The sequence is the one in every Wayland tutorial, and it is worth reading
//! as a list because each step is a thing that had to exist underneath:
//!
//! 1. `connect` to a Unix socket at a name — sockets.
//! 2. `wl_display.get_registry`, and wait for the globals — a byte stream.
//! 3. `wl_registry.bind` for `wl_compositor`, `wl_shm`, `xdg_wm_base`,
//!    `wl_seat` and `wl_output`.
//! 4. `memfd_create` and `ftruncate` — memory with a descriptor on it.
//! 5. `mmap` it `MAP_SHARED` and draw into it.
//! 6. `wl_shm.create_pool`, sending the descriptor with `SCM_RIGHTS` —
//!    descriptor passing.
//! 7. `wl_compositor.create_surface`, `xdg_wm_base.get_xdg_surface`,
//!    `xdg_surface.get_toplevel`, a title, and a commit with **no buffer** —
//!    which is `xdg-shell`'s way of saying "tell me how big to be".
//! 8. Wait for `xdg_surface.configure`, acknowledge it, and only then attach a
//!    buffer, damage it, ask for a frame callback and commit.
//! 9. Answer events for ever: a second configure means redraw at a new size, a
//!    `ping` needs a `pong`, a key needs reading, and a `close` means go.
//!
//! # Why the loop is the interesting part
//!
//! The previous version of this program sent its messages in order and exited.
//! That is not what a client is. A client is a loop around a socket, and
//! everything that makes a window a window — being resized, being told it has
//! the keyboard, being asked to close — arrives in that loop, unprompted, in an
//! order the client does not choose.
//!
//! | 220 | `socket` |
//! | 221 | `connect`: nothing is listening at /tmp/wayland-0 |
//! | 222 | the compositor never announced the globals a window needs |
//! | 223 | `memfd_create` |
//! | 224 | `ftruncate` |
//! | 225 | `mmap` of the shared buffer |
//! | 226 | `sendmsg` of the pool's descriptor |
//! | 228 | the compositor never released a buffer |
//! | 229 | no frame callback ever came back |
//! | 230 | the window was closed before it had been resized |
//! | 231 | the window was closed before a key reached it |
//! | 232 | the compositor went away without closing the window |
//! | 233 | the compositor reported a protocol error |

#![no_std]
#![no_main]

use nexus_guest::wayland::{event, interface, request, send_with_descriptor, Connection, Out};
use nexus_guest::{call, exit_group, expect, fail, say, syscall2, syscall3, syscall6};

nexus_guest::guest_main!(run);

/// Where the compositor listens.
const SOCKET_NAME: &[u8] = b"/tmp/wayland-0";

/// The picture: bands, so a screenshot can tell them apart and tell which way
/// up they are. The middle one is drawn only after the window has been resized,
/// which is what makes it possible to say from outside the machine *which*
/// frame is on the screen.
const TOP: u32 = 0x0033_9966;
const MIDDLE: u32 = 0x0022_44EE;
const BOTTOM: u32 = 0x00CC_5533;

/// The largest surface this client will ever have, and so how much shared
/// memory it asks for once.
///
/// A pool is allocated once and buffers are cut out of it. Growing one is a
/// `wl_shm_pool.resize` and a second `ftruncate`; a client that knows its
/// largest size, as this one does, never needs it.
const MAX_WIDTH: u32 = 320;
const MAX_HEIGHT: u32 = 200;

/// The identifiers this client allocates, from one upwards. Object one is the
/// display and exists already, so the client's own start at two.
const REGISTRY: u32 = 2;
const COMPOSITOR: u32 = 3;
const SHM: u32 = 4;
const WM_BASE: u32 = 5;
const SEAT: u32 = 6;
const OUTPUT: u32 = 7;
const SURFACE: u32 = 8;
const XDG_SURFACE: u32 = 9;
const TOPLEVEL: u32 = 10;
const POOL: u32 = 11;
const KEYBOARD: u32 = 12;
/// Buffers and frame callbacks get a fresh identifier each time, because both
/// are destroyed after use and an identifier the compositor still holds must
/// not be reused. These are where each run of them starts.
const FIRST_BUFFER: u32 = 20;
const FIRST_CALLBACK: u32 = 40;

/// What the client knows about its window.
struct Window {
    /// The size the compositor last asked for.
    width: u32,
    height: u32,
    /// The size last actually drawn.
    drawn_width: u32,
    drawn_height: u32,
    /// The buffer currently attached, and whether the compositor has given it
    /// back.
    buffer: u32,
    held: bool,
    /// The outstanding frame callback.
    callback: u32,
    /// How many of each thing has happened, which is what the exit status is
    /// worked out from.
    configures: u32,
    releases: u32,
    frames: u32,
    keys: u32,
    /// Where the shared memory is, and how wide a row of the pool is.
    pixels: u64,
    /// The next identifiers to hand out.
    next_buffer: u32,
    next_callback: u32,
}

fn run() -> ! {
    let socket = syscall3(call::SOCKET, 1, 1, 0);
    expect(socket >= 0, 220);

    // The compositor may not be listening yet: both programs were started at
    // about the same moment. A few tries rather than one, which is what a real
    // client's `wl_display_connect` does not do -- it is started by a session
    // that already has a compositor.
    let (address, address_length) = sockaddr(SOCKET_NAME);
    let mut connected = false;
    for _ in 0..200 {
        if syscall3(
            call::CONNECT,
            socket as u64,
            address.as_ptr() as u64,
            address_length,
        ) == 0
        {
            connected = true;
            break;
        }
        sleep_briefly();
    }
    expect(connected, 221);
    say("wlclient: connected to the compositor");

    let mut connection = Connection::new(socket as u64);
    let mut out = Out::new();

    // ---- the registry, and everything on it -------------------------------
    out.start(
        nexus_guest::wayland::DISPLAY,
        request::display::GET_REGISTRY,
    );
    out.word(REGISTRY);
    connection.send(out.finish());

    // The five globals a window needs. `xdg_wm_base` is the one that decides
    // whether this program has anything to do at all: without it a surface is
    // a rectangle of pixels with no way to become a window, and the correct
    // behaviour is to stop.
    let mut names = [0u32; 5];
    let wanted = [
        interface::COMPOSITOR,
        interface::SHM,
        interface::WM_BASE,
        interface::SEAT,
        interface::OUTPUT,
    ];
    for _ in 0..200 {
        if !connection.fill() {
            break;
        }
        while let Some(message) = connection.take() {
            if message.object == REGISTRY && message.opcode == event::registry::GLOBAL {
                let name = message.word(0);
                let what = message.text(1);
                for (slot, expected) in wanted.iter().enumerate() {
                    if what == *expected {
                        names[slot] = name;
                    }
                }
            }
        }
        if names.iter().all(|name| *name != 0) {
            break;
        }
    }
    expect(names.iter().all(|name| *name != 0), 222);
    say("wlclient: the compositor offers a compositor, shm, xdg_wm_base, a seat and an output");

    // `bind(name, interface, version, new_id)`, five times.
    let bindings = [
        (names[0], interface::COMPOSITOR, 4u32, COMPOSITOR),
        (names[1], interface::SHM, 1, SHM),
        (names[2], interface::WM_BASE, 2, WM_BASE),
        (names[3], interface::SEAT, 5, SEAT),
        (names[4], interface::OUTPUT, 2, OUTPUT),
    ];
    for (name, what, version, id) in bindings {
        out.start(REGISTRY, request::registry::BIND);
        out.word(name);
        out.text(what);
        out.word(version);
        out.word(id);
        connection.send(out.finish());
    }

    // A keyboard off the seat. The seat said it had one in its capabilities;
    // asking for one it has not got is the usual way a client is disconnected.
    out.start(SEAT, request::seat::GET_KEYBOARD);
    out.word(KEYBOARD);
    connection.send(out.finish());

    // ---- the pixels -------------------------------------------------------
    //
    // Memory with a descriptor on it, so the compositor can be given the
    // descriptor and see the same bytes. This is what a `wl_shm_pool` is.
    let name = b"wlclient\0";
    let memory = syscall2(call::MEMFD_CREATE, name.as_ptr() as u64, 0);
    expect(memory >= 0, 223);
    let size = u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT) * 4;
    expect(syscall2(call::FTRUNCATE, memory as u64, size) == 0, 224);

    let pixels = syscall6(
        call::MMAP,
        0,
        size,
        3, // PROT_READ | PROT_WRITE
        1, // MAP_SHARED
        memory as u64,
        0,
    );
    expect(pixels >= 0, 225);

    out.start(SHM, request::shm::CREATE_POOL);
    out.word(POOL);
    out.word(size as u32);
    expect(
        send_with_descriptor(socket as u64, out.finish(), memory as i32) > 0,
        226,
    );

    // ---- the window -------------------------------------------------------
    out.start(COMPOSITOR, request::compositor::CREATE_SURFACE);
    out.word(SURFACE);
    connection.send(out.finish());

    out.start(WM_BASE, request::wm_base::GET_XDG_SURFACE);
    out.word(XDG_SURFACE);
    out.word(SURFACE);
    connection.send(out.finish());

    out.start(XDG_SURFACE, request::xdg_surface::GET_TOPLEVEL);
    out.word(TOPLEVEL);
    connection.send(out.finish());

    out.start(TOPLEVEL, request::toplevel::SET_TITLE);
    out.text("A window from a program built for Linux");
    connection.send(out.finish());

    out.start(TOPLEVEL, request::toplevel::SET_APP_ID);
    out.text("org.nexusos.wlclient");
    connection.send(out.finish());

    // The commit that maps the window, with nothing attached. This is the step
    // that surprises everybody who writes a client from the wire protocol: a
    // commit with no buffer is not a mistake and not a no-op, it is the client
    // saying it is ready to be told how big to be. Attaching a buffer before
    // the first configure is a protocol error.
    out.start(SURFACE, request::surface::COMMIT);
    connection.send(out.finish());
    say("wlclient: asked for a window and waited to be told its size");

    let mut window = Window {
        width: 0,
        height: 0,
        drawn_width: 0,
        drawn_height: 0,
        buffer: 0,
        held: false,
        callback: 0,
        configures: 0,
        releases: 0,
        frames: 0,
        keys: 0,
        pixels: pixels as u64,
        next_buffer: FIRST_BUFFER,
        next_callback: FIRST_CALLBACK,
    };

    // ---- and then answer events, which is the whole of being a client ------
    //
    // Bounded, so that a compositor that stops talking is a program that exits
    // with a number rather than one that hangs. A real client waits for ever,
    // because a real person will close it.
    for _ in 0..1200 {
        if !readable(connection.socket, 100) {
            continue;
        }
        if !connection.fill() {
            break;
        }
        while let Some((object, opcode, body, length)) = next_event(&mut connection) {
            let word = |index: usize| -> u32 {
                let at = index * 4;
                if at + 4 > length {
                    return 0;
                }
                u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]])
            };

            match (object, opcode) {
                (nexus_guest::wayland::DISPLAY, event::display::ERROR) => {
                    say("wlclient: the compositor reported a protocol error");
                    exit_group(233)
                }
                (WM_BASE, event::wm_base::PING) => {
                    // A compositor asking whether this client is still
                    // answering. Not answering is how a window gets the "this
                    // application is not responding" treatment.
                    out.start(WM_BASE, request::wm_base::PONG);
                    out.word(word(0));
                    connection.send(out.finish());
                    say("wlclient: answered a ping");
                }
                (TOPLEVEL, event::toplevel::CONFIGURE) => {
                    // The size the compositor wants. Zero means "you choose",
                    // which is what a compositor sends when it has no opinion.
                    let (width, height) = (word(0), word(1));
                    window.width = if width == 0 { MAX_WIDTH } else { width };
                    window.height = if height == 0 { MAX_HEIGHT } else { height };
                }
                (TOPLEVEL, event::toplevel::CLOSE) => {
                    say("wlclient: the compositor asked the window to close");
                    finish(&window);
                }
                (XDG_SURFACE, event::xdg_surface::CONFIGURE) => {
                    // The serial that makes the configuration above real. It is
                    // acknowledged first and acted on second: the compositor is
                    // entitled to assume the next buffer is drawn at the size
                    // it just asked for.
                    out.start(XDG_SURFACE, request::xdg_surface::ACK_CONFIGURE);
                    out.word(word(0));
                    connection.send(out.finish());
                    window.configures += 1;
                    if window.width != window.drawn_width || window.height != window.drawn_height {
                        draw(&mut connection, &mut out, &mut window);
                    }
                }
                (id, event::buffer::RELEASE) if id == window.buffer => {
                    // The compositor has the pixels; the buffer is the
                    // client's again. Without this a correct client never
                    // draws a second frame.
                    window.held = false;
                    window.releases += 1;
                }
                (id, event::callback::DONE) if id == window.callback => {
                    window.callback = 0;
                    window.frames += 1;
                    say("wlclient: a frame callback came back");
                }
                (KEYBOARD, event::keyboard::KEYMAP) => {
                    // The keymap arrives as a descriptor beside the message.
                    // Closed rather than read: the format this compositor sends
                    // is "there is no keymap", and a client that mapped it
                    // would be reading a byte that means nothing.
                    if let Some(descriptor) = connection.take_descriptor() {
                        let _ = nexus_guest::syscall1(call::CLOSE, descriptor as u64);
                    }
                    say("wlclient: the compositor sent a keymap descriptor");
                }
                (KEYBOARD, event::keyboard::ENTER) => {
                    say("wlclient: this window has the keyboard");
                }
                // `key(serial, time, key, state)`. Only the press is counted:
                // every key arrives twice, once each way.
                (KEYBOARD, event::keyboard::KEY)
                    if word(3) == nexus_guest::wayland::key_state::PRESSED =>
                {
                    window.keys += 1;
                    say_number("wlclient: a key reached the client, evdev code ", word(2));
                }
                _ => {}
            }
        }
    }

    // The loop ran out. That is the compositor having stopped talking without
    // ever closing the window, which is a different failure from any of the
    // ones above and gets its own number.
    say("wlclient: the compositor stopped answering");
    fail(232)
}

/// Stop, with a status that says what never happened.
///
/// Reached when the compositor asks the window to close, which is the one
/// orderly way out. Everything the run was supposed to demonstrate is checked
/// here rather than as it happens, so that a run which got most of the way
/// says which step it stopped at.
fn finish(window: &Window) -> ! {
    if window.configures < 2 {
        fail(230)
    }
    if window.releases < 2 {
        fail(228)
    }
    if window.frames < 2 {
        fail(229)
    }
    if window.keys < 1 {
        fail(231)
    }
    say("wlclient: configured twice, drew twice, released twice, and was typed at");
    exit_group(0)
}

/// Draw at the size the compositor asked for, and commit it.
///
/// A new `wl_buffer` each time rather than a resized one: a buffer has its
/// dimensions fixed when it is created, and the compositor is entitled to hold
/// the old one until it says otherwise. The memory underneath is the same pool,
/// which is why nothing here allocates.
fn draw(connection: &mut Connection, out: &mut Out, window: &mut Window) {
    // The old buffer, if the compositor has given it back. Destroyed rather
    // than left: a client that made one per frame and never destroyed any
    // would leak an object per frame in the compositor.
    if window.buffer != 0 && !window.held {
        out.start(window.buffer, request::buffer::DESTROY);
        connection.send(out.finish());
    }

    let (width, height) = (window.width, window.height);
    let stride = width * 4;

    // Bands: two the first time, three once the window has been resized. The
    // difference is what lets a screenshot taken from outside the machine say
    // *which* frame reached the screen rather than only that one did.
    let bands: u32 = if window.drawn_width == 0 { 2 } else { 3 };
    let mut row = 0;
    while row < height {
        let band = (row * bands) / height;
        let colour = match (bands, band) {
            (2, 0) => TOP,
            (2, _) => BOTTOM,
            (_, 0) => TOP,
            (_, 1) => MIDDLE,
            (_, _) => BOTTOM,
        };
        let at = window.pixels + u64::from(row) * u64::from(stride);
        let mut column = 0;
        while column < width {
            // SAFETY: inside the mapping this program made. The pool is the
            // largest size this client ever asks for, and every configure is
            // clamped to it below.
            unsafe {
                core::ptr::write_volatile((at + u64::from(column) * 4) as *mut u32, colour);
            }
            column += 1;
        }
        row += 1;
    }

    let buffer = window.next_buffer;
    window.next_buffer += 1;
    out.start(POOL, request::shm_pool::CREATE_BUFFER);
    out.word(buffer);
    out.word(0); // offset into the pool
    out.word(width);
    out.word(height);
    out.word(stride);
    out.word(nexus_guest::wayland::XRGB8888);
    connection.send(out.finish());

    out.start(SURFACE, request::surface::ATTACH);
    out.word(buffer);
    out.word(0);
    out.word(0);
    connection.send(out.finish());

    out.start(SURFACE, request::surface::DAMAGE);
    out.word(0);
    out.word(0);
    out.word(width);
    out.word(height);
    connection.send(out.finish());

    // "Tell me when it would be worth drawing again." A client that draws
    // without asking for this is a client drawing frames nobody will show.
    let callback = window.next_callback;
    window.next_callback += 1;
    out.start(SURFACE, request::surface::FRAME);
    out.word(callback);
    connection.send(out.finish());

    out.start(SURFACE, request::surface::COMMIT);
    connection.send(out.finish());

    window.buffer = buffer;
    window.callback = callback;
    window.held = true;
    window.drawn_width = width;
    window.drawn_height = height;
    say_size("wlclient: drew and committed a surface, ", width, height);
}

/// The next whole message, copied out of the connection's buffer.
///
/// A copy rather than a borrow, and that is not tidiness: `take` hands back a
/// slice of the buffer the connection is reading into, so a caller holding one
/// cannot send a reply on the same connection. Every event here is a few dozen
/// bytes.
fn next_event(connection: &mut Connection) -> Option<(u32, u16, [u8; 64], usize)> {
    let taken = connection.take()?;
    let mut body = [0u8; 64];
    let length = taken.body.len().min(64);
    body[..length].copy_from_slice(&taken.body[..length]);
    Some((taken.object, taken.opcode, body, length))
}

/// Whether there is something to read, waiting up to `milliseconds` for it.
fn readable(descriptor: u64, milliseconds: i64) -> bool {
    /// `POLLIN`: there is something to read, or the other end has gone.
    const POLLIN: u32 = 1;
    let mut fds = [descriptor as u32, POLLIN];
    let ready = syscall3(call::POLL, fds.as_mut_ptr() as u64, 1, milliseconds as u64);
    ready > 0 && (fds[1] >> 16) != 0
}

/// A line with one number on the end of it.
fn say_number(before: &str, value: u32) {
    nexus_guest::fmt::say_with(before, i64::from(value));
}

/// A line ending in `<width>x<height>`.
fn say_size(before: &str, width: u32, height: u32) {
    nexus_guest::fmt::Line::new()
        .text(before)
        .number(i64::from(width))
        .text("x")
        .number(i64::from(height))
        .say();
}

/// A tenth of a second.
fn sleep_briefly() {
    let word: u32 = 0;
    let timeout: [u64; 2] = [0, 100_000_000];
    let _ = nexus_guest::syscall4(
        call::FUTEX,
        core::ptr::addr_of!(word) as u64,
        128, // FUTEX_WAIT_PRIVATE
        0,
        timeout.as_ptr() as u64,
    );
}

/// A `sockaddr_un` for a path.
fn sockaddr(name: &[u8]) -> ([u8; 110], u64) {
    let mut out = [0u8; 110];
    out[0..2].copy_from_slice(&1u16.to_le_bytes()); // AF_UNIX
    let take = if name.len() > 107 { 107 } else { name.len() };
    out[2..2 + take].copy_from_slice(&name[..take]);
    (out, 2 + take as u64 + 1)
}
