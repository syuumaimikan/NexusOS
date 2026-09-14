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
//! # Minimising
//!
//! The right button on a title bar puts a window away, and a strip along the
//! bottom of the rectangle holds a tab for each one that has been put away.
//! Clicking a tab brings its window back, raised and focused.
//!
//! The tab is the whole reason minimising is a feature rather than a trap. A
//! window that can be put away and not brought back has been destroyed with
//! extra steps, and the client would go on drawing frames into a surface
//! nobody would ever see again. So the strip is *reserved*: windows are laid
//! out and clamped within the rectangle above it, and cannot be moved over the
//! one place that can undo the thing.
//!
//! A minimised client is still told its frames are shown. It has not been
//! stopped, it is not being punished, and it does not know: a client that could
//! tell whether it was visible would be a client that could behave differently
//! when nobody was looking.
//!
//! # What it is not
//!
//! No window that is not a rectangle.

#![no_std]
#![no_main]

extern crate alloc;

/// Where this program's allocations come from.
///
/// It went the whole of its life without one: a compositor moves pixels and
/// never needed a string it had not been given. What it needs one for now is
/// arithmetic it has to report -- how much of the display each repaint actually
/// touched -- and a number that cannot be printed is a measurement nobody can
/// check.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

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
/// The network.
///
/// Held for the same reason as the settings directory, and read no more than
/// that one is: this program's business is the display. What it does with this
/// is hand it to the browser, and a compositor that could not would be a
/// machine where nothing started from the desktop could reach the wire.
const NETWORK: Handle = Handle(6);

/// The filesystem, held to lend to a terminal and read by nothing here.
const FILESYSTEM: Handle = Handle(7);

/// The speaker, held on the same terms and never used by this program.
///
/// A compositor that made a noise of its own would be a compositor with an
/// opinion about when a machine should beep, and that belongs to whatever the
/// person is actually using.
const SOUND: Handle = Handle(8);
/// And what the machine is doing: memory, processors, processes.
///
/// Lent to the terminal, which is where somebody asks a machine about itself.
/// Held separately from everything else because it is the one endowment that
/// describes the person using the machine rather than what a program may do to
/// it -- so a program can be given the disk and the speaker and not this.
const MACHINE: Handle = Handle(9);

/// The directory the system's settings live in.
///
/// Never read here. This program's whole business is the display, and what it
/// does with this handle is hand it to the one program that has business in it:
/// the setup wizard, on a machine that has never been configured. Holding a
/// handle in order to pass it on is a real thing to hold -- the alternative is
/// the wizard asking the kernel for the filesystem, which is ambient authority
/// with more steps.
const SETTINGS: Handle = Handle(5);

/// The keys this program gives the keyboard and the pointer in its wait set.
///
/// Above anything a client can be given, since client keys are an index shifted
/// left with a bit for which of its two things became ready.
const KEY_KEYBOARD: u64 = 0xFFFF;
const KEY_POINTER: u64 = 0xFFFE;
/// And the desktop's, whose channel and process are watched like a client's.
const KEY_SHELL: u64 = 0xFFFD;
const KEY_SHELL_ENDED: u64 = 0xFFFC;
/// The wallpaper's channel, which carries only "I have drawn".
const KEY_WALL: u64 = 0xFFFB;

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
    /// The same, for the right button, which puts a window away.
    other_held: bool,
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
    pub const RIGHT: u32 = 1 << 1;
}

/// What a key looks like on the wire: a kind, then a number.
///
/// Only tab is named here, because it is the only one this program acts on
/// itself. Everything else is forwarded without being looked at -- a compositor
/// that inspected the keys it routes would be a compositor that could read what
/// someone typed into a window.
mod key {
    pub const TAB: u8 = 5;
    /// A function key, numbered from one.
    pub const FUNCTION: u8 = 6;
    /// Not a key: the interface language changed, and this is what to.
    ///
    /// Passed to the desktop, which draws text, and to nobody else. It arrives
    /// on the key channel because that is where the kernel says it, and it is
    /// the one thing on that channel this program forwards somewhere other than
    /// to whoever has focus.
    pub const LANGUAGE: u8 = 7;
    /// Bytes one key takes.
    pub const SIZE: usize = 5;
}

/// What this program and the desktop say to each other.
///
/// The desktop is a client with a list: it is told which windows exist and what
/// state each is in, and it says what should happen to them. It cannot do any
/// of it itself -- it has no handle to a window, no way to reach the display,
/// and no idea where its own strip is.
mod desk {
    /// Here is every window and what state it is in.
    pub const WINDOWS: &[u8] = b"win";
    /// Somebody clicked in the strip, at these coordinates within it.
    pub const CLICK: &[u8] = b"clk";
    /// The interface language is now this.
    pub const LANGUAGE: &[u8] = b"lng";

    /// Bring this window back and give it focus.
    pub const SHOW: &[u8] = b"show";
    /// Put this window away.
    pub const HIDE: &[u8] = b"hide";
    /// Start another program.
    pub const OPEN: &[u8] = b"open";
    /// Start a window that can fetch a page.
    pub const BROWSE: &[u8] = b"web ";
    /// Start a window with a prompt in it.
    pub const TERMINAL: &[u8] = b"term";
    /// Show this machine its own settings.
    pub const SETTINGS: &[u8] = b"sett";
    /// Show what there is to install.
    pub const PACKAGES: &[u8] = b"pkgs";
    /// Show the pictures on this machine.
    pub const PICTURES: &[u8] = b"pics";
    /// Start the agent.
    pub const ASSIST: &[u8] = b"asst";
    /// End the session.
    ///
    /// The desktop asks; this program does it. Which is the right way round: a
    /// dock that could not end a session would be a machine with no way out
    /// except the power switch, and a dock that ended it *itself* would be a
    /// dock deciding when the display stops.
    pub const QUIT: &[u8] = b"quit";

    /// What state a window can be in, as the desktop is told it.
    pub const GONE: u8 = 0;
    pub const SHOWN: u8 = 1;
    pub const AWAY: u8 = 2;
    pub const FOCUSED: u8 = 3;
}

/// Where this program maps the framebuffer.
const FRAMEBUFFER_AT: usize = 0x0000_0000_2000_0000;
/// Where it maps the first client surface; the second goes a stride further on.
const SURFACES_AT: usize = 0x0000_0000_3000_0000;
/// Address space set aside per surface.
///
/// Twice what one surface may be, because a resize has the old and the new
/// mapped at once for the length of the copy between them.
///
/// Thirty-two megabytes. It was one, which was ample while this program owned a
/// rectangle in the corner of the screen and nowhere near enough the moment it
/// owned the screen: a window filling 1920 by 1200 is nine megabytes of pixels,
/// and what a limit that is too small looks like is a program that will not
/// start with no hint as to why.
///
/// Address space, not memory. Six of these is a hundred and ninety-two
/// megabytes of *addresses* reserved in a space that has terabytes of them; the
/// pages behind a surface are allocated when the surface is made and are as
/// large as the window actually is.
const SURFACE_STRIDE: usize = 0x0200_0000;
/// The largest a surface may be, which is what bounds how large a window is.
const MAX_SURFACE: usize = SURFACE_STRIDE / 2;

/// The colour this machine picks things out in.
///
/// Set once, from the message that hands over the framebuffer. A static rather
/// than a field because the three places that draw chrome are free functions --
/// they take a tile and a rectangle, not the whole of this program's state --
/// and threading one colour through all of them would be threading it for the
/// sake of avoiding a word.
static ACCENT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0x0038_8BE8);

