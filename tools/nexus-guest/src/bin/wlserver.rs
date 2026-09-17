//! A Wayland compositor, in the sense that matters: it speaks the protocol.
//!
//! It binds a Wayland socket, accepts one client, answers the handshake,
//! negotiates a window through `xdg-shell`, takes a shared-memory buffer the
//! client hands it over `SCM_RIGHTS`, copies the pixels out of it onto the
//! screen through `/dev/nexus/display`, and sends back the things a client
//! waits for: a configure, a buffer release, a frame callback and a key.
//!
//! # What this is and is not
//!
//! It is a real implementation of the wire protocol: the messages on the socket
//! are the ones in `wayland.xml` and `xdg-shell.xml`, with the sizes and
//! opcodes and padding a `libwayland` client would send and expect. A client
//! that spoke the protocol correctly would be served correctly.
//!
//! It implements `wl_display`, `wl_registry`, `wl_compositor`, `wl_shm`,
//! `wl_shm_pool`, `wl_surface`, `wl_buffer`, `wl_callback`, `wl_output`,
//! `wl_seat`, `wl_keyboard`, `xdg_wm_base`, `xdg_surface` and `xdg_toplevel`.
//! That is the set a client needs before it will put anything on a screen. The
//! previous version of this program stopped at `wl_surface`, and a real client
//! against it would have connected, looked for `xdg_wm_base`, not found it, and
//! stopped -- which is the correct thing for it to do.
//!
//! It is still **not a Wayland compositor**. One client, one toplevel, no
//! subsurfaces, no popups, no regions, no pointer, no touch, no output scaling,
//! no XKB keymap, no `linux-dmabuf`, and no protocol errors: a client that
//! misused an object is ignored where a compositor would disconnect it. Window
//! management is a script rather than a policy -- this program decides by
//! itself to resize the window once and then close it, because there is nobody
//! here to drag an edge.
//!
//! It has also never been tested against `libwayland`, because there is no
//! `libwayland` on this machine and no toolchain here that could build one. It
//! has been tested against the client next door, which was written from the
//! same protocol description. That is a weaker claim than "Wayland works" and
//! it is the one being made.
//!
//! | 200 | `/dev/nexus/display` |
//! | 201 | the window's size |
//! | 202 | mapping the window |
//! | 203 | `socket` |
//! | 204 | `bind` |
//! | 205 | `listen` |
//! | 206 | `accept` |
//! | 208 | the client's buffer arrived with no descriptor |
//! | 209 | mapping the client's buffer |
//! | 210 | presenting |
//! | 211 | `memfd_create` for the keymap |
//! | 212 | `ftruncate` of the keymap |

#![no_std]
#![no_main]

use nexus_guest::wayland::{
    capability, event, interface, key_state, keymap, mode, request, send_with_descriptor,
    Connection, Out,
};
use nexus_guest::{call, expect, fail, say, syscall1, syscall2, syscall3, syscall4, syscall6};

nexus_guest::guest_main!(run);

/// Where a Wayland compositor listens.
///
/// The name every client looks for: `$XDG_RUNTIME_DIR/wayland-0`. Written here
/// as an absolute path because this machine has no `XDG_RUNTIME_DIR` and a
/// client that had to read one would need an environment this does not set.
const SOCKET_NAME: &[u8] = b"/tmp/wayland-0";

/// The device this compositor draws through.
const DISPLAY_DEVICE: &[u8] = b"/dev/nexus/display\0";
const DISPLAY_INFO: u32 = 0x4E58_0001;
const DISPLAY_PRESENT: u32 = 0x4E58_0002;
const DISPLAY_EVENT: u32 = 0x4E58_0003;

/// What `NEXUS_DISPLAY_EVENT` reports. Only the first is acted on here.
const EVENT_CHARACTER: u32 = 1;

/// The identifiers this server hands out for its globals.
///
/// A registry's `name` is not an object identifier -- it is a number the server
/// picks to refer to a global, which the client passes back to `bind`. These
/// are the ones this server uses.
const GLOBAL_COMPOSITOR: u32 = 1;
const GLOBAL_SHM: u32 = 2;
const GLOBAL_OUTPUT: u32 = 3;
const GLOBAL_SEAT: u32 = 4;
const GLOBAL_WM_BASE: u32 = 5;

/// The size this compositor gives a window to start with, and the size it then
/// asks for instead.
///
/// Two different sizes because a configure that asked for the size the client
/// already had would prove nothing. What is being shown is that the client
/// draws at the size the *compositor* chose, which is the whole of what
/// `xdg_toplevel.configure` is for.
const FIRST_WIDTH: u32 = 320;
const FIRST_HEIGHT: u32 = 200;
const SECOND_WIDTH: u32 = 240;
const SECOND_HEIGHT: u32 = 150;

/// What is behind the client's window.
const BACKGROUND: u32 = 0x0020_2838;

/// How many objects the server will track for one client.
const MAX_OBJECTS: usize = 64;

