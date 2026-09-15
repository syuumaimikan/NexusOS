//! The client half of the compositor's protocol.
//!
//! Every window on this machine does the same four things: take a surface from
//! the compositor, map it, draw a frame and say so, and wait to be told that the
//! frame has been shown or that a key was pressed or that the window is now a
//! different size. Before this crate each program did all four itself, which is
//! a hundred and twenty lines of handshake copied five times — and a handshake
//! copied five times is a handshake that is subtly different in one of them.
//!
//! # Where it sits
//!
//! Above [`nexus_user`], which is the system call boundary, and beside
//! [`nexus_ui`], which draws. It deliberately does not live in `nexus_ui`: that
//! crate says of itself that it knows nothing about windows, input or events,
//! and it should go on not knowing. Drawing is useful to something that has no
//! compositor at all — the kernel's own panel uses the same font — and a
//! drawing library that dragged in the window protocol would be a drawing
//! library nothing could use without one.
//!
//! # The handshake, and why there is one
//!
//! A frame is drawn, then `damaged` is sent, and nothing is drawn again until
//! `shown` comes back. Without the reply a program would redraw into a surface
//! the compositor was reading, which is what tearing is. The reply is also the
//! only flow control there is: a program that draws faster than the screen
//! refreshes spends its time waiting rather than filling memory with frames
//! nobody will see.
//!
//! # What a program provides
//!
//! [`App`]: how to draw, and what a key means. Everything else has a default,
//! including the two that most windows do not want — being woken on a clock,
//! and being woken by something other than the compositor.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use nexus_ui::Canvas;
use nexus_user::Handle;

/// What the compositor says, and what a program says back.
mod wire {
    /// The frame has been put on screen; the surface is the program's again.
    pub const SHOWN: &[u8] = b"shown";
    /// A new surface, and the size of it, follow.
    pub const RESIZED: &[u8] = b"size";
    /// This program has drawn.
    pub const DAMAGED: &[u8] = b"damaged";
}

/// The `damaged` message, with the rectangle on it when there is one.
///
/// A fixed buffer rather than a `Vec`: this is sent once a frame and a window
/// program has a small heap. Twenty-three bytes, on the stack.
fn damage_message(rectangle: Option<(u32, u32, u32, u32)>) -> [u8; wire::DAMAGED.len() + 16] {
    let mut out = [0u8; wire::DAMAGED.len() + 16];
    out[..wire::DAMAGED.len()].copy_from_slice(wire::DAMAGED);
    if let Some((x, y, width, height)) = rectangle {
        let at = wire::DAMAGED.len();
        out[at..at + 4].copy_from_slice(&x.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&y.to_le_bytes());
        out[at + 8..at + 12].copy_from_slice(&width.to_le_bytes());
        out[at + 12..at + 16].copy_from_slice(&height.to_le_bytes());
    }
    out
}

/// How much of that message to send.
///
/// The word alone when there is no rectangle, so that a compositor which has
/// never heard of rectangles sees exactly what it always saw.
const fn damage_length(rectangle: Option<(u32, u32, u32, u32)>) -> usize {
    if rectangle.is_some() {
        wire::DAMAGED.len() + 16
    } else {
        wire::DAMAGED.len()
    }
}

/// What a key is, as the kernel sends it and the compositor forwards it.
mod raw {
    pub const CHARACTER: u8 = 1;
    pub const BACKSPACE: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const ESCAPE: u8 = 4;
    pub const TAB: u8 = 5;
    pub const FUNCTION: u8 = 6;
    pub const LANGUAGE: u8 = 7;
    pub const MOVE: u8 = 8;

    /// A key message is one byte of kind and four of value.
    pub const SIZE: usize = 5;
}

/// Which way a movement key went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Movement {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