/// What that colour is, and shades of it.
///
/// Every piece of chrome is the same hue at a different weight, so a machine
/// with a green accent has a green title bar and a green focus ring rather than
/// a green button on a blue window.
fn accent(weight: u8) -> u32 {
    let colour = ACCENT.load(core::sync::atomic::Ordering::Relaxed);
    let (red, green, blue) = ((colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF);
    let weight = u32::from(weight);
    ((red * weight / 255) << 16) | ((green * weight / 255) << 8) | (blue * weight / 255)
}

/// The interface language the kernel last announced, or `NO_LANGUAGE`.
///
/// Remembered because it is a fact about the machine and it is announced as an
/// event. A program that starts after the announcement -- the desktop, on a
/// machine that has just been through its first-run setup -- would otherwise
/// never hear it: the wizard was running when the kernel said it, and the
/// wizard is not the desktop. So the last one said is kept and passed on to
/// whoever starts next.
static LANGUAGE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(NO_LANGUAGE);

/// What [`LANGUAGE`] holds before anything has been announced.
const NO_LANGUAGE: u32 = u32::MAX;

/// Note what the kernel said the interface language is now.
fn remember_language(message: &[u8]) {
    if message.len() >= 5 && message[0] == key::LANGUAGE {
        LANGUAGE.store(read_u32(message, 1), core::sync::atomic::Ordering::Relaxed);
    }
}

/// Tell a client the interface language, if one has been announced.
fn tell_language(channel: Handle) {
    let index = LANGUAGE.load(core::sync::atomic::Ordering::Relaxed);
    if index == NO_LANGUAGE {
        return;
    }
    let mut forward = [0u8; desk::LANGUAGE.len() + 4];
    forward[..desk::LANGUAGE.len()].copy_from_slice(desk::LANGUAGE);
    forward[desk::LANGUAGE.len()..].copy_from_slice(&index.to_le_bytes());
    nexus_user::send(channel, &forward, &[]).ok();
}

/// Which function key ends the session.
///
/// F10, because F1 is already the language. There is no modifier here to hang
/// it off -- the keyboard decoder reports the key and not the shift state --
/// so it is a key nothing else uses rather than a chord.
const LEAVE_KEY: u32 = 10;

/// The program this one gives surfaces to.
const CLIENT: &[u8] = b"BIN/CLIENT.ELF";
/// The one it runs instead, once, on a machine nobody has set up.
const SETUP: &[u8] = b"BIN/SETUP.ELF";
/// And the one that fetches a page, which is the only client given the network.
const BROWSER: &[u8] = b"BIN/BROWSE.ELF";
/// And the one with a prompt in it, which is the only client given the disk.
const TERMINAL: &[u8] = b"BIN/TERM.ELF";
/// And the one that draws what is behind everything else.
const WALLPAPER: &[u8] = b"BIN/WALL.ELF";
/// And the one that changes the file the other two read.
const SETTINGS_WINDOW: &[u8] = b"BIN/SET.ELF";
/// And the one that shows what is installed and installs more.
const STORE: &[u8] = b"BIN/STORE.ELF";
/// And the one that shows pictures.
const VIEWER: &[u8] = b"BIN/VIEW.ELF";
/// And the agent, which is given less than any of them.
const ASSISTANT: &[u8] = b"BIN/ASSIST.ELF";
/// The program that draws the strip along the bottom and says what a click in
/// it means.
const SHELL: &[u8] = b"BIN/SHELL.ELF";
/// How many windows there can be at once.
///
/// A fixed number of slots rather than a list, because every one of them is a
/// megabyte of address space set aside for a surface and an entry in a wait
/// set: a compositor that grew both without a bound would be a compositor a
/// client could exhaust by asking for windows.
const CLIENTS: usize = 4;
/// How many are started at boot.
///
/// The rest are empty until somebody presses the button on the desktop. Two,
/// because two is what it takes to show that keys reach one window and not the
/// other, and a machine that filled its screen before anyone touched it would
/// have nothing left to launch into.
const STARTED: usize = 2;
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
///
/// Twenty and not twelve. Twelve was chosen when this program owned a fifth of
/// the display and every window was a few hundred pixels across; on a screen
/// nineteen hundred wide it is a target a hand has to be told about to find,
/// and the test that drives the pointer into it was the first thing to notice.
const GRIP: u32 = 20;
/// How tall the strip along the bottom is.
///
/// Reserved: windows are laid out and clamped above it, so the one place that
/// brings a minimised window back cannot be covered by another window. Tall
/// enough for a line of text, because what is in it is drawn by a program with
/// a font and not by this one with rectangles.
///
/// Thirty-six and not twenty-four, for the same reason the grip grew: the
/// strip was sized when it lived at the bottom of a fifth of the display, and
/// on a screen twelve hundred tall a twenty-four-pixel bar with twenty-pixel
/// tabs in it is a row of targets a hand has to aim at.
const TASKBAR: u32 = 36;

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

/// A rectangle of the display, in screen coordinates.
///
/// Held as edges rather than as a position and a size, because every operation
/// on it is a comparison of edges and an origin-plus-extent form spends its
/// life adding the two back together.
#[derive(Clone, Copy)]
struct Region {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Region {
    /// Nothing.
    const fn nothing() -> Self {
        Self {
            left: u32::MAX,
            top: u32::MAX,
            right: 0,
            bottom: 0,
        }
    }

    /// A rectangle from a corner and a size.
    const fn of(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        }
    }

    /// Whether there is anything in it.
    const fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// The smallest rectangle holding both.
    ///
    /// A union of rectangles is not a rectangle, and this is the bounding box
    /// rather than the union: two windows at opposite corners give a damage
    /// region covering the whole screen. That is the approximation this
    /// compositor makes, and it is worth naming -- a list of disjoint
    /// rectangles would repaint less and is a great deal more arithmetic to get
    /// right. What it buys as it stands is the common case: one window
    /// redrawing while nothing else moves.
    const fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            left: if self.left < other.left {
                self.left
            } else {
                other.left
            },
            top: if self.top < other.top {
                self.top
            } else {
                other.top
            },
            right: if self.right > other.right {
                self.right
            } else {
                other.right
            },
            bottom: if self.bottom > other.bottom {
                self.bottom
            } else {
                other.bottom
            },
        }
    }

    /// How many pixels it covers.
    const fn area(self) -> u32 {
        if self.is_empty() {
            0
        } else {
            (self.right - self.left) * (self.bottom - self.top)
        }
    }
}

/// The whole of the rectangle this program owns.
fn everything(screen: &Screen) -> Region {
    Region::of(screen.x, screen.y, screen.width, screen.height)
}

/// Where the wallpaper is: everything above the strip.
fn region_of_wallpaper(screen: &Screen) -> Region {
    Region::of(screen.x, screen.y, screen.width, screen.usable_height())
}

/// The strip along the bottom.
fn strip(screen: &Screen) -> Region {
    Region::of(screen.x, screen.taskbar_y(), screen.width, TASKBAR)
}

/// Where a window is.
fn region_of(tile: &Tile) -> Region {
    Region::of(tile.x, tile.y, tile.width, tile.height)
}

/// Repaints performed, and how many pixels they were asked to cover.
///
/// The second is what damage tracking is about: before it, every repaint was
/// the whole rectangle whatever had changed.
static REPAINTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DAMAGED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The rectangle drawing is currently confined to.
///
/// A property of the repaint in progress, and there is exactly one at a time
/// because there is one thread. Kept here rather than threaded through every
/// drawing call because the alternative is an extra parameter on `fill`,
/// `composite`, and every decoration that calls them -- which is a lot of
/// places to get right for something that is the same value in all of them.
mod clip {
    use core::sync::atomic::{AtomicU32, Ordering};

    static LEFT: AtomicU32 = AtomicU32::new(0);
    static TOP: AtomicU32 = AtomicU32::new(0);
    static RIGHT: AtomicU32 = AtomicU32::new(u32::MAX);
    static BOTTOM: AtomicU32 = AtomicU32::new(u32::MAX);
    /// Pixels actually written, which is what damage tracking is for.
    static WRITTEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Confine drawing to this rectangle until the next call.
    pub fn set(region: super::Region) {
        LEFT.store(region.left, Ordering::Relaxed);
        TOP.store(region.top, Ordering::Relaxed);
        RIGHT.store(region.right, Ordering::Relaxed);
        BOTTOM.store(region.bottom, Ordering::Relaxed);
    }