/// What kind of thing an object identifier refers to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    Registry,
    Compositor,
    Shm,
    ShmPool,
    Surface,
    Buffer,
    Output,
    Seat,
    Keyboard,
    WmBase,
    XdgSurface,
    Toplevel,
}

/// One object the client has made.
#[derive(Clone, Copy)]
struct Object {
    id: u32,
    kind: Kind,
    /// For a pool: where it is mapped. For a buffer: where its pixels start.
    at: u64,
    size: u64,
    width: u32,
    height: u32,
    stride: u32,
}

impl Object {
    const fn empty() -> Self {
        Self {
            id: 0,
            kind: Kind::None,
            at: 0,
            size: 0,
            width: 0,
            height: 0,
            stride: 0,
        }
    }
}

/// The window this compositor draws into.
struct Screen {
    descriptor: u64,
    pixels: u64,
    width: u32,
    height: u32,
    stride: u32,
}

/// A rectangle of a surface that changed.
#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    const fn nothing() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    fn is_nothing(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The smallest rectangle holding both.
    ///
    /// A client may damage several rectangles before one commit, and what a
    /// compositor owes it is that every damaged pixel is redrawn -- not that
    /// nothing else is. The union of two boxes is the cheapest way to keep that
    /// promise, and is what most compositors do until the arithmetic is worth
    /// more than the pixels.
    fn union(self, other: Rect) -> Rect {
        if self.is_nothing() {
            return other;
        }
        if other.is_nothing() {
            return self;
        }
        let x = if self.x < other.x { self.x } else { other.x };
        let y = if self.y < other.y { self.y } else { other.y };
        let right_a = self.x.saturating_add(self.width);
        let right_b = other.x.saturating_add(other.width);
        let bottom_a = self.y.saturating_add(self.height);
        let bottom_b = other.y.saturating_add(other.height);
        let right = if right_a > right_b { right_a } else { right_b };
        let bottom = if bottom_a > bottom_b {
            bottom_a
        } else {
            bottom_b
        };
        Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

/// The one window this compositor manages, and where the conversation about it
/// has got to.
///
/// One rather than a list, and that is a limitation rather than a design: a
/// real compositor keeps this per `xdg_toplevel`. With one client and one
/// toplevel the two are the same thing, and pretending otherwise would be a
/// table with one row in it.
struct Shell {
    wm_base: u32,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    keyboard: u32,
    /// The buffer attached but not yet committed. A client may attach and then
    /// change its mind, so nothing is drawn until `commit`.
    attached: u32,
    /// The callback to fire after the next frame reaches the screen.
    frame: u32,
    /// Everything the client has said changed since its last commit.
    damage: Rect,
    /// The last serial sent in a configure, and the last one acknowledged.
    serial: u32,
    acked: u32,
    /// Whether the keyboard has been told it is on this surface.
    entered: bool,
    /// The size last asked for, and the size last actually drawn.
    asked_width: u32,
    asked_height: u32,
    drawn_width: u32,
    drawn_height: u32,
    /// Frames put on the screen, and keys sent to the client.
    frames: u32,
    keys: u32,
    /// Where the script in [`advance`] has got to.
    stage: u32,
    /// The serial of the outstanding ping, and whether it came back.
    ping: u32,
    ponged: bool,
}

impl Shell {
    const fn new() -> Self {
        Self {
            wm_base: 0,
            surface: 0,
            xdg_surface: 0,
            toplevel: 0,
            keyboard: 0,
            attached: 0,
            frame: 0,
            damage: Rect::nothing(),
            serial: 0,
            acked: 0,
            entered: false,
            asked_width: FIRST_WIDTH,
            asked_height: FIRST_HEIGHT,
            drawn_width: 0,
            drawn_height: 0,
            frames: 0,
            keys: 0,
            stage: 0,
            ping: 0,
            ponged: false,
        }
    }

    /// The next serial. Serials are per-connection and only have to increase:
    /// a client matches an `ack_configure` to a configure by this number.
    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }
}

fn run() -> ! {
    // ---- the screen -------------------------------------------------------
    let display = syscall4(
        call::OPENAT,
        (-100i64) as u64,
        DISPLAY_DEVICE.as_ptr() as u64,
        2, // O_RDWR
        0,
    );
    expect(display >= 0, 200);

    let mut info = [0u32; 4];
    expect(
        syscall3(
            call::IOCTL,
            display as u64,
            u64::from(DISPLAY_INFO),
            info.as_mut_ptr() as u64,
        ) == 0,
        201,
    );
    let (width, height, stride) = (info[0], info[1], info[2]);
    expect(width > 0 && height > 0, 201);

    let pixels = syscall6(
        call::MMAP,
        0,
        u64::from(stride) * u64::from(height),
        3, // PROT_READ | PROT_WRITE
        1, // MAP_SHARED
        display as u64,
        0,
    );
    expect(pixels >= 0, 202);
    let screen = Screen {
        descriptor: display as u64,
        pixels: pixels as u64,
        width,
        height,
        stride,
    };

    // Something recognisable before any client connects, so that a screenshot
    // taken too early says "the compositor is up and nobody has drawn" rather
    // than showing whatever was in the buffer.
    fill(&screen, BACKGROUND);
    present(&screen, whole(&screen));

    // ---- the socket -------------------------------------------------------
    let listening = syscall3(call::SOCKET, 1, 1, 0);
    expect(listening >= 0, 203);
    let (address, address_length) = sockaddr(SOCKET_NAME);
    expect(
        syscall3(
            call::BIND,
            listening as u64,
            address.as_ptr() as u64,
            address_length,
        ) == 0,
        204,
    );
    expect(syscall2(call::LISTEN, listening as u64, 4) == 0, 205);
    say("wlserver: listening on /tmp/wayland-0");

    let client = syscall4(call::ACCEPT, listening as u64, 0, 0, 0);
    expect(client >= 0, 206);
    say("wlserver: a client connected");

    serve(Connection::new(client as u64), &screen)
}

