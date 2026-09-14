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
    fn ticked(&mut self) -> bool {
        false
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
            Self::CouldNotWait => "could not wait on the compositor",
        };
        out.write_str(said)
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

/// A frame loop gives up after this many turns without being told anything. A
/// window whose compositor has stopped answering would otherwise spin.
const PATIENCE: u64 = 8_000_000;

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

        let mapped = nexus_user::memory_map(surface, at, true).map_err(|_| Trouble::WouldNotMap)?;
        if width as usize * height as usize * 4 > mapped {
            return Err(Trouble::TooSmall);
        }

        let spare = received.handles - 1;
        let carried = spare.min(lent.len());
        lent[..carried].copy_from_slice(&handles[1..1 + carried]);

        let set = nexus_user::wait_set().map_err(|_| Trouble::CouldNotWait)?;
        nexus_user::watch(set, compositor, SAID).map_err(|_| Trouble::CouldNotWait)?;

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
    pub fn watch(&self, handle: Handle, key: u64) -> Result<(), nexus_user::Error> {
        nexus_user::watch(self.set, handle, key)
    }

    /// Stop watching something.
    pub fn unwatch(&self, key: u64) -> Result<(), nexus_user::Error> {
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
    pub fn run(mut self, app: &mut impl App) -> u32 {
        let mut stale = true;
        let mut in_flight = false;
        let mut drawn = 0u32;

        for _ in 0..PATIENCE {
            if !app.running() {
                break;
            }
            if stale && !in_flight {
                let mut canvas = self.canvas();
                app.draw(&mut canvas);
                if nexus_user::send(self.compositor, wire::DAMAGED, &[]).is_err() {
                    break;
                }
                drawn += 1;
                stale = false;
                in_flight = true;
            }

            let mut keys = [0u64; 4];
            let waited = match app.tick_ms() {
                Some(milliseconds) => nexus_user::wait_any_until(self.set, &mut keys, milliseconds),
                None => nexus_user::wait_any(self.set, &mut keys),
            };
            let Ok(ready) = waited else {
                break;
            };

            // Anything that is not the compositor is this program's own
            // business: it is told that something happened and looks for
            // itself, because this crate has no idea what it was watching. A
            // wake with nothing ready -- the clock -- is the same question.
            if ready == 0 || !keys[..ready.min(keys.len())].contains(&SAID) {
                if app.ticked() {
                    stale = true;
                }
                continue;
            }

            match self.heard(app) {
                Heard::Shown => in_flight = false,
                Heard::Changed => stale = true,
                Heard::Nothing => {}
                Heard::Gone => break,
            }
        }

        drawn
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
            self.width = read_u32(bytes, 4);
            self.height = read_u32(bytes, 8);
            let Ok(mapped) = nexus_user::memory_map(self.surface, self.at, true) else {
                return Heard::Gone;
            };
            if self.width as usize * self.height as usize * 4 > mapped {
                return Heard::Gone;
            }
            app.resized(self.width, self.height);
            return Heard::Changed;
        }

        match Key::of(bytes) {
            Some(key) if app.key(key) => Heard::Changed,
            _ => Heard::Nothing,
        }
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
    /// The compositor has gone, or the surface could not be replaced.
    Gone,
}

/// Read a little-endian `u32` out of a message.
fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&buffer[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}