    /// What it is now.
    pub fn current() -> (u32, u32, u32, u32) {
        (
            LEFT.load(Ordering::Relaxed),
            TOP.load(Ordering::Relaxed),
            RIGHT.load(Ordering::Relaxed),
            BOTTOM.load(Ordering::Relaxed),
        )
    }

    /// Note pixels written.
    pub fn wrote(count: u64) {
        WRITTEN.fetch_add(count, Ordering::Relaxed);
    }

    /// How many have been written altogether.
    pub fn written() -> u64 {
        WRITTEN.load(Ordering::Relaxed)
    }
}

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
    /// Whether this machine has been through its first-run setup.
    configured: bool,
}

impl Screen {
    /// The part windows may occupy: everything but the strip of tabs.
    fn usable_height(&self) -> u32 {
        self.height.saturating_sub(TASKBAR)
    }

    /// Where the strip of tabs begins.
    fn taskbar_y(&self) -> u32 {
        self.y + self.usable_height()
    }
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
    /// Put away, and reachable only by its tab.
    minimised: bool,
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

    /// What to tell the desktop about this window.
    fn state(&self, focused: bool) -> u8 {
        if !self.live {
            desk::GONE
        } else if self.minimised {
            desk::AWAY
        } else if focused {
            desk::FOCUSED
        } else {
            desk::SHOWN
        }
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
    // Before anything allocates, which here is only the report at the end.
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("compositor: FAILED: could not get a heap");
        finish();
    }

    let Some(screen) = take_the_display() else {
        finish();
    };

    // Everything, once, before anything else draws. What was on the display a
    // moment ago is whatever the kernel last painted, and it is not this
    // program's -- leaving it visible under the parts nothing has covered yet
    // is a screen showing two systems at once.
    fill(
        &screen,
        screen.x,
        screen.y,
        screen.width,
        screen.height,
        BACKGROUND,
    );

    // A machine nobody has set up gets one window and one program, filling the
    // rectangle this owns. Nothing else is started until it has finished: there
    // is no desktop to put anything on until somebody has said what language it
    // is in and who it belongs to.
    if !screen.configured {
        run_setup(&screen);
    }

    // Two tiles side by side, with a gap around and between them. Laid out
    // before any client exists, because a client is told the size of its
    // surface and cannot be told twice.
    let tile_width = (screen.width - GAP * (STARTED as u32 + 1)) / STARTED as u32;
    let tile_height = screen.usable_height() - GAP * 2;
    if tile_width == 0 || tile_height == 0 {
        failed("compositor: FAILED: the rectangle it was given is too small to divide");
        finish();
    }

    // What is behind the windows. Started first, so that the first frame
    // anything draws already has something under it -- and given a surface the
    // size of the area windows may occupy, because a wallpaper that included
    // the strip would be a wallpaper drawn over by the desktop every minute.
    let mut wallpaper = start_wallpaper(&screen);

    let mut tiles: [Option<Tile>; CLIENTS] = [const { None }; CLIENTS];
    for (index, slot) in tiles.iter_mut().enumerate().take(STARTED) {
        let x = screen.x + GAP + (tile_width + GAP) * index as u32;
        let y = screen.y + GAP;
        match start_client(index, x, y, tile_width, tile_height) {
            Some(tile) => *slot = Some(tile),
            None => finish(),
        }
    }

    // And the desktop, which gets the strip. Started last, so that the first
    // list of windows it is sent describes something that already exists --
    // a dock drawn before there was anything to put in it would show an empty
    // machine for as long as it took the first client to appear.
    //
    // Given the settings directory too, read-only: the strip shows a clock and
    // whose machine this is, and both are in that file. Read and transfer, not
    // write -- a dock that could rewrite the machine's settings is a dock that
    // can lock somebody out of it, and drawing a clock does not need that.
    let for_shell = nexus_user::duplicate(
        SETTINGS,
        nexus_user::rights::READ | nexus_user::rights::TRANSFER,
    );
    let for_shell = match for_shell {
        Ok(handle) => [handle],
        Err(_) => {
            failed("compositor: FAILED: could not pass the settings on to the desktop");
            finish();
        }
    };
    let shell = start_program(
        SHELL,
        CLIENTS,
        screen.x,
        screen.taskbar_y(),
        screen.width,
        TASKBAR,
        CLIENTS as u32,
        &for_shell,
    );
    if let Some(shell) = &shell {
        tell_language(shell.channel);
    }
    let mut shell = match shell {
        Some(shell) => Some(shell),
        None => {
            failed("compositor: FAILED: the desktop would not start");
            finish();
        }
    };

    serve(&screen, &mut tiles, &mut shell, &mut wallpaper);
    finish()
}

/// Start the program that draws the background.
///
/// Not fatal if it will not start. A machine with no wallpaper is a machine
/// with a plain background, which is what this program painted before there was
/// one; a machine that refused to show a desktop because a decoration failed
/// would be trading everything for nothing.
fn start_wallpaper(screen: &Screen) -> Option<Tile> {
    let Ok(theirs) = nexus_user::duplicate(
        SETTINGS,
        nexus_user::rights::READ | nexus_user::rights::TRANSFER,
    ) else {
        nexus_user::log("compositor: no settings to lend the wallpaper; it will draw the default")
            .ok();
        return None;
    };

    let tile = start_program(
        WALLPAPER,
        // Past every window and past the desktop, so its surface does not sit
        // where one of theirs will go.
        CLIENTS + 2,
        screen.x,
        screen.y,
        screen.width,
        screen.usable_height(),
        0,
        &[theirs],
    );
    if tile.is_none() {
        nexus_user::log("compositor: the wallpaper would not start; the background stays plain")
            .ok();
    }
    tile
}