/// One request, copied out of the connection's buffer.
///
/// A copy rather than a borrow, and that is not tidiness: `Connection::take`
/// hands back a slice of the buffer it is reading into, so a handler holding
/// one cannot send a reply on the same connection. Every request here is at
/// most a few dozen bytes.
struct Request {
    object: u32,
    opcode: u16,
    body: [u8; 128],
    length: usize,
}

impl Request {
    /// The `index`th word of the body.
    fn word(&self, index: usize) -> u32 {
        let at = index * 4;
        if at + 4 > self.length {
            return 0;
        }
        u32::from_le_bytes([
            self.body[at],
            self.body[at + 1],
            self.body[at + 2],
            self.body[at + 3],
        ])
    }

    /// The string starting at word `index`, and how many words it occupies
    /// including its length.
    ///
    /// The second number is the part that matters. A string on the wire is a
    /// length and then that many bytes *padded to four*, so everything after it
    /// is at an offset that depends on how long it was -- which is why `bind`'s
    /// version and new identifier cannot be read from a fixed position. Reading
    /// them from one was a bug that bound the wrong interface and then bound
    /// nothing at all.
    fn text(&self, index: usize) -> (&str, usize) {
        let at = index * 4;
        let length = self.word(index) as usize;
        if length == 0 || at + 4 + length > self.length {
            return ("", 1);
        }
        let words = 1 + length.div_ceil(4);
        // The length includes the terminating NUL, which is not part of it.
        let text = core::str::from_utf8(&self.body[at + 4..at + 4 + length - 1]).unwrap_or("");
        (text, words)
    }
}

/// Answer one client, forward what is typed at it, and run the window through
/// the states a compositor would put it through.
fn serve(mut connection: Connection, screen: &Screen) -> ! {
    let mut objects = [Object::empty(); MAX_OBJECTS];
    let mut shell = Shell::new();

    loop {
        // A wait with a deadline rather than a blocking read, because the
        // socket is not the only thing this program listens to: the keyboard
        // arrives through `/dev/nexus/display`, and a compositor blocked in
        // `recvmsg` would forward a key only when the client happened to say
        // something first.
        if readable(connection.socket, 50) {
            if !connection.fill() {
                // The client has gone. The window stays: a compositor whose
                // last client left does not take the screen down with it.
                break;
            }
            loop {
                let request = {
                    let Some(message) = connection.take() else {
                        break;
                    };
                    let mut body = [0u8; 128];
                    let length = if message.body.len() > 128 {
                        128
                    } else {
                        message.body.len()
                    };
                    body[..length].copy_from_slice(&message.body[..length]);
                    Request {
                        object: message.object,
                        opcode: message.opcode,
                        body,
                        length,
                    }
                };
                dispatch(&mut connection, &mut objects, screen, &mut shell, &request);
            }
        }

        forward_keys(&mut connection, screen, &mut shell);
        advance(&mut connection, &mut shell);
    }

    // Whatever the client last showed stays on the screen. A long, paced loop
    // rather than an exit, so a screenshot taken afterwards finds a window.
    loop {
        present(screen, whole(screen));
        sleep_briefly();
    }
}

/// Take the window through the states a person would otherwise put it through.
///
/// A compositor does this in response to somebody dragging an edge or clicking
/// a close button. There is nobody here, so the sequence is written down: show
/// the first frame, ask whether the client is still answering, ask for a
/// different size, prove it drew at that size, send it a key, and ask the
/// window to close. Each step waits for the client to have finished the
/// previous one, which is what makes it a test of the protocol rather than of
/// the timing.
fn advance(connection: &mut Connection, shell: &mut Shell) {
    match shell.stage {
        // The first frame is on the screen. A ping goes with it, which is how a
        // compositor finds out whether a client is still answering rather than
        // merely still connected.
        0 if shell.frames >= 1 => {
            say("wlserver: the client's first frame is on the screen");
            let serial = shell.next_serial();
            shell.ping = serial;
            let mut out = Out::new();
            out.start(shell.wm_base, event::wm_base::PING);
            out.word(serial);
            connection.send(out.finish());
            shell.stage = 1;
        }
        // Ask for a different size.
        1 if shell.ponged => {
            say("wlserver: the client answered a ping");
            shell.asked_width = SECOND_WIDTH;
            shell.asked_height = SECOND_HEIGHT;
            configure(connection, shell);
            say("wlserver: asked the window to be 240x150");
            shell.stage = 2;
        }
        // Wait for it to draw at that size.
        2 if shell.drawn_width == SECOND_WIDTH && shell.drawn_height == SECOND_HEIGHT => {
            say("wlserver: the client redrew at the size it was given");
            shell.stage = 3;
        }
        // Wait for somebody to type. The key itself is forwarded elsewhere;
        // this only notices that one went.
        3 if shell.keys >= 1 => {
            shell.stage = 4;
        }
        // And ask the window to close, which is a request and not an order: a
        // client is free to ignore it, and one with unsaved work should.
        4 => {
            let mut out = Out::new();
            out.start(shell.toplevel, event::toplevel::CLOSE);
            connection.send(out.finish());
            say("wlserver: asked the window to close");
            shell.stage = 5;
        }
        _ => {}
    }
}