impl Movement {
    /// The movement this number means, as the keyboard driver writes it.
    const fn of(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Up),
            1 => Some(Self::Down),
            2 => Some(Self::Left),
            3 => Some(Self::Right),
            4 => Some(Self::PageUp),
            5 => Some(Self::PageDown),
            6 => Some(Self::Home),
            7 => Some(Self::End),
            _ => None,
        }
    }
}

/// A key, decoded.
///
/// A type rather than the two numbers, because the two numbers have been
/// decoded separately in every program that reads them, and the decoding is the
/// part that is easy to get subtly wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character was typed.
    Character(char),
    Backspace,
    Enter,
    Escape,
    Tab,
    /// A function key, by its number: `Function(2)` is F2.
    Function(u32),
    /// The interface language was changed. Given to the program because a
    /// program holding translated strings has to draw them again.
    Language,
    /// An arrow, a page key, home or end.
    Move(Movement),
}

impl Key {
    /// Read one out of a message, if it is one.
    #[must_use]
    pub fn of(message: &[u8]) -> Option<Self> {
        if message.len() < raw::SIZE {
            return None;
        }
        let value = u32::from_le_bytes([message[1], message[2], message[3], message[4]]);
        match message[0] {
            raw::CHARACTER => char::from_u32(value).map(Self::Character),
            raw::BACKSPACE => Some(Self::Backspace),
            raw::ENTER => Some(Self::Enter),
            raw::ESCAPE => Some(Self::Escape),
            raw::TAB => Some(Self::Tab),
            raw::FUNCTION => Some(Self::Function(value)),
            raw::LANGUAGE => Some(Self::Language),
            raw::MOVE => Movement::of(value).map(Self::Move),
            _ => None,
        }
    }
}

/// What a program does with a window.
///
/// Only [`draw`](App::draw) has to be written. A window that does nothing else
/// is a picture, which is a legitimate thing to be.
pub trait App {
    /// Put a frame together. Called only when the surface is the program's.
    fn draw(&mut self, canvas: &mut Canvas);

    /// Which part of the last frame actually changed.
    ///
    /// `None` -- the default -- means all of it, which is what every window
    /// meant before this existed and is the right answer for one that redraws
    /// its whole contents anyway.
    ///
    /// Returning a rectangle is what lets the compositor repaint a strip
    /// instead of a window. It matters most for the things that change a little
    /// and often: a caret, a clock, a progress bar, a frame of video. The
    /// wallpaper is the extreme case -- a full-screen composite four times a
    /// second was what capped an animated background at four frames.
    ///
    /// Called immediately after [`draw`](App::draw), so a program can record
    /// what it touched while it is touching it.
    ///
    /// # Being honest about it
    ///
    /// A rectangle smaller than what was drawn leaves the rest stale on screen
    /// until something else repaints it. The compositor does not check, and it
    /// could not: it has no idea what the program meant to draw. A window that
    /// is unsure should return `None` and cost a little more.
    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        None
    }

    /// Act on a key. Returns whether anything on screen changed.
    ///
    /// Returning `false` for a key that changed nothing is what keeps a window
    /// from drawing again for every keystroke it ignored.
    fn key(&mut self, key: Key) -> bool {
        let _ = key;
        false
    }

    /// The window is now this size. The surface has already been remapped.
    fn resized(&mut self, width: u32, height: u32) {
        let _ = (width, height);
    }

    /// How long to wait before being woken anyway, in milliseconds.
    ///
    /// `None` — the default — means wait until something happens, which is what
    /// a window that only reacts to keys wants. A clock is for the windows that
    /// change on their own, and asking for one when nothing changes is a
    /// program that costs a wake-up a second for ever.
    fn tick_ms(&mut self) -> Option<u64> {
        None
    }

    /// Woken by the clock, or by one of the extra handles being watched.
    /// Returns whether anything on screen changed.
    ///
    /// The simple form, for a window that watches nothing and only wants a
    /// clock. A program watching more than one handle should implement
    /// [`woken`](App::woken) instead and look at the key.
    fn ticked(&mut self) -> bool {
        false
    }

    /// The same, told which of its handles became ready.
    ///
    /// `None` is the clock. `Some(key)` is the key the handle was watched
    /// under, one call per ready handle -- because a program watching three
    /// services cannot safely find out which one replied by reading all three:
    /// `receive` blocks, and two of those reads would block for ever.
    ///
    /// Defaults to [`ticked`](App::ticked), so a window that does not care
    /// which it was needs neither.
    fn woken(&mut self, key: Option<u64>) -> bool {
        let _ = key;
        self.ticked()
    }

    /// Whether to carry on. Checked after every event.
    fn running(&self) -> bool {
        true
    }
}