/// Run the first-run wizard, and wait for it.
///
/// One client, the whole rectangle, and nothing else running. It is given the
/// settings directory as well as its surface, with write on it -- one of the
/// two programs that get it that way, the other being the settings window, and
/// between them the reason this program holds that handle at all.
fn run_setup(screen: &Screen) {
    let Ok(theirs) = nexus_user::duplicate(
        SETTINGS,
        nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
    ) else {
        failed("compositor: FAILED: could not pass on the settings directory");
        return;
    };

    let Some(tile) = start_program(
        SETUP,
        // The slot after every window, so its surface does not sit where a
        // client's will go: the wizard finishes and the windows start, and two
        // mappings at one address would be one of them writing over the other.
        CLIENTS + 1,
        screen.x + GAP,
        screen.y + GAP,
        screen.width - GAP * 2,
        screen.usable_height() - GAP * 2,
        0,
        &[theirs],
    ) else {
        failed("compositor: FAILED: the setup program would not start");
        return;
    };

    nexus_user::log("compositor: this machine has not been set up; showing the wizard").ok();

    // Its own little event loop, because none of the rest of this program's
    // machinery exists yet: no desktop, no other windows, no order to keep. Two
    // things are watched, its channel and its process, so that a wizard that
    // stops is noticed as readily as one that draws.
    let Ok(set) = nexus_user::wait_set() else {
        failed("compositor: FAILED: could not make a wait set");
        return;
    };
    const DREW: u64 = 1;
    const ENDED: u64 = 2;
    const TYPED: u64 = 3;
    if nexus_user::watch(set, tile.channel, DREW).is_err()
        || nexus_user::watch(set, tile.process, ENDED).is_err()
        || nexus_user::watch(set, KEYS, TYPED).is_err()
    {
        failed("compositor: FAILED: could not watch the wizard");
        return;
    }

    let mut keys = [0u64; 3];
    let mut frames = 0u32;
    // Generous: somebody is typing, and reading. Bounded all the same, because
    // a wizard that stopped drawing without ending would otherwise hold the
    // machine before anything else has started.
    for _ in 0..65_536 {
        let Ok(count) = nexus_user::wait_any(set, &mut keys) else {
            break;
        };
        if count == 0 {
            break;
        }
        let mut finished = false;
        for key in &keys[..count] {
            match *key {
                ENDED => finished = true,
                // Whatever the keyboard sent, straight to the wizard: it is the
                // only thing running, so there is no routing decision to make.
                //
                // Taken from the set rather than read on spec. A plain receive
                // here blocks until a key arrives, and the wizard finishing is
                // not a key -- so this loop would sit waiting for a keystroke
                // for a program that had already gone, and the desktop would
                // never start. It did exactly that.
                TYPED => {
                    let mut message = [0u8; 32];
                    let mut none = [Handle(0); 1];
                    match nexus_user::receive(KEYS, &mut message, &mut none) {
                        Ok(received) if received.bytes > 0 => {
                            remember_language(&message[..received.bytes]);
                            if nexus_user::send(tile.channel, &message[..received.bytes], &[])
                                .is_err()
                            {
                                finished = true;
                            }
                        }
                        // The kernel has stopped sending. Not a failure: it
                        // means there is no keyboard, and the wizard can still
                        // be finished from one that appears later.
                        _ => {
                            nexus_user::unwatch(set, TYPED).ok();
                        }
                    }
                }
                _ => {
                    let mut message = [0u8; 32];
                    let mut none = [Handle(0); 1];
                    match nexus_user::receive(tile.channel, &mut message, &mut none) {
                        Ok(_) => {
                            frames += 1;
                            composite(screen, &tile);
                            if nexus_user::send(tile.channel, b"shown", &[]).is_err() {
                                finished = true;
                            }
                        }
                        Err(_) => finished = true,
                    }
                }
            }
        }
        if finished {
            break;
        }
    }

    nexus_user::log(&alloc::format!(
        "compositor: the wizard drew {frames} frames and finished"
    ))
    .ok();

    // Everything it had goes back, including the address space its surface was
    // mapped at -- the windows that come next need it.
    nexus_user::memory_unmap(tile.surface, tile.mapped_at).ok();
    nexus_user::close(tile.surface).ok();
    nexus_user::close(tile.channel).ok();
    nexus_user::close(tile.process).ok();
    nexus_user::close(set).ok();
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
    if received.handles != 1 || received.bytes < 36 {
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
        // Decided by the kernel, which is the only thing that can read the
        // store before a program runs. One bit: what the setting *says* is the
        // business of whoever shows it.
        configured: received.bytes >= 36 && read_u32(&buffer, 32) != 0,
    };

    // What this machine picks things out in, read by the kernel out of the
    // settings and handed over as one number. This program does not open the
    // settings file: a compositor with an opinion about what a setting means is
    // a compositor deciding what a desktop looks like.
    if received.bytes >= 40 {
        ACCENT.store(read_u32(&buffer, 36), core::sync::atomic::Ordering::Relaxed);
    }

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
    // A different tint per client, so two tiles that are the same colour mean
    // one buffer reached both and not that compositing worked.
    start_program(
        CLIENT,
        index,
        x,
        y,
        width,
        height,
        0x40u32 + index as u32 * 0x70,
        &[],
    )
}

/// Start a program, make it a surface, and give it one end of the memory.
///
/// The same path for a client and for the desktop, because the desktop *is* a
/// client: one surface, one channel, no handle to anything else. What differs
/// is only which program is started and what the third number in its first
/// message means -- a tint for a client, a count of window slots for the
/// desktop.
#[allow(clippy::too_many_arguments)]
fn start_program(
    program: &[u8],
    index: usize,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    third: u32,
    // Anything else the program is to be given, sent with its surface rather
    // than after it. In the same message on purpose: a program that had to read
    // two would have to know there were two, and one that was started without
    // the second would block for ever waiting for something nobody was going to
    // send.
    also: &[nexus_user::Handle],
) -> Option<Tile> {
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
    if nexus_user::send(SPAWNER, program, &[]).is_err() {
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
    message[8..12].copy_from_slice(&third.to_le_bytes());
    // And which client it is, only so that it can say so. A client has no use
    // for the number beyond naming itself in a log -- it cannot address another
    // client, and there is nothing for it to index into.
    message[12..16].copy_from_slice(&(index as u32).to_le_bytes());

    let mut handed = alloc::vec::Vec::with_capacity(1 + also.len());
    handed.push(theirs);
    handed.extend_from_slice(also);
    if nexus_user::send(channel, &message, &handed).is_err() {
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
        minimised: false,
    })
}