/// Send a configure pair: the size, then the serial that acknowledges it.
///
/// Two messages, and the order matters. `xdg_toplevel.configure` carries the
/// size and the states; `xdg_surface.configure` carries the serial and means
/// "that is the whole of this configuration". A client applies nothing until
/// the second arrives, which is what lets a compositor change several things
/// at once without the client drawing a frame half way between.
fn configure(connection: &mut Connection, shell: &mut Shell) {
    let mut out = Out::new();
    out.start(shell.toplevel, event::toplevel::CONFIGURE);
    out.word(shell.asked_width);
    out.word(shell.asked_height);
    // No states: not maximised, not fullscreen, not resizing, not activated.
    // Still four bytes on the wire -- an empty array is a length of zero, not
    // an absence.
    out.array(&[]);
    connection.send(out.finish());

    let serial = shell.next_serial();
    out.start(shell.xdg_surface, event::xdg_surface::CONFIGURE);
    out.word(serial);
    connection.send(out.finish());
}

/// Read whatever was typed at this window and send it on as a key.
///
/// The compositor above hands this program *characters*, because that is what
/// its own keyboard protocol carries. Wayland carries evdev key codes, which
/// are positions on a keyboard rather than letters -- so this maps back, which
/// is a thing a real compositor never has to do because it sits on the evdev
/// codes to begin with. The mapping is small and assumes an English layout,
/// and that is written down here rather than hidden: it is the one place in
/// this program that guesses.
fn forward_keys(connection: &mut Connection, screen: &Screen, shell: &mut Shell) {
    if shell.keyboard == 0 || !shell.entered {
        return;
    }
    let mut event = [0u32; 4];
    if syscall3(
        call::IOCTL,
        screen.descriptor,
        u64::from(DISPLAY_EVENT),
        event.as_mut_ptr() as u64,
    ) != 0
    {
        return;
    }
    if event[0] != EVENT_CHARACTER {
        return;
    }
    let Some(code) = evdev_code(event[1]) else {
        return;
    };

    let time = shell.frames.wrapping_mul(16);
    let mut out = Out::new();
    for state in [key_state::PRESSED, key_state::RELEASED] {
        let serial = shell.next_serial();
        out.start(shell.keyboard, event::keyboard::KEY);
        out.word(serial);
        out.word(time);
        out.word(code);
        out.word(state);
        connection.send(out.finish());
    }
    shell.keys += 1;
    say("wlserver: forwarded a key to the client");
}

/// An evdev key code for a character, on an English layout.
///
/// `None` for anything not on this small keyboard, because a code invented for
/// a character would be a key the client believes was pressed.
fn evdev_code(character: u32) -> Option<u32> {
    /// The letters in the order their codes run, which is the order they are on
    /// the keyboard and not the order of the alphabet.
    const LETTERS: &[u8] = b"qwertyuiopasdfghjklzxcvbnm";
    const LETTER_CODES: &[u32] = &[
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, // q to p
        30, 31, 32, 33, 34, 35, 36, 37, 38, // a to l
        44, 45, 46, 47, 48, 49, 50, // z to m
    ];
    /// Zero is after nine on a keyboard, not before one.
    const DIGIT_CODES: &[u32] = &[11, 2, 3, 4, 5, 6, 7, 8, 9, 10];

    let character = u8::try_from(character).ok()?;
    let lowered = character.to_ascii_lowercase();
    if let Some(at) = LETTERS.iter().position(|letter| *letter == lowered) {
        return Some(LETTER_CODES[at]);
    }
    if lowered.is_ascii_digit() {
        return Some(DIGIT_CODES[usize::from(lowered - b'0')]);
    }
    match lowered {
        b' ' => Some(57),
        b'\n' | b'\r' => Some(28),
        _ => None,
    }
}