/// What went wrong before there was a window to say it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The compositor sent nothing, or went away.
    NothingArrived,
    /// The first message had no surface in it.
    NoSurface,
    /// The surface would not map.
    WouldNotMap,
    /// The surface is smaller than the size that came with it.
    TooSmall,
    /// The size in the message cannot describe a surface at all.
    ImpossibleSize,
    /// A wait set could not be made, or something could not be watched.
    CouldNotWait,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let said = match self {
            Self::NothingArrived => "nothing arrived to draw on",
            Self::NoSurface => "no surface came with the message",
            Self::WouldNotMap => "the surface would not map",
            Self::TooSmall => "the surface is smaller than the size it was given",
            Self::ImpossibleSize => "the size that arrived cannot describe a surface",
            Self::CouldNotWait => "could not wait on the compositor",
        };
        out.write_str(said)
    }
}

/// Why the frame loop stopped, and how much it drew before it did.
///
/// A count of frames on its own cannot tell a window that was closed from one
/// whose compositor stopped answering, and those call for different reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub ended: Ended,
    pub frames: u32,
}

/// What ended a frame loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The program said it was done.
    Finished,
    /// The compositor's channel closed.
    Disconnected,
    /// A frame was sent and never acknowledged.
    ///
    /// Distinct from being disconnected on purpose: a compositor that is alive
    /// and has stopped answering leaves a client blocked for ever, and a
    /// program that reported that as an ordinary end would be a program hiding
    /// the one fault worth reporting.
    NotAcknowledged,
    /// A resize arrived and the new surface could not be mapped.
    LostSurface,
    /// The loop ran longer than its bound allows.
    OutOfPatience,
}

impl core::fmt::Display for Ended {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str(match self {
            Self::Finished => "the program finished",
            Self::Disconnected => "the compositor went away",
            Self::NotAcknowledged => "the compositor stopped acknowledging frames",
            Self::LostSurface => "a new surface could not be mapped",
            Self::OutOfPatience => "the frame loop ran out of turns",
        })
    }
}

/// A surface, and the channel it came down.
pub struct Window {
    compositor: Handle,
    surface: Handle,
    at: usize,
    width: u32,
    height: u32,
    /// Which window this is, as the compositor numbers them. Useful only for
    /// saying so in a log: a program cannot address another window with it.
    index: u32,
    /// A third number whose meaning is the compositor's, passed through.
    extra: u32,
    set: Handle,
}

/// The wait-set key for the compositor's channel.
const SAID: u64 = 1;

/// Where a program's own keys should start, so that they cannot collide with
/// the one this crate uses.
pub const FIRST_KEY: u64 = 16;

/// A frame loop gives up after this many turns. A bound on the whole life of a
/// window rather than on any one wait: it is the last line of defence, not the
/// timeout.
const PATIENCE: u64 = 8_000_000;

/// How long a frame may go unacknowledged before the compositor is treated as
/// having stopped answering.
///
/// The real timeout, and the one that matters. Ten seconds is far longer than
/// any composite takes and far shorter than a person will wait at a window that
/// has stopped redrawing.
const ACKNOWLEDGE_MS: u64 = 10_000;