/// The loop.
///
/// One wait over every client's channel and every client's process. A client
/// saying it has drawn and a client dying arrive the same way, which is the
/// only arrangement in which neither can be starved by the other.
fn serve(
    screen: &Screen,
    tiles: &mut [Option<Tile>; CLIENTS],
    shell: &mut Option<Tile>,
    wallpaper: &mut Option<Tile>,
) {
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

    if let Some(shell) = shell {
        if nexus_user::watch(set, shell.channel, KEY_SHELL).is_err()
            || nexus_user::watch(set, shell.process, KEY_SHELL_ENDED).is_err()
        {
            failed("compositor: FAILED: could not watch the desktop");
            return;
        }
    }

    if let Some(wallpaper) = wallpaper.as_ref() {
        // Its channel only. Whether the *process* has ended does not matter:
        // a wallpaper that stopped leaves its last frame on screen, which is a
        // background, and there is nothing to rearrange when it goes.
        if nexus_user::watch(set, wallpaper.channel, KEY_WALL).is_err() {
            failed("compositor: FAILED: could not watch the wallpaper");
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
        other_held: false,
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
    // How many programs the desktop has asked to start, and how many of its
    // commands changed a window.
    let mut opened = 0u32;
    let mut commanded = 0u32;
    // What the desktop was last told, so it is told again only when it differs.
    // Seeded with a state no window can be in, so the first list always goes.
    let mut reported = [u8::MAX; CLIENTS];
    // Damage gathered over everything one wake-up brought, and composited once
    // at the end of it.
    //
    // A wait returns every member that is ready, which on a busy machine is
    // several: two clients that both drew, a key, and the desktop's own frame
    // can all arrive together. Compositing after each of them puts the same
    // pixels on the display three times and shows nobody the first two -- so
    // the work is done, the damage is remembered, and the display is touched
    // once when there is nothing left to do.
    //
    // This is as close to frame scheduling as a machine with no vertical blank
    // can get. There is no deadline here and no notion of a frame's duration:
    // what it does is refuse to composite faster than it is asked to, which is
    // the half of the problem that does not need the hardware to tell it when a
    // scanout began.
    let mut pending = Region::nothing();
    let mut batched = 0u32;

    announce(shell, tiles, focus, &mut reported);
    repaint(
        screen,
        tiles,
        shell,
        wallpaper,
        &order,
        focus,
        &mut cursor,
        pointing,
        everything(screen),
    );

    // Whether somebody has asked the session to end, and whether the summary
    // of what was composited has been said. They are different moments: the
    // numbers are worth having when the last window closes, which on a machine
    // somebody is using is the middle of the session and not the end of it.
    let mut leaving = false;
    let mut summarised = false;
    // How many frames the wallpaper has drawn, for the summary.
    let mut painted = 0u32;

    // Bounded, so a client that neither draws nor dies cannot hang the machine.
    // The bound is a backstop and not a schedule, and it is large because this
    // is a desktop session: a person who uses a machine for an afternoon
    // generates a great many wake-ups, and a compositor that stopped after a
    // thousand of them would be a machine that logs itself out at lunchtime.
    for _ in 0..16_000_000u64 {
        // The last window has closed. That used to end the session, which was
        // the wrong answer to the right observation: a desktop with nothing on
        // it is a desktop, not a finished machine. What it ends is the run of
        // work worth reporting on, so the numbers are said here, once, and the
        // session carries on with an empty screen and a strip that can start
        // something new.
        if ended > 0 && !summarised && tiles.iter().flatten().all(|tile| !tile.live) {
            summarised = true;
            summarise(screen, composited, batched);
            nexus_user::log("compositor: every window has closed; the desktop is empty").ok();
        }

        if leaving {
            break;
        }

        let mut keys = [0u64; CLIENTS * 2 + 2];
        let Ok(count) = nexus_user::wait_any(set, &mut keys) else {
            failed("compositor: FAILED: could not wait on its clients");
            return;
        };
        if count == 0 {
            break;
        }

        for key in &keys[..count] {
            if *key == KEY_KEYBOARD {
                match read_key(set, tiles, shell, &mut focus) {
                    Some(typed) => {
                        if matches!(typed, Typed::Forwarded) {
                            forwarded += 1;
                        }
                        if matches!(typed, Typed::Leave) {
                            leaving = true;
                        }
                        announce(shell, tiles, focus, &mut reported);
                        // Everything, because a key may have moved the focus,
                        // and a focus ring is on two windows at once.
                        pending = pending.union(everything(screen));
                    }
                    None => return,
                }
                continue;
            }

            if *key == KEY_POINTER {
                match read_pointer(
                    screen,
                    set,
                    tiles,
                    shell,
                    &mut cursor,
                    &mut focus,
                    &mut order,
                ) {
                    // What the pointer says it changed. A movement with no
                    // button held changes nothing but the pointer itself, and
                    // that is the overwhelming majority of what a mouse sends.
                    Some(moved_damage) => {
                        moved += 1;
                        announce(shell, tiles, focus, &mut reported);
                        pending = pending.union(moved_damage);
                    }
                    None => return,
                }
                continue;
            }

            // The desktop, which is a client with a list rather than a window.
            if *key == KEY_SHELL {
                // Its own frames touch the strip and nothing else; its
                // commands move windows, which is everything.
                let asked = match read_shell(screen, set, tiles, shell, &mut focus, &mut order) {
                    Some(Asked::Nothing) => Region::nothing(),
                    Some(Asked::Ended) => {
                        // Not `return`: what is left of this turn round the
                        // loop is the composite of whatever else arrived with
                        // it, and a session that stopped mid-frame would leave
                        // half a repaint on the display it is handing back.
                        leaving = true;
                        Region::nothing()
                    }
                    Some(Asked::Drew) => {
                        composited += 1;
                        strip(screen)
                    }
                    Some(Asked::Changed) => {
                        commanded += 1;
                        everything(screen)
                    }
                    Some(Asked::Opened) => {
                        commanded += 1;
                        opened += 1;
                        everything(screen)
                    }
                    None => return,
                };
                announce(shell, tiles, focus, &mut reported);
                pending = pending.union(asked);
                continue;
            }

            if *key == KEY_WALL {
                // It has drawn. Everything above it has to be drawn again over
                // the part that changed, which is what `repaint` does anyway --
                // so the damage is the wallpaper's rectangle and the order
                // takes care of the rest.
                let mut message = [0u8; 32];
                let mut none = [Handle(0); 1];
                match wallpaper
                    .as_ref()
                    .map(|tile| nexus_user::receive(tile.channel, &mut message, &mut none))
                {
                    Some(Ok(_)) => {
                        if let Some(tile) = wallpaper.as_mut() {
                            tile.frames += 1;
                            if nexus_user::send(tile.channel, b"shown", &[]).is_err() {
                                nexus_user::unwatch(set, KEY_WALL).ok();
                            }
                        }
                        painted += 1;
                        pending = pending.union(region_of_wallpaper(screen));
                    }
                    _ => {
                        nexus_user::unwatch(set, KEY_WALL).ok();
                    }
                }
                continue;
            }

            if *key == KEY_SHELL_ENDED {
                // The desktop has gone. The windows have not, and a compositor
                // that stopped with them on screen would be throwing away
                // everything that still works because the dock died.
                nexus_user::unwatch(set, KEY_SHELL_ENDED).ok();
                nexus_user::unwatch(set, KEY_SHELL).ok();
                if let Some(gone) = shell.take() {
                    nexus_user::close(gone.surface).ok();
                    nexus_user::close(gone.channel).ok();
                    nexus_user::close(gone.process).ok();
                }
                nexus_user::log("compositor: the desktop ended; its strip is empty").ok();
                pending = pending.union(strip(screen));
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
                        // That window and no other. This is what damage
                        // tracking exists for: a client redrawing twice a
                        // second used to cost the whole display every time.
                        let drawn = region_of(tile);
                        pending = pending.union(drawn);
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
                announce(shell, tiles, focus, &mut reported);
                // Everything: its window goes, and what was behind it has to
                // come back.
                pending = pending.union(everything(screen));
            }
        }

        // Once, for everything that arrived together. On a machine where three
        // things become ready in the same instant this is one composite instead
        // of three, and the two that were skipped were never on screen long
        // enough for anybody to see them.
        if !pending.is_empty() {
            batched += 1;
            repaint(
                screen,
                tiles,
                shell,
                wallpaper,
                &order,
                focus,
                &mut cursor,
                pointing,
                pending,
            );
            pending = Region::nothing();
        }
    }

    for tile in tiles
        .iter()
        .flatten()
        .chain(shell.iter())
        .chain(wallpaper.iter())
    {
        nexus_user::close(tile.surface).ok();
        nexus_user::close(tile.channel).ok();
        nexus_user::close(tile.process).ok();
    }
    nexus_user::close(set).ok();

    if composited == 0 {
        failed("compositor: FAILED: no client ever drew anything");
        return;
    }

    // Unless the desktop emptied first and it was said there. Once either way:
    // a summary printed twice is two sets of numbers for one session, and
    // whoever reads the log has to work out which is which.
    if !summarised {
        summarise(screen, composited, batched);
    }
    if moved > 0 {
        nexus_user::log("compositor: moved a pointer of its own across the display").ok();
    }
    if forwarded > 0 {
        nexus_user::log("compositor: routed keys to the client that had focus").ok();
    }
    if commanded > 0 {
        nexus_user::log("compositor: did what the desktop asked of a window").ok();
    }
    if opened > 0 {
        nexus_user::log("compositor: started a program because the desktop asked").ok();
    }
    if painted > 0 {
        nexus_user::log(&alloc::format!(
            "compositor: composited {painted} frames of wallpaper from a program it does not read"
        ))
        .ok();
    }
    nexus_user::log("compositor: the session ended").ok();
}

/// Say what compositing cost, in the only terms that mean anything.
///
/// How many pixels were actually repainted, against how many repainting the
/// whole rectangle every time would have cost. That ratio is the whole case for
/// damage tracking, and a number nobody prints is a number nobody checks.
fn summarise(screen: &Screen, composited: u32, batched: u32) {
    let repaints = REPAINTS.load(core::sync::atomic::Ordering::Relaxed);
    let damaged = DAMAGED.load(core::sync::atomic::Ordering::Relaxed);
    let whole = u64::from(screen.width) * u64::from(screen.height) * repaints.max(1);
    nexus_user::log(&alloc::format!(
        "compositor: {repaints} repaints covered {damaged} pixels of a possible {whole},          {} written",
        clip::written()
    ))
    .ok();
    nexus_user::log(&alloc::format!(
        "compositor: {batched} composites for {composited} things that changed"
    ))
    .ok();
    nexus_user::log("compositor: composited every frame its clients drew").ok();
}

/// Which program a new window is for.
///
/// Not a path, because the caller is the desktop and the desktop must not be
/// able to name a program: a dock that could ask for any executable on the disk
/// would be a dock that can run anything, through a compositor that would hand
/// it the network.
#[derive(Clone, Copy, PartialEq, Eq)]
enum What {
    /// The demonstration client.
    Client,
    /// The browser, which is given the network.
    Browser,
    /// The terminal, which is given the filesystem and a way to start programs.
    Terminal,
    /// The settings window, which is given the settings directory to write.
    Settings,
    /// The package window, which is given both the filesystem and the record of
    /// what is installed.
    Packages,
    /// The picture window, which is given the filesystem to read from.
    Pictures,
    /// The agent, which is given the filesystem to read and the machine
    /// snapshot, and nothing that can change anything.
    Assistant,
}

/// What reading from the keyboard turned out to be.
enum Typed {
    /// Nothing a client has to see: a language change, a focus move, a key for
    /// a window that has gone.
    Nothing,
    /// Sent on to whoever has focus.
    Forwarded,
    /// The key that ends the session.
    Leave,
}

/// What reading from the desktop turned out to be.
enum Asked {
    /// Nothing this program has to act on.
    Nothing,
    /// It asked for the session to end.
    Ended,
    /// It drew its strip.
    Drew,
    /// It asked for something and a window changed.
    Changed,
    /// It asked for a program and one was started.
    Opened,
}

/// Tell the desktop which windows exist and what state each is in.
///
/// Sent on every change rather than asked for, because the desktop has no way
/// to ask: it holds one channel and no handle to a window. It is also what
/// keeps the strip honest -- a dock that drew what it last *asked* for would
/// show a window it wanted raised as raised whether or not it was.
fn announce(
    shell: &mut Option<Tile>,
    tiles: &[Option<Tile>; CLIENTS],
    focus: usize,
    last: &mut [u8; CLIENTS],
) {
    let mut states = [desk::GONE; CLIENTS];
    for (index, tile) in tiles.iter().enumerate() {
        states[index] = match tile {
            Some(tile) => tile.state(index == focus),
            None => desk::GONE,
        };
    }
    // Only when it has changed. Every pointer movement passes through here, and
    // a desktop told the same thing sixty times a second would redraw its strip
    // sixty times a second to show something that did not move -- which is the
    // cost this whole arrangement is supposed to avoid, moved one process along.
    if states == *last {
        return;
    }
    *last = states;

    let Some(shell) = shell else { return };
    let mut message = [0u8; desk::WINDOWS.len() + CLIENTS];
    message[..desk::WINDOWS.len()].copy_from_slice(desk::WINDOWS);
    message[desk::WINDOWS.len()..].copy_from_slice(&states);
    // A failure means the desktop has gone, which its process key will say.
    nexus_user::send(shell.channel, &message, &[]).ok();
}

/// Read whatever the desktop said, and do it.
///
/// This is the one place a program other than this one decides what happens to
/// a window. It is deliberately narrow: three commands, each naming a slot this
/// program already has, and every one of them checked here. The desktop cannot
/// name a window that does not exist, cannot reach one it was not told about,
/// and cannot ask for anything but these three things.
fn read_shell(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    shell: &mut Option<Tile>,
    focus: &mut usize,
    order: &mut [usize; CLIENTS],
) -> Option<Asked> {
    let Some(desktop) = shell else {
        return Some(Asked::Nothing);
    };

    let mut message = [0u8; 32];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(desktop.channel, &mut message, &mut none) else {
        nexus_user::unwatch(set, KEY_SHELL).ok();
        return Some(Asked::Nothing);
    };
    let message = &message[..received.bytes];

    if message == b"damaged" {
        desktop.frames += 1;
        // Answered, so the desktop knows its strip is free again. The same
        // handshake every client gets, for the same reason: without it the
        // strip would be redrawn while this program was still reading it.
        if nexus_user::send(desktop.channel, b"shown", &[]).is_err() {
            nexus_user::unwatch(set, KEY_SHELL).ok();
        }
        return Some(Asked::Drew);
    }

    if message == desk::OPEN {
        return open_window(screen, set, tiles, focus, order, What::Client);
    }

    if message == desk::BROWSE {
        return open_window(screen, set, tiles, focus, order, What::Browser);
    }

    if message == desk::TERMINAL {
        return open_window(screen, set, tiles, focus, order, What::Terminal);
    }

    if message == desk::SETTINGS {
        return open_window(screen, set, tiles, focus, order, What::Settings);
    }

    if message == desk::PACKAGES {
        return open_window(screen, set, tiles, focus, order, What::Packages);
    }

    if message == desk::PICTURES {
        return open_window(screen, set, tiles, focus, order, What::Pictures);
    }

    if message == desk::ASSIST {
        return open_window(screen, set, tiles, focus, order, What::Assistant);
    }

    if message == desk::QUIT {
        nexus_user::log("compositor: the desktop asked to end the session").ok();
        return Some(Asked::Ended);
    }

    if message.len() >= 8 && (message.starts_with(desk::SHOW) || message.starts_with(desk::HIDE)) {
        let slot = read_u32(message, 4) as usize;
        let show = message.starts_with(desk::SHOW);
        // Checked rather than trusted. The desktop is a program like any other,
        // and one that named a slot out of range would be indexing this
        // program's array from outside it.
        let Some(Some(tile)) = tiles.get_mut(slot) else {
            return Some(Asked::Nothing);
        };
        if !tile.live {
            return Some(Asked::Nothing);
        }
        tile.minimised = !show;
        if show {
            raise(order, slot);
            *focus = slot;
            nexus_user::log("compositor: brought a window back because the desktop asked").ok();
        } else {
            nexus_user::log("compositor: put a window away because the desktop asked").ok();
        }
        return Some(Asked::Changed);
    }

    Some(Asked::Nothing)
}

/// Start a program into an empty slot, because the desktop asked.
///
/// A slot whose client has ended is not reused: its surface is still mapped
/// where the new one would go, and unmapping it to make room is work with
/// nothing behind it while there are slots that were never used at all.
fn open_window(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    focus: &mut usize,
    order: &mut [usize; CLIENTS],
    what: What,
) -> Option<Asked> {
    let Some(slot) = tiles.iter().position(Option::is_none) else {
        nexus_user::log("compositor: the desktop asked for a window and there was no room").ok();
        return Some(Asked::Nothing);
    };

    // Offset from the ones before it, so a new window is visibly a new window
    // and not one exactly covering another.
    let step = slot as u32 * 12;
    let width = (screen.width / 2).min(screen.width.saturating_sub(step + GAP * 2));
    let height =
        (screen.usable_height() / 2).min(screen.usable_height().saturating_sub(step + GAP * 2));
    if width < MIN_SIZE || height < MIN_SIZE {
        return Some(Asked::Nothing);
    }

    // Reported by `start_client` if it fails; a compositor that stopped because
    // a program would not start would be a compositor a missing file could take
    // the screen away with.
    let tile = match what {
        What::Client => start_client(
            slot,
            screen.x + GAP + step,
            screen.y + GAP + step,
            width,
            height,
        )?,
        What::Browser => {
            // The network goes with the surface, in the same message. Read and
            // write and transfer, because it has to ask and be answered and be
            // given the handle at all -- and not close, so that a browser
            // cannot take the network away from the program that lent it.
            let Ok(theirs) = nexus_user::duplicate(
                NETWORK,
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
            ) else {
                failed("compositor: FAILED: could not lend the network to a browser");
                return Some(Asked::Nothing);
            };
            start_program(
                BROWSER,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[theirs],
            )?
        }
        What::Terminal => {
            // The filesystem and a way to start programs. Read, write and
            // transfer on the first; not close, so a terminal cannot take the
            // disk away from the program that lent it.
            let lending =
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER;
            // And the network, because a shell is where network tools live on
            // every system anybody has used. It is the widest thing a terminal
            // is given and it is worth naming: a shell with the disk, the
            // spawner and the network can do most of what this machine can do,
            // which is what a shell is for -- and it is still a decision made
            // here, by the program that holds those handles, rather than
            // something the terminal could have helped itself to.
            let (Ok(files), Ok(spawner), Ok(sound), Ok(machine), Ok(network)) = (
                nexus_user::duplicate(FILESYSTEM, lending),
                nexus_user::duplicate(SPAWNER, lending),
                nexus_user::duplicate(SOUND, lending),
                nexus_user::duplicate(MACHINE, lending),
                nexus_user::duplicate(NETWORK, lending),
            ) else {
                failed("compositor: FAILED: could not lend a terminal what it needs");
                return Some(Asked::Nothing);
            };
            start_program(
                TERMINAL,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[files, spawner, sound, machine, network],
            )?
        }
        What::Settings => {
            // The settings directory, and this time with write on it. The
            // wizard is the only other program that gets it that way, and for
            // the same reason: changing the machine is what it is for. Not
            // close, so that a settings window cannot take the directory away
            // from the compositor that lent it.
            let Ok(theirs) = nexus_user::duplicate(
                SETTINGS,
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
            ) else {
                failed("compositor: FAILED: could not lend the settings to a settings window");
                return Some(Asked::Nothing);
            };
            start_program(
                SETTINGS_WINDOW,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[theirs],
            )?
        }
        What::Packages => {
            // The filesystem, because installing writes files, and the settings
            // directory, because the record of what is installed lives there
            // and an install that did not update it would be an install the
            // updater would do again. Two handles and no spawner: this window
            // installs software and cannot start any.
            let lending =
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER;
            let (Ok(files), Ok(record)) = (
                nexus_user::duplicate(FILESYSTEM, lending),
                nexus_user::duplicate(SETTINGS, lending),
            ) else {
                failed("compositor: FAILED: could not lend a package window what it needs");
                return Some(Asked::Nothing);
            };
            start_program(
                STORE,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[files, record],
            )?
        }
        What::Pictures => {
            // The filesystem, and read-only: a picture viewer that could write
            // is a picture viewer that can delete a photograph. Transfer as
            // well, because that is the right a handle needs to cross a channel
            // at all.
            let Ok(files) = nexus_user::duplicate(
                FILESYSTEM,
                nexus_user::rights::READ | nexus_user::rights::TRANSFER,
            ) else {
                failed("compositor: FAILED: could not lend the disk to a picture window");
                return Some(Asked::Nothing);
            };
            start_program(
                VIEWER,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[files],
            )?
        }
        What::Assistant => {
            // The narrowest set anything here is given, and deliberately so.
            // An agent decides for itself what to do, which is what makes it an
            // agent and what makes the question "what can it do to my machine"
            // worth answering exactly: it can read files in the directory it
            // was handed, and it can ask how busy the machine is.
            //
            // Not write. Not the spawner. Not the network. Everything else it
            // might be asked to do is refused by name, with the permission it
            // would have needed, which is more useful than being unable to ask.
            let reading = nexus_user::rights::READ | nexus_user::rights::TRANSFER;
            let talking =
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER;
            let (Ok(files), Ok(machine)) = (
                nexus_user::duplicate(FILESYSTEM, reading),
                nexus_user::duplicate(MACHINE, talking),
            ) else {
                failed("compositor: FAILED: could not lend the agent what it may have");
                return Some(Asked::Nothing);
            };
            start_program(
                ASSISTANT,
                slot,
                screen.x + GAP + step,
                screen.y + GAP + step,
                width,
                height,
                0,
                &[files, machine],
            )?
        }
    };
    if nexus_user::watch(set, tile.channel, channel_key(slot)).is_err()
        || nexus_user::watch(set, tile.process, process_key(slot)).is_err()
    {
        failed("compositor: FAILED: could not watch a window it had just started");
        return None;
    }
    tiles[slot] = Some(tile);
    raise(order, slot);
    *focus = slot;
    nexus_user::log(&alloc::format!(
        "{} in slot {slot}, {width}x{height}",
        match what {
            What::Client => "compositor: started a window because someone pressed the desktop",
            What::Browser => "compositor: started a browser, and lent it the network",
            What::Terminal => {
                "compositor: started a terminal, and lent it the filesystem and the network"
            }
            What::Settings => "compositor: started the settings, and lent them the settings",
            What::Packages => "compositor: started the packages, and lent them the disk",
            What::Pictures => "compositor: started a picture window, and lent it the disk to read",
            What::Assistant => {
                "compositor: started the agent, and lent it the disk to read and nothing to write"
            }
        }
    ))
    .ok();
    Some(Asked::Opened)
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
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    shell: &mut Option<Tile>,
    focus: &mut usize,
) -> Option<Typed> {
    let mut message = [0u8; 32];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(KEYS, &mut message, &mut none) else {
        // The kernel has stopped sending. Not a failure: it means there is no
        // keyboard any more, and there is still a screen to composite.
        nexus_user::unwatch(set, KEY_KEYBOARD).ok();
        return Some(Typed::Nothing);
    };
    if received.bytes < key::SIZE {
        failed("compositor: FAILED: a key arrived in the wrong shape");
        return None;
    }

    // Not a key at all: the kernel saying what the interface language is now.
    // It goes to the desktop, which draws words, and to nobody else -- a client
    // drawing a gradient has nothing to do with it, and forwarding it as though
    // it were a keystroke would put a byte nobody expects into every window.
    if message[0] == key::LANGUAGE {
        remember_language(&message[..key::SIZE]);
        if let Some(shell) = shell {
            let mut forward = [0u8; desk::LANGUAGE.len() + 4];
            forward[..desk::LANGUAGE.len()].copy_from_slice(desk::LANGUAGE);
            forward[desk::LANGUAGE.len()..].copy_from_slice(&message[1..5]);
            nexus_user::send(shell.channel, &forward, &[]).ok();
        }
        return Some(Typed::Nothing);
    }

    // The way out from a keyboard.
    //
    // A machine with no pointer still has to be able to finish, and a session
    // that could only be ended by clicking is a session somebody with a broken
    // mouse cannot leave. Not forwarded to whoever has focus: it is addressed
    // to the session, and a client that saw it would be a client that could be
    // confused into thinking it was typed at it.
    if message[0] == key::FUNCTION && read_u32(&message, 1) == LEAVE_KEY {
        nexus_user::log("compositor: somebody asked to end the session").ok();
        return Some(Typed::Leave);
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
        return Some(Typed::Nothing);
    }

    let Some(Some(tile)) = tiles.get_mut(*focus) else {
        return Some(Typed::Nothing);
    };
    if !tile.live {
        return Some(Typed::Nothing);
    }
    if nexus_user::send(tile.channel, &message[..key::SIZE], &[]).is_err() {
        // It has gone. Ordinary, and the process key will say so.
        return Some(Typed::Nothing);
    }
    tile.keys += 1;
    Some(Typed::Forwarded)
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
/// Returns what changed on screen, or `None` to stop. The pointer's own
/// rectangle is not included: every repaint covers that anyway, because the
/// pixels underneath it have to be repainted before it is drawn again.
fn read_pointer(
    screen: &Screen,
    set: Handle,
    tiles: &mut [Option<Tile>; CLIENTS],
    shell: &mut Option<Tile>,
    cursor: &mut Pointer,
    focus: &mut usize,
    order: &mut [usize; CLIENTS],
) -> Option<Region> {
    let mut message = [0u8; 32];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(POINTER, &mut message, &mut none) else {
        // The kernel has stopped sending; there is still a screen to composite.
        nexus_user::unwatch(set, KEY_POINTER).ok();
        return Some(Region::nothing());
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
    let put_away = buttons & pointer::RIGHT != 0;
    // Nothing, until something is found to have changed. A bare movement is by
    // far the commonest thing a mouse sends and it changes no window at all.
    let mut damage = Region::nothing();

    // The right button on a title bar puts a window away. Checked before the
    // left button's business, because the two are different acts and a window
    // being minimised is not a window being raised.
    if put_away && !cursor.other_held {
        for &index in order.iter().rev() {
            let Some(Some(tile)) = tiles.get_mut(index) else {
                continue;
            };
            if tile.live && !tile.minimised && tile.title_contains(cursor.x, cursor.y) {
                tile.minimised = true;
                cursor.dragging = None;
                // Where it was, so that what was behind it comes back.
                damage = damage.union(region_of(tile));
                nexus_user::log("compositor: put a window away, leaving its tab").ok();
                break;
            }
        }
    }
    cursor.other_held = put_away;

    // A press in the strip belongs to the desktop, and this program does not
    // decide what it means. It passes on where the press landed, in coordinates
    // inside the strip -- because that is the only rectangle the desktop knows
    // about, and telling it a screen position would be telling it where its own
    // surface is, which is exactly what a client must not be able to learn.
    //
    // Checked before the windows are: windows are clamped above the strip, so
    // nothing else can be there.
    if pressed && !cursor.held && cursor.y >= screen.taskbar_y() {
        cursor.held = true;
        if let Some(shell) = shell {
            let mut message = [0u8; desk::CLICK.len() + 8];
            message[..desk::CLICK.len()].copy_from_slice(desk::CLICK);
            let local_x = cursor.x - screen.x;
            let local_y = cursor.y - screen.taskbar_y();
            message[3..7].copy_from_slice(&local_x.to_le_bytes());
            message[7..11].copy_from_slice(&local_y.to_le_bytes());
            if nexus_user::send(shell.channel, &message, &[]).is_ok() {
                nexus_user::log("compositor: a press in the strip went to the desktop").ok();
            }
        }
        // Nothing yet: what a press in the strip changes is decided by the
        // desktop, and arrives as a command.
        return Some(Region::nothing());
    }

    if pressed && !cursor.held {
        // Front to back, so a press lands on what is visible rather than on
        // whatever happens to be first in the array. A compositor that searched
        // the other way would give focus to the window *under* the one that was
        // clicked, which looks like the click going through it.
        for &index in order.iter().rev() {
            let Some(tile) = tiles.get(index).and_then(Option::as_ref) else {
                continue;
            };
            if !tile.live || tile.minimised || !tile.contains(cursor.x, cursor.y) {
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

            // Raising changes what covers what, and the ring moves off
            // whatever had it. Both are everything.
            damage = everything(screen);
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
                    let moved_y = clamp(
                        wanted_y,
                        screen.y,
                        screen.taskbar_y().saturating_sub(tile.height),
                    );
                    if moved_x != tile.x || moved_y != tile.y {
                        // Where it was and where it went. Not the whole screen:
                        // a window being dragged across a display is the one
                        // case where the difference is worth having.
                        damage = damage.union(region_of(tile));
                        tile.x = moved_x;
                        tile.y = moved_y;
                        damage = damage.union(region_of(tile));
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
                        screen.taskbar_y().saturating_sub(tile.y),
                    );

                    if wanted_width != tile.width || wanted_height != tile.height {
                        let before = region_of(tile);
                        match resize(index, tile, wanted_width, wanted_height) {
                            Ok(()) => {
                                damage = damage.union(before).union(region_of(tile));
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
    Some(damage)
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
#[allow(clippy::too_many_arguments)]
fn repaint(
    screen: &Screen,
    tiles: &[Option<Tile>; CLIENTS],
    shell: &Option<Tile>,
    wallpaper: &Option<Tile>,
    order: &[usize; CLIENTS],
    focus: usize,
    cursor: &mut Pointer,
    pointing: bool,
    damage: Region,
) {
    // Everything still happens -- the background, every window back to front,
    // the decorations, the strip -- and every one of them is clipped to the
    // damage. That is what makes this correct rather than merely fast: a
    // compositor that repainted only the window that changed would leave a hole
    // in whatever was above it, and getting *that* right needs the arithmetic
    // this deliberately does not have. Drawing the whole scene into a small
    // rectangle costs the rectangle, not the scene.
    //
    // The pointer's rectangle is always part of the damage, so the pixels it
    // covers are freshly painted before it is drawn again. Without that, the
    // saved pixels underneath it would be saved from a framebuffer that already
    // had a pointer in it, and the pointer would smear.
    let damage = damage.union(Region::of(cursor.x, cursor.y, CURSOR as u32, CURSOR as u32));
    let damage = damage.union(Region::nothing()); // keep the bounds sane
    if damage.is_empty() {
        return;
    }
    clip::set(damage);
    REPAINTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    DAMAGED.fetch_add(
        u64::from(damage.area()),
        core::sync::atomic::Ordering::Relaxed,
    );

    // The saved pixels are from before this repaint and mean nothing after it,
    // so the cursor is forgotten rather than restored.
    cursor.drawn = None;

    // The background. A colour underneath in case the wallpaper has not drawn
    // yet, and then the wallpaper over it -- which is one program's surface
    // composited exactly as a window's is, by a compositor that has no idea
    // what is in it.
    fill(
        screen,
        screen.x,
        screen.y,
        screen.width,
        screen.height,
        BACKGROUND,
    );
    if let Some(wallpaper) = wallpaper {
        composite(screen, wallpaper);
    }

    for &index in order {
        let Some(tile) = tiles.get(index).and_then(Option::as_ref) else {
            continue;
        };
        if !tile.live || tile.minimised {
            continue;
        }
        composite(screen, tile);
        title_bar(screen, tile, index == focus);
        grip(screen, tile, index == focus);
        outline(screen, tile, index == focus);
    }

    // The strip, last of the windows and before the pointer. Composited from
    // the desktop's surface, exactly as a window is: this program does not know
    // what is drawn in there, and the moment it did it would be a compositor
    // deciding what a dock looks like.
    match shell {
        Some(shell) => composite(screen, shell),
        // Nothing has it. Filled rather than left as it was, because the strip
        // is reserved space and the last thing drawn in it is not the truth
        // about a machine whose desktop has ended.
        None => fill(
            screen,
            screen.x,
            screen.taskbar_y(),
            screen.width,
            TASKBAR,
            0x0006_0D1A,
        ),
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
    let colour = if focused { accent(220) } else { accent(90) };
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
    // Clipped here rather than at every call site. Every decoration this
    // program draws goes through this function, so one comparison here is what
    // makes the whole of a repaint respect the damage rectangle.
    let (left, top, right, bottom) = clip::current();
    let x0 = x.max(left);
    let y0 = y.max(top);
    let x1 = (x + width).min(right);
    let y1 = (y + height).min(bottom);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    for row in y0..y1 {
        let destination =
            FRAMEBUFFER_AT + ((row as usize * screen.stride as usize) + x0 as usize) * 4;
        for column in 0..(x1 - x0) as usize {
            // SAFETY: the framebuffer is mapped writable, and the rectangle was
            // checked against the screen when the display was taken.
            unsafe {
                core::ptr::write_volatile((destination + column * 4) as *mut u32, colour);
            }
        }
    }
    clip::wrote(u64::from(x1 - x0) * u64::from(y1 - y0));
}

/// Draw a window's title bar.
///
/// A decoration: over the client's own pixels, after its surface has been
/// copied out, so a client can neither draw one nor remove the one it has. It
/// is also what there is to take hold of -- a window with no bar is a window
/// that cannot be picked up without picking up whatever is inside it.
fn title_bar(screen: &Screen, tile: &Tile, focused: bool) {
    let colour = if focused { accent(150) } else { accent(52) };
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
    let colour = if focused { accent(255) } else { accent(60) };

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
    // The same clip, applied to the copy. What is not damaged is not copied,
    // which is where nearly all of the saving is: a window is tens of thousands
    // of pixels and the thing that changed is usually one of them.
    let (left, top, right, bottom) = clip::current();
    let x0 = tile.x.max(left);
    let y0 = tile.y.max(top);
    let x1 = (tile.x + tile.width).min(right);
    let y1 = (tile.y + tile.height).min(bottom);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    for row in y0..y1 {
        let source = tile.mapped_at
            + (((row - tile.y) as usize * tile.width as usize) + (x0 - tile.x) as usize) * 4;
        let destination =
            FRAMEBUFFER_AT + ((row as usize * screen.stride as usize) + x0 as usize) * 4;

        for column in 0..(x1 - x0) as usize {
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
    clip::wrote(u64::from(x1 - x0) * u64::from(y1 - y0));
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