/// Act on one request.
fn dispatch(
    connection: &mut Connection,
    objects: &mut [Object; MAX_OBJECTS],
    screen: &Screen,
    shell: &mut Shell,
    request: &Request,
) {
    let mut out = Out::new();
    let object = request.object;

    if object == nexus_guest::wayland::DISPLAY {
        match request.opcode {
            request::display::SYNC => {
                // A `wl_callback` that is done immediately and then destroyed.
                // Every client's first round trip is this, and a server that
                // did not answer it would leave the client waiting for ever.
                let callback = request.word(0);
                out.start(callback, event::callback::DONE);
                out.word(0); // the serial, which nothing here counts
                connection.send(out.finish());
                delete(connection, callback);
            }
            request::display::GET_REGISTRY => {
                let registry = request.word(0);
                remember(objects, registry, Kind::Registry);
                // Everything this server offers, announced at once. A client
                // learns what is here only from these.
                announce(
                    connection,
                    registry,
                    GLOBAL_COMPOSITOR,
                    interface::COMPOSITOR,
                    4,
                );
                announce(connection, registry, GLOBAL_SHM, interface::SHM, 1);
                announce(connection, registry, GLOBAL_OUTPUT, interface::OUTPUT, 2);
                announce(connection, registry, GLOBAL_SEAT, interface::SEAT, 5);
                announce(connection, registry, GLOBAL_WM_BASE, interface::WM_BASE, 2);
            }
            _ => {}
        }
        return;
    }

    match (kind_of(objects, object), request.opcode) {
        (Kind::Registry, request::registry::BIND) => {
            bind(connection, objects, shell, request);
        }
        (Kind::Compositor, request::compositor::CREATE_SURFACE) => {
            let surface = request.word(0);
            remember(objects, surface, Kind::Surface);
            shell.surface = surface;
        }
        (Kind::Shm, request::shm::CREATE_POOL) => {
            // `create_pool(new_id, fd, size)`. The descriptor is not in the
            // message -- it arrived beside it, and this is where it is taken.
            let pool = request.word(0);
            let size = u64::from(request.word(1));
            let Some(descriptor) = connection.take_descriptor() else {
                fail(208)
            };
            let at = syscall6(
                call::MMAP,
                0,
                size,
                3, // PROT_READ | PROT_WRITE
                1, // MAP_SHARED
                descriptor as u64,
                0,
            );
            if at < 0 {
                fail(209)
            }
            let _ = syscall1(call::CLOSE, descriptor as u64);
            let slot = remember(objects, pool, Kind::ShmPool);
            objects[slot].at = at as u64;
            objects[slot].size = size;
        }
        (Kind::ShmPool, request::shm_pool::CREATE_BUFFER) => {
            // `create_buffer(new_id, offset, width, height, stride, format)`.
            let buffer = request.word(0);
            let offset = u64::from(request.word(1));
            let pool = find(objects, object);
            let base = objects[pool].at;
            let slot = remember(objects, buffer, Kind::Buffer);
            objects[slot].at = base + offset;
            objects[slot].width = request.word(2);
            objects[slot].height = request.word(3);
            objects[slot].stride = request.word(4);
        }
        (Kind::Buffer, request::buffer::DESTROY) => {
            forget(objects, object);
            delete(connection, object);
        }
        (Kind::Surface, request::surface::ATTACH) => {
            // Remembered against the surface until it is committed: a client
            // may attach and then change its mind.
            shell.attached = request.word(0);
        }
        (Kind::Surface, request::surface::DAMAGE)
        | (Kind::Surface, request::surface::DAMAGE_BUFFER) => {
            // `damage` is in surface coordinates and `damage_buffer` in buffer
            // coordinates. With no scaling and no transform set they are the
            // same rectangle, which is why both arrive here -- and why a
            // compositor that grew either would have to separate them.
            shell.damage = shell.damage.union(Rect {
                x: request.word(0),
                y: request.word(1),
                width: request.word(2),
                height: request.word(3),
            });
        }
        (Kind::Surface, request::surface::FRAME) => {
            // "Tell me when it would be worth drawing again." The callback is
            // fired after the frame reaches the screen; a client that asked for
            // one and never got it simply stops drawing, which is what a
            // missing frame callback looks like from outside.
            shell.frame = request.word(0);
        }
        (Kind::Surface, request::surface::COMMIT) => {
            commit(connection, objects, screen, shell);
        }
        (Kind::WmBase, request::wm_base::GET_XDG_SURFACE) => {
            // `get_xdg_surface(new_id, surface)`.
            let xdg = request.word(0);
            remember(objects, xdg, Kind::XdgSurface);
            shell.xdg_surface = xdg;
        }
        (Kind::WmBase, request::wm_base::PONG) => {
            if request.word(0) == shell.ping {
                shell.ponged = true;
            }
        }
        (Kind::XdgSurface, request::xdg_surface::GET_TOPLEVEL) => {
            let toplevel = request.word(0);
            remember(objects, toplevel, Kind::Toplevel);
            shell.toplevel = toplevel;
        }
        (Kind::XdgSurface, request::xdg_surface::ACK_CONFIGURE) => {
            shell.acked = request.word(0);
        }
        (Kind::Toplevel, request::toplevel::SET_TITLE) => {
            let (title, _) = request.text(0);
            say_two("wlserver: the window is called ", title);
        }
        (Kind::Toplevel, request::toplevel::SET_APP_ID) => {
            let (application, _) = request.text(0);
            say_two("wlserver: its application is ", application);
        }
        (Kind::Seat, request::seat::GET_KEYBOARD) => {
            let keyboard = request.word(0);
            remember(objects, keyboard, Kind::Keyboard);
            shell.keyboard = keyboard;
            send_keymap(connection, keyboard);
        }
        _ => {}
    }
}