/// How many ready keys one wait may report.
///
/// Sized for the compositor's own key plus every handle a program is allowed to
/// add, so that a wake never has to be split across two calls -- a ready key
/// that did not fit would be a service whose reply was silently not dispatched.
const READY: usize = 1 + MAX_WATCHED;

/// How many handles of its own a program may watch through this crate.
pub const MAX_WATCHED: usize = 15;

impl Window {
    /// Take the first message from the compositor: a size, a surface, and
    /// whatever else this program was lent.
    ///
    /// The extra handles arrive in the same message as the surface rather than
    /// after it, which is the compositor's decision and a good one: a program
    /// that had to read two messages would have to know there were two, and one
    /// started without the second would wait for ever for something nobody was
    /// going to send. `lent` is filled with whatever came after the surface, and
    /// how many of them there were is returned.
    ///
    /// `at` is where the surface is mapped, which is the program's own choice,
    /// as every mapping is.
    pub fn open(
        compositor: Handle,
        at: usize,
        lent: &mut [Handle],
    ) -> Result<(Self, usize), Trouble> {
        let mut buffer = [0u8; 32];
        let mut handles = [Handle(0); nexus_user::MAX_HANDLES];
        let received = nexus_user::receive(compositor, &mut buffer, &mut handles)
            .map_err(|_| Trouble::NothingArrived)?;
        if received.handles < 1 || received.bytes < 16 {
            return Err(Trouble::NoSurface);
        }

        let width = read_u32(&buffer, 0);
        let height = read_u32(&buffer, 4);
        let extra = read_u32(&buffer, 8);
        let index = read_u32(&buffer, 12);
        let surface = handles[0];

        if !plausible(width, height) {
            return Err(Trouble::ImpossibleSize);
        }
        let mapped = nexus_user::memory_map(surface, at, true).map_err(|_| Trouble::WouldNotMap)?;
        if !fits(width, height, mapped) {
            nexus_user::memory_unmap(surface, at).ok();
            return Err(Trouble::TooSmall);
        }

        let spare = received.handles - 1;
        let carried = spare.min(lent.len());
        lent[..carried].copy_from_slice(&handles[1..1 + carried]);
        // Anything that arrived beyond what the caller asked for is closed
        // rather than kept. A handle nobody named is authority this program was
        // given and cannot use, and holding it open would keep whatever it
        // refers to alive for as long as this window runs.
        for extra in &handles[1 + carried..received.handles] {
            nexus_user::close(*extra).ok();
        }

        let set = match nexus_user::wait_set() {
            Ok(set) => set,
            Err(_) => {
                nexus_user::memory_unmap(surface, at).ok();
                return Err(Trouble::CouldNotWait);
            }
        };
        if nexus_user::watch(set, compositor, SAID).is_err() {
            nexus_user::close(set).ok();
            nexus_user::memory_unmap(surface, at).ok();
            return Err(Trouble::CouldNotWait);
        }

        Ok((
            Self {
                compositor,
                surface,
                at,
                width,
                height,
                index,
                extra,
                set,
            },
            carried,
        ))
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Which window this is, as the compositor numbers them.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The third number from the opening message, whatever the compositor meant
    /// by it.
    #[must_use]
    pub const fn extra(&self) -> u32 {
        self.extra
    }

    /// Watch something else as well as the compositor: a service this program
    /// is waiting on, a process it started. Use a key at or above [`FIRST_KEY`].
    ///
    /// Anything that becomes ready arrives as [`App::ticked`], because from the
    /// frame loop's point of view the two are the same event: something
    /// happened, and the program may want to draw.
    /// Keys below [`FIRST_KEY`] are refused rather than passed on. The
    /// compositor's own membership lives down there, and a program that
    /// unwatched key 1 -- by arithmetic, or by counting from zero -- would stop
    /// its own window from ever being told anything again, with nothing to say
    /// what had happened.
    pub fn watch(&self, handle: Handle, key: u64) -> Result<(), nexus_user::Error> {
        if key < FIRST_KEY {
            return Err(nexus_user::Error::Invalid);
        }
        nexus_user::watch(self.set, handle, key)
    }

    /// Stop watching something.
    pub fn unwatch(&self, key: u64) -> Result<(), nexus_user::Error> {
        if key < FIRST_KEY {
            return Err(nexus_user::Error::Invalid);
        }
        nexus_user::unwatch(self.set, key)
    }

    /// A canvas over the surface.
    ///
    /// Called only from inside the frame loop, between a `shown` and the
    /// `damaged` that follows it, which is the stretch of time in which the
    /// surface belongs to this program and nothing else is reading it.
    fn canvas(&mut self) -> Canvas {
        // SAFETY: `at` is mapped writable for `width * height * 4` bytes --
        // checked when it was mapped and again on every resize -- and the
        // compositor is not reading it, which is what the handshake establishes.
        unsafe { Canvas::packed(self.at, self.width, self.height) }
    }

    /// Draw, say so, and act on what comes back, until the program is done or
    /// the compositor stops answering.
    ///
    /// Returns how many frames were drawn, which is the number worth logging:
    /// a window that drew nothing and a window that drew is the difference
    /// between a program that failed and one that ran.
    pub fn run(mut self, app: &mut impl App) -> Outcome {
        let mut stale = true;
        // When the frame in flight was sent, or `None` when none is.
        let mut sent_at: Option<u64> = None;
        let mut drawn = 0u32;
        let mut ended = Ended::OutOfPatience;

        for _ in 0..PATIENCE {
            if !app.running() {
                ended = Ended::Finished;
                break;
            }
            if stale && sent_at.is_none() {
                let mut canvas = self.canvas();
                app.draw(&mut canvas);
                // Asked after drawing, so a program can note what it touched
                // as it touches it rather than predicting it beforehand.
                let rectangle = app.damage();
                let message = damage_message(rectangle);
                let length = damage_length(rectangle);
                if nexus_user::send(self.compositor, &message[..length], &[]).is_err() {
                    ended = Ended::Disconnected;
                    break;
                }
                drawn += 1;
                stale = false;
                sent_at = Some(nexus_user::uptime());
            }

            // The shorter of what the program asked for and what is left of the
            // frame's patience. Without the second the loop would wait for ever
            // on a compositor that is alive and has stopped answering, which is
            // the one failure a window cannot report from inside itself.
            let ticked = app.tick_ms();
            let remaining = sent_at
                .map(|at| ACKNOWLEDGE_MS.saturating_sub(nexus_user::uptime().saturating_sub(at)));
            let deadline = match (ticked, remaining) {
                (Some(one), Some(other)) => Some(one.min(other)),
                (Some(one), None) => Some(one),
                (None, Some(other)) => Some(other),
                (None, None) => None,
            };

            let mut keys = [0u64; READY];
            let waited = match deadline {
                Some(milliseconds) => nexus_user::wait_any_until(self.set, &mut keys, milliseconds),
                None => nexus_user::wait_any(self.set, &mut keys),
            };
            let Ok(ready) = waited else {
                ended = Ended::Disconnected;
                break;
            };
            let ready = ready.min(keys.len());

            // Everything that is ready, and not only the first thing.
            //
            // This used to hand the whole wake to the compositor whenever the
            // compositor was among the keys, and drop the rest. A window
            // waiting on a service therefore stopped hearing from it for as
            // long as somebody was typing -- the service was ready, the wake
            // said so, and the reply was thrown away. Both are dispatched now,
            // the program's own handles first, because what they carry is what
            // the next frame is going to draw.
            let mut said = false;
            for key in &keys[..ready] {
                if *key == SAID {
                    said = true;
                } else if app.woken(Some(*key)) {
                    stale = true;
                }
            }

            if ready == 0 {
                // The deadline. Either the program's clock or the frame's
                // patience, and which one it was is a question of whether a
                // frame has been waiting too long.
                if let Some(at) = sent_at {
                    if nexus_user::uptime().saturating_sub(at) >= ACKNOWLEDGE_MS {
                        ended = Ended::NotAcknowledged;
                        break;
                    }
                }
                if app.woken(None) {
                    stale = true;
                }
                continue;
            }

            if !said {
                continue;
            }

            match self.heard(app) {
                Heard::Shown => sent_at = None,
                Heard::Changed => stale = true,
                Heard::Nothing => {}
                Heard::Gone => {
                    ended = Ended::Disconnected;
                    break;
                }
                Heard::LostSurface => {
                    ended = Ended::LostSurface;
                    break;
                }
            }
        }

        Outcome {
            ended,
            frames: drawn,
        }
    }

    /// Read one message from the compositor and act on it.
    fn heard(&mut self, app: &mut impl App) -> Heard {
        let mut message = [0u8; 64];
        let mut incoming = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(self.compositor, &mut message, &mut incoming) else {
            return Heard::Gone;
        };
        let bytes = &message[..received.bytes];

        if bytes == wire::SHOWN {
            return Heard::Shown;
        }

        if received.bytes >= 12 && bytes.starts_with(wire::RESIZED) && received.handles == 1 {
            // The old surface goes before the new one is mapped, because they
            // are mapped at the same address: a program that kept both would be
            // a program asking for two things in one place.
            nexus_user::memory_unmap(self.surface, self.at).ok();
            nexus_user::close(self.surface).ok();
            self.surface = incoming[0];
            let width = read_u32(bytes, 4);
            let height = read_u32(bytes, 8);
            if !plausible(width, height) {
                return Heard::LostSurface;
            }
            let Ok(mapped) = nexus_user::memory_map(self.surface, self.at, true) else {
                return Heard::LostSurface;
            };
            if !fits(width, height, mapped) {
                nexus_user::memory_unmap(self.surface, self.at).ok();
                return Heard::LostSurface;
            }
            self.width = width;
            self.height = height;
            app.resized(self.width, self.height);
            return Heard::Changed;
        }

        match Key::of(bytes) {
            Some(key) if app.key(key) => Heard::Changed,
            _ => Heard::Nothing,
        }
    }
}

impl Drop for Window {
    /// Give back what this window owns, and nothing else.
    ///
    /// The surface and the wait set are this crate's; the compositor's channel
    /// and whatever was lent to the program are not, and closing those would be
    /// closing somebody else's handle. `run` takes `self`, so this is also what
    /// tidies up after a window that has finished -- which matters the moment a
    /// program opens a second one.
    fn drop(&mut self) {
        nexus_user::memory_unmap(self.surface, self.at).ok();
        nexus_user::close(self.surface).ok();
        nexus_user::close(self.set).ok();
    }
}

/// What one message from the compositor turned out to be.
enum Heard {
    /// The frame is on screen.
    Shown,
    /// Something changed and the window wants drawing again.
    Changed,
    /// Nothing this program has to act on.
    Nothing,
    /// The compositor has gone.
    Gone,
    /// A resize arrived and the surface could not be replaced.
    LostSurface,
}

/// Whether a width and height could describe a surface at all.
///
/// Zero is not a window, and a dimension large enough to overflow the byte
/// count is a malformed message rather than a very large screen. Checked here
/// so that the arithmetic below cannot wrap: on a build without overflow checks
/// `width * height * 4` can come back small for enormous dimensions, and the
/// comparison that is supposed to guard an `unsafe` block would pass.
fn plausible(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && bytes_for(width, height).is_some()
}

/// How many bytes a surface of this size needs, if that is a number.
fn bytes_for(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
}

/// Whether a mapping of `mapped` bytes is big enough for this size.
fn fits(width: u32, height: u32, mapped: usize) -> bool {
    matches!(bytes_for(width, height), Some(needed) if needed <= mapped)
}

/// Read a little-endian `u32` out of a message.
fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buffer[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}