/// `wl_registry.bind`: hand the client an object for one of the globals.
fn bind(
    connection: &mut Connection,
    objects: &mut [Object; MAX_OBJECTS],
    shell: &mut Shell,
    request: &Request,
) {
    // `bind(name, interface, version, new_id)`. The interface is a string, so
    // the two arguments after it are at an offset that depends on its length.
    let global = request.word(0);
    let (name, words) = request.text(1);
    let new_id = request.word(1 + words + 1);
    let bound = match global {
        GLOBAL_COMPOSITOR if name == interface::COMPOSITOR => Kind::Compositor,
        GLOBAL_SHM if name == interface::SHM => Kind::Shm,
        GLOBAL_OUTPUT if name == interface::OUTPUT => Kind::Output,
        GLOBAL_SEAT if name == interface::SEAT => Kind::Seat,
        GLOBAL_WM_BASE if name == interface::WM_BASE => Kind::WmBase,
        _ => Kind::None,
    };
    if bound == Kind::None || new_id == 0 {
        return;
    }
    remember(objects, new_id, bound);

    let mut out = Out::new();
    match bound {
        Kind::Shm => {
            // A `wl_shm` tells the client which pixel layouts it takes. A
            // client that got none would have nothing it could send.
            out.start(new_id, event::shm::FORMAT);
            out.word(nexus_guest::wayland::XRGB8888);
            connection.send(out.finish());
        }
        Kind::Seat => {
            // What this seat has. A keyboard and nothing else: there is a
            // pointer on this machine, but nothing here reads it, and a seat
            // that claimed one would be a client waiting for motion that never
            // comes.
            out.start(new_id, event::seat::CAPABILITIES);
            out.word(capability::KEYBOARD);
            connection.send(out.finish());
            out.start(new_id, event::seat::NAME);
            out.text("nexus-seat0");
            connection.send(out.finish());
        }
        Kind::Output => send_output(connection, new_id),
        Kind::WmBase => shell.wm_base = new_id,
        _ => {}
    }
}

/// Describe the output: where it is, how large, and how it is scaled.
///
/// The sizes are the *window's*, not the screen's, because this compositor's
/// output is a window on somebody else's screen. Saying otherwise would tell a
/// client it had a nineteen-hundred pixel display to lay itself out on.
fn send_output(connection: &mut Connection, output: u32) {
    let mut out = Out::new();
    out.start(output, event::output::GEOMETRY);
    out.word(0); // x
    out.word(0); // y
    out.word(0); // physical width in millimetres: not known
    out.word(0); // physical height
    out.word(0); // subpixel order: not known
    out.text("NexusOS");
    out.text("nexus-wayland");
    out.word(0); // transform: normal
    connection.send(out.finish());

    out.start(output, event::output::MODE);
    out.word(mode::CURRENT | mode::PREFERRED);
    out.word(FIRST_WIDTH);
    out.word(FIRST_HEIGHT);
    out.word(60_000); // refresh, in millihertz
    connection.send(out.finish());

    out.start(output, event::output::SCALE);
    out.word(1);
    connection.send(out.finish());

    // And "that is all of it". Everything above is one description, and a
    // client applies none of it until this arrives.
    out.start(output, event::output::DONE);
    connection.send(out.finish());
}

/// Send the keyboard's keymap, which this compositor does not have.
///
/// The argument is an `fd`, so one has to travel with the event whatever the
/// format says -- there is no way to leave an `fd` out of a message. The format
/// is `NONE`, which is the honest answer: a real compositor sends an XKB keymap
/// as text, and producing one would need `xkbcommon` or a hand-written table. A
/// wrong keymap is worse than an absent one, because it puts letters under the
/// wrong keys and everything afterwards looks like a broken keyboard.
fn send_keymap(connection: &mut Connection, keyboard: u32) {
    let name = b"nexus-keymap\0";
    let memory = syscall2(call::MEMFD_CREATE, name.as_ptr() as u64, 0);
    expect(memory >= 0, 211);
    expect(syscall2(call::FTRUNCATE, memory as u64, 1) == 0, 212);

    let mut out = Out::new();
    out.start(keyboard, event::keyboard::KEYMAP);
    out.word(keymap::NONE);
    out.word(1); // how much is in the descriptor
    let bytes = out.finish();
    let _ = send_with_descriptor(connection.socket, bytes, memory as i32);
    let _ = syscall1(call::CLOSE, memory as u64);

    // How a held key repeats: milliseconds between repeats, then the delay
    // before the first. The client does the repeating, because only it knows
    // what a held key means in its own text field.
    out.start(keyboard, event::keyboard::REPEAT_INFO);
    out.word(25);
    out.word(600);
    connection.send(out.finish());
}

/// `wl_surface.commit`: everything the client has said since its last commit
/// becomes true at once.
fn commit(
    connection: &mut Connection,
    objects: &mut [Object; MAX_OBJECTS],
    screen: &Screen,
    shell: &mut Shell,
) {
    let mut out = Out::new();

    if shell.attached == 0 {
        // A commit with no buffer. In `xdg-shell` this is not a mistake but the
        // required first step: a client creates its surface, creates the
        // toplevel, and commits *nothing*, which is it saying "I am ready to be
        // told how big to be". The compositor answers with a configure, and
        // only then may the client attach anything.
        if shell.xdg_surface != 0 && shell.toplevel != 0 && shell.serial == 0 {
            configure(connection, shell);
            say("wlserver: a window asked to be mapped, and was told what size to be");
        }
        return;
    }

    let buffer = find(objects, shell.attached);
    if objects[buffer].kind != Kind::Buffer {
        return;
    }
    let attached = shell.attached;
    let (buffer_width, buffer_height) = (objects[buffer].width, objects[buffer].height);

    // A surface that shrank leaves the rest of the window showing what was
    // there before. Clearing is what a compositor does with the part of its
    // output the surface no longer covers.
    //
    // And when that happens the *whole* window has to be presented, not the
    // damage: the pixels that changed are the ones the surface stopped
    // covering, which by definition are outside anything the client damaged.
    // Presenting the damage alone would clear the window in memory and leave
    // the old, larger picture on the screen.
    let resized = buffer_width != shell.drawn_width || buffer_height != shell.drawn_height;
    if resized {
        fill(screen, BACKGROUND);
        shell.damage = whole_surface(buffer_width, buffer_height);
    }

    // Everything the client damaged, cut down to what it actually has. A client
    // is free to damage more than its buffer holds, and a compositor that
    // believed one would read past the end of a mapping.
    let damage = if shell.damage.is_nothing() {
        whole_surface(buffer_width, buffer_height)
    } else {
        clip(shell.damage, buffer_width, buffer_height)
    };
    shell.damage = Rect::nothing();

    copy(screen, &objects[buffer], damage);
    present(screen, if resized { whole(screen) } else { damage });
    shell.frames += 1;
    shell.drawn_width = buffer_width;
    shell.drawn_height = buffer_height;
    shell.attached = 0;

    // The client may reuse the buffer now. Without this a correct client waits
    // for ever before drawing again.
    out.start(attached, event::buffer::RELEASE);
    connection.send(out.finish());

    // The keyboard is on this surface as soon as there is something to look at.
    // A real compositor decides this by where the pointer is or which window
    // was last raised; with one window there is one answer.
    if !shell.entered && shell.keyboard != 0 {
        let serial = shell.next_serial();
        out.start(shell.keyboard, event::keyboard::ENTER);
        out.word(serial);
        out.word(shell.surface);
        out.array(&[]); // nothing is already held down
        connection.send(out.finish());

        // And nothing held among the modifiers. A client that never hears this
        // does not know whether shift is down, and most assume the worst.
        let serial = shell.next_serial();
        out.start(shell.keyboard, event::keyboard::MODIFIERS);
        out.word(serial);
        out.word(0); // depressed
        out.word(0); // latched
        out.word(0); // locked
        out.word(0); // group
        connection.send(out.finish());
        shell.entered = true;
        say("wlserver: the keyboard is on the client's surface");
    }

    // And the frame callback, which is the client's cue to draw the next one.
    if shell.frame != 0 {
        let callback = shell.frame;
        shell.frame = 0;
        out.start(callback, event::callback::DONE);
        out.word(shell.frames.wrapping_mul(16)); // a timestamp, in milliseconds
        connection.send(out.finish());
        delete(connection, callback);
    }

    say("wlserver: put a client buffer on the screen");
}

/// Tell the client about one global.
fn announce(connection: &Connection, registry: u32, name: u32, interface: &str, version: u32) {
    let mut out = Out::new();
    out.start(registry, event::registry::GLOBAL);
    out.word(name);
    out.text(interface);
    out.word(version);
    connection.send(out.finish());
}

/// Tell the client an identifier is free again.
fn delete(connection: &Connection, id: u32) {
    let mut out = Out::new();
    out.start(nexus_guest::wayland::DISPLAY, event::display::DELETE_ID);
    out.word(id);
    connection.send(out.finish());
}

/// Record an object, returning its slot.
fn remember(objects: &mut [Object; MAX_OBJECTS], id: u32, kind: Kind) -> usize {
    for (slot, object) in objects.iter_mut().enumerate() {
        if object.kind == Kind::None || object.id == id {
            *object = Object {
                id,
                kind,
                ..Object::empty()
            };
            return slot;
        }
    }
    0
}

/// Forget one, so its slot and its identifier can be used again.
fn forget(objects: &mut [Object; MAX_OBJECTS], id: u32) {
    for object in objects.iter_mut() {
        if object.kind != Kind::None && object.id == id {
            *object = Object::empty();
        }
    }
}

/// The slot an identifier is in, or zero.
fn find(objects: &[Object; MAX_OBJECTS], id: u32) -> usize {
    for (slot, object) in objects.iter().enumerate() {
        if object.kind != Kind::None && object.id == id {
            return slot;
        }
    }
    0
}

/// What kind of object an identifier names.
fn kind_of(objects: &[Object; MAX_OBJECTS], id: u32) -> Kind {
    if id == 0 {
        return Kind::None;
    }
    objects[find(objects, id)].kind
}

/// The whole of the screen, as a rectangle.
fn whole(screen: &Screen) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: screen.width,
        height: screen.height,
    }
}

/// The whole of a surface, as a rectangle.
fn whole_surface(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// A rectangle cut down to what is actually there.
fn clip(rectangle: Rect, width: u32, height: u32) -> Rect {
    if rectangle.x >= width || rectangle.y >= height {
        return Rect::nothing();
    }
    let right = rectangle.x.saturating_add(rectangle.width).min(width);
    let bottom = rectangle.y.saturating_add(rectangle.height).min(height);
    Rect {
        x: rectangle.x,
        y: rectangle.y,
        width: right - rectangle.x,
        height: bottom - rectangle.y,
    }
}

/// Copy the damaged part of a client's buffer onto the window.
///
/// Only the damaged part, which is the point of damage: a client that changed
/// one line of text has said so, and a compositor that copied the whole surface
/// anyway would have asked for the information and thrown it away.
fn copy(screen: &Screen, buffer: &Object, damage: Rect) {
    let rows = damage.height.min(screen.height.saturating_sub(damage.y));
    let columns = damage.width.min(screen.width.saturating_sub(damage.x));
    let mut row = 0;
    while row < rows {
        let source = buffer.at + u64::from(damage.y + row) * u64::from(buffer.stride);
        let target = screen.pixels + u64::from(damage.y + row) * u64::from(screen.stride);
        let mut column = 0;
        while column < columns {
            let at = u64::from(damage.x + column) * 4;
            // SAFETY: both addresses are inside mappings this program made, and
            // the loop is bounded by the smaller of the two sizes in each
            // direction.
            unsafe {
                let pixel = core::ptr::read_volatile((source + at) as *const u32);
                core::ptr::write_volatile((target + at) as *mut u32, pixel);
            }
            column += 1;
        }
        row += 1;
    }
}

/// Fill the whole window with one colour.
fn fill(screen: &Screen, colour: u32) {
    let mut row = 0;
    while row < screen.height {
        let to = screen.pixels + u64::from(row) * u64::from(screen.stride);
        let mut column = 0;
        while column < screen.width {
            // SAFETY: inside the mapping this program made, bounded by its size.
            unsafe {
                core::ptr::write_volatile((to + u64::from(column) * 4) as *mut u32, colour);
            }
            column += 1;
        }
        row += 1;
    }
}

/// Put a rectangle of the window on the screen.
fn present(screen: &Screen, damage: Rect) {
    if damage.is_nothing() {
        return;
    }
    let rectangle = [damage.x, damage.y, damage.width, damage.height];
    if syscall3(
        call::IOCTL,
        screen.descriptor,
        u64::from(DISPLAY_PRESENT),
        rectangle.as_ptr() as u64,
    ) != 0
    {
        fail(210)
    }
}

/// Whether there is something to read, waiting up to `milliseconds` for it.
///
/// `struct pollfd` is a descriptor, then two sixteen-bit fields: what is being
/// waited for, and what happened. Built by hand here because this layer has no
/// structures, and it is two words, which is shorter than a definition of it
/// would be.
fn readable(descriptor: u64, milliseconds: i64) -> bool {
    /// `POLLIN`: there is something to read, or the other end has gone.
    const POLLIN: u32 = 1;
    let mut fds = [descriptor as u32, POLLIN];
    let ready = syscall3(call::POLL, fds.as_mut_ptr() as u64, 1, milliseconds as u64);
    ready > 0 && (fds[1] >> 16) != 0
}

/// A tenth of a second, through the one call this layer has that sleeps.
fn sleep_briefly() {
    let word: u32 = 0;
    let timeout: [u64; 2] = [0, 100_000_000];
    let _ = syscall4(
        call::FUTEX,
        core::ptr::addr_of!(word) as u64,
        128, // FUTEX_WAIT_PRIVATE
        0,
        timeout.as_ptr() as u64,
    );
}

/// Two pieces of text on one line, without a formatter.
fn say_two(before: &str, after: &str) {
    nexus_guest::fmt::Line::new().text(before).text(after).say();
}

/// A `sockaddr_un` for a path.
fn sockaddr(name: &[u8]) -> ([u8; 110], u64) {
    let mut out = [0u8; 110];
    out[0..2].copy_from_slice(&1u16.to_le_bytes()); // AF_UNIX
    let take = if name.len() > 107 { 107 } else { name.len() };
    out[2..2 + take].copy_from_slice(&name[..take]);
    (out, 2 + take as u64 + 1)
}
