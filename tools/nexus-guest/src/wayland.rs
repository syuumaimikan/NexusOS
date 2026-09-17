//! The Wayland wire protocol, as it is actually on the socket.
//!
//! Wayland is two things: a wire format, and a set of interfaces described in
//! XML that everybody generates bindings from. This is the first of those, by
//! hand, and only as much of the second as it takes to get a picture onto a
//! screen.
//!
//! # The format
//!
//! Every message is a header and some arguments, and the header is two
//! thirty-two bit words:
//!
//! ```text
//!   object id  |  (total size in bytes) << 16  |  opcode
//! ```
//!
//! The size includes the header, and messages are padded to four bytes. There
//! is no framing beyond that: a reader that miscounts one message reads the
//! next one's header as arguments and never recovers, which is why the size is
//! in the header rather than at the end.
//!
//! Arguments are: `int` and `uint`, a word each; `object` and `new_id`, a word
//! each, being an object identifier; `string`, a length *including* its
//! terminating NUL followed by that many bytes padded to four; `array`, the
//! same without the NUL; and `fd`, which is **not in the message at all** — it
//! travels beside it as a `SCM_RIGHTS` control message on the socket.
//!
//! That last one is why Wayland needs a Unix socket rather than any other kind
//! of connection, and why it could not be implemented here until descriptors
//! could be passed across one.
//!
//! # Who allocates identifiers
//!
//! The client, from 1 upwards; the server, from `0xFF000000` upwards. Object
//! one is the `wl_display` and exists before anything is sent. A client asks
//! for a new object by sending the identifier it has chosen for it, which is
//! why there is no round trip to create one — and is why a client that reuses
//! an identifier the server still holds gets a protocol error rather than a
//! second object.

use crate::{call, syscall3};

/// The identifier of `wl_display`, which exists before anything is sent.
pub const DISPLAY: u32 = 1;

/// The first identifier a server may allocate.
pub const SERVER_BASE: u32 = 0xFF00_0000;

/// The interfaces this implements, by name as they appear on the wire.
///
/// `xdg_wm_base` is not in `wayland.xml`. It is in `xdg-shell.xml`, which is an
/// extension -- and it is the extension without which a window is not a window:
/// core Wayland gives a client a surface, and says nothing about whether that
/// surface is a window, where it is, what it is called or how it is closed. A
/// real client binds `wl_compositor`, then looks for `xdg_wm_base`, and stops
/// if it is not there.
pub mod interface {
    pub const DISPLAY: &str = "wl_display";
    pub const REGISTRY: &str = "wl_registry";
    pub const CALLBACK: &str = "wl_callback";
    pub const COMPOSITOR: &str = "wl_compositor";
    pub const SHM: &str = "wl_shm";
    pub const SHM_POOL: &str = "wl_shm_pool";
    pub const SURFACE: &str = "wl_surface";
    pub const BUFFER: &str = "wl_buffer";
    pub const SEAT: &str = "wl_seat";
    pub const KEYBOARD: &str = "wl_keyboard";
    pub const OUTPUT: &str = "wl_output";
    pub const WM_BASE: &str = "xdg_wm_base";
    pub const XDG_SURFACE: &str = "xdg_surface";
    pub const TOPLEVEL: &str = "xdg_toplevel";
}

/// The requests a client sends, by interface and opcode.
///
/// The numbers are positions in the interface's request list, so an interface
/// that gained a request in the middle would renumber everything after it --
/// which is why Wayland never does, and why every one of these is safe to write
/// down as a constant.
pub mod request {
    pub mod display {
        pub const SYNC: u16 = 0;
        pub const GET_REGISTRY: u16 = 1;
    }
    pub mod registry {
        pub const BIND: u16 = 0;
    }
    pub mod compositor {
        pub const CREATE_SURFACE: u16 = 0;
        pub const CREATE_REGION: u16 = 1;
    }
    pub mod shm {
        pub const CREATE_POOL: u16 = 0;
    }
    pub mod shm_pool {
        pub const CREATE_BUFFER: u16 = 0;
        pub const DESTROY: u16 = 1;
        pub const RESIZE: u16 = 2;
    }
    pub mod surface {
        pub const DESTROY: u16 = 0;
        pub const ATTACH: u16 = 1;
        pub const DAMAGE: u16 = 2;
        pub const FRAME: u16 = 3;
        pub const SET_OPAQUE_REGION: u16 = 4;
        pub const SET_INPUT_REGION: u16 = 5;
        pub const COMMIT: u16 = 6;
        pub const SET_BUFFER_TRANSFORM: u16 = 7;
        pub const SET_BUFFER_SCALE: u16 = 8;
        pub const DAMAGE_BUFFER: u16 = 9;
    }
    pub mod buffer {
        pub const DESTROY: u16 = 0;
    }
    pub mod seat {
        pub const GET_POINTER: u16 = 0;
        pub const GET_KEYBOARD: u16 = 1;
        pub const GET_TOUCH: u16 = 2;
        pub const RELEASE: u16 = 3;
    }
    pub mod keyboard {
        pub const RELEASE: u16 = 0;
    }
    pub mod output {
        pub const RELEASE: u16 = 0;
    }
    /// `xdg_wm_base`, from `xdg-shell.xml`.
    pub mod wm_base {
        pub const DESTROY: u16 = 0;
        pub const CREATE_POSITIONER: u16 = 1;
        pub const GET_XDG_SURFACE: u16 = 2;
        pub const PONG: u16 = 3;
    }
    pub mod xdg_surface {
        pub const DESTROY: u16 = 0;
        pub const GET_TOPLEVEL: u16 = 1;
        pub const GET_POPUP: u16 = 2;
        pub const SET_WINDOW_GEOMETRY: u16 = 3;
        pub const ACK_CONFIGURE: u16 = 4;
    }
    pub mod toplevel {
        pub const DESTROY: u16 = 0;
        pub const SET_PARENT: u16 = 1;
        pub const SET_TITLE: u16 = 2;
        pub const SET_APP_ID: u16 = 3;
        pub const MOVE: u16 = 5;
        pub const RESIZE: u16 = 6;
        pub const SET_MAX_SIZE: u16 = 7;
        pub const SET_MIN_SIZE: u16 = 8;
        pub const SET_MAXIMIZED: u16 = 9;
        pub const UNSET_MAXIMIZED: u16 = 10;
        pub const SET_FULLSCREEN: u16 = 11;
        pub const UNSET_FULLSCREEN: u16 = 12;
        pub const SET_MINIMIZED: u16 = 13;
    }
}

/// The events a server sends.
pub mod event {
    pub mod display {
        pub const ERROR: u16 = 0;
        pub const DELETE_ID: u16 = 1;
    }
    pub mod registry {
        pub const GLOBAL: u16 = 0;
        pub const GLOBAL_REMOVE: u16 = 1;
    }
    pub mod callback {
        pub const DONE: u16 = 0;
    }
    pub mod shm {
        pub const FORMAT: u16 = 0;
    }
    pub mod buffer {
        pub const RELEASE: u16 = 0;
    }
    pub mod surface {
        pub const ENTER: u16 = 0;
        pub const LEAVE: u16 = 1;
    }
    pub mod seat {
        pub const CAPABILITIES: u16 = 0;
        pub const NAME: u16 = 1;
    }
    pub mod keyboard {
        pub const KEYMAP: u16 = 0;
        pub const ENTER: u16 = 1;
        pub const LEAVE: u16 = 2;
        pub const KEY: u16 = 3;
        pub const MODIFIERS: u16 = 4;
        pub const REPEAT_INFO: u16 = 5;
    }
    pub mod output {
        pub const GEOMETRY: u16 = 0;
        pub const MODE: u16 = 1;
        pub const DONE: u16 = 2;
        pub const SCALE: u16 = 3;
    }
    pub mod wm_base {
        pub const PING: u16 = 0;
    }
    pub mod xdg_surface {
        pub const CONFIGURE: u16 = 0;
    }
    pub mod toplevel {
        pub const CONFIGURE: u16 = 0;
        pub const CLOSE: u16 = 1;
    }
}

/// What a `wl_seat` says it has. A bitmask, so a seat may have several.
pub mod capability {
    pub const POINTER: u32 = 1;
    pub const KEYBOARD: u32 = 2;
    pub const TOUCH: u32 = 4;
}

/// How a `wl_keyboard.keymap` is described.
pub mod keymap {
    /// There is no keymap. The descriptor still travels with the event -- the
    /// argument is an `fd` and the wire format has no way to leave one out --
    /// but what is in it means nothing.
    pub const NONE: u32 = 0;
    /// An XKB keymap, as text. What every real compositor sends, and what this
    /// one cannot: it would need `xkbcommon`, or a hand-written keymap, and a
    /// wrong keymap is worse than an absent one.
    pub const XKB_V1: u32 = 1;
}

/// Whether a key went down or came up.
pub mod key_state {
    pub const RELEASED: u32 = 0;
    pub const PRESSED: u32 = 1;
}

/// `wl_output.mode` flags.
pub mod mode {
    pub const CURRENT: u32 = 1;
    pub const PREFERRED: u32 = 2;
}

/// `WL_SHM_FORMAT_XRGB8888`: thirty-two bits a pixel, blue in the low byte.
///
/// Zero would be `ARGB8888`. This one is what a client uses when it has no
/// transparency to offer, and is what the surface behind this machine's
/// compositor holds.
pub const XRGB8888: u32 = 1;

/// The most bytes one message may be.
///
/// A bound rather than a trust: the size comes out of the header, which comes
/// off the socket, and a reader that believed a size of four gigabytes would
/// try to hold one.
pub const MAX_MESSAGE: usize = 4096;

/// A message being built.
pub struct Out {
    bytes: [u8; MAX_MESSAGE],
    at: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

impl Out {
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: [0u8; MAX_MESSAGE],
            at: 0,
        }
    }

    /// Start a message to `object` with `opcode`.
    ///
    /// The size is left as zero and filled in by [`finish`](Self::finish),
    /// because it is not known until the arguments have been written.
    pub fn start(&mut self, object: u32, opcode: u16) {
        self.at = 0;
        self.word(object);
        self.word(u32::from(opcode));
    }

    /// One thirty-two bit argument: an `int`, a `uint`, an `object` or a
    /// `new_id`. All four are one word on the wire.
    pub fn word(&mut self, value: u32) {
        if self.at + 4 <= MAX_MESSAGE {
            self.bytes[self.at..self.at + 4].copy_from_slice(&value.to_le_bytes());
            self.at += 4;
        }
    }

    /// A string: its length including the terminating NUL, then the bytes,
    /// padded out to a multiple of four.
    pub fn text(&mut self, value: &str) {
        let bytes = value.as_bytes();
        self.word(bytes.len() as u32 + 1);
        for byte in bytes {
            if self.at < MAX_MESSAGE {
                self.bytes[self.at] = *byte;
                self.at += 1;
            }
        }
        if self.at < MAX_MESSAGE {
            self.bytes[self.at] = 0;
            self.at += 1;
        }
        while !self.at.is_multiple_of(4) && self.at < MAX_MESSAGE {
            self.bytes[self.at] = 0;
            self.at += 1;
        }
    }

    /// An array: its length in bytes, then the bytes, padded out to a
    /// multiple of four.
    ///
    /// Like a string without the terminating NUL, and the difference matters
    /// on the wire: a reader that treated one as the other would be one byte
    /// out for the rest of the message. `xdg_toplevel.configure` carries a
    /// list of states as an array of `u32`, and an empty one is the usual
    /// answer -- which is still four bytes, not nothing.
    pub fn array(&mut self, values: &[u32]) {
        self.word(values.len() as u32 * 4);
        for value in values {
            self.word(*value);
        }
    }

    /// Fill in the size and hand back the bytes to send.
    #[must_use]
    pub fn finish(&mut self) -> &[u8] {
        let size = self.at as u32;
        // The second word is the size and the opcode together, and the opcode
        // is already in its low half.
        let opcode =
            u32::from_le_bytes([self.bytes[4], self.bytes[5], self.bytes[6], self.bytes[7]])
                & 0xFFFF;
        self.bytes[4..8].copy_from_slice(&((size << 16) | opcode).to_le_bytes());
        &self.bytes[..self.at]
    }
}

/// One message read off the socket.
pub struct Message<'a> {
    pub object: u32,
    pub opcode: u16,
    /// The arguments, without the header.
    pub body: &'a [u8],
}

impl Message<'_> {
    /// The `index`th word of the body.
    #[must_use]
    pub fn word(&self, index: usize) -> u32 {
        let at = index * 4;
        if at + 4 > self.body.len() {
            return 0;
        }
        u32::from_le_bytes([
            self.body[at],
            self.body[at + 1],
            self.body[at + 2],
            self.body[at + 3],
        ])
    }

    /// The string starting at word `index`, without its terminating NUL.
    #[must_use]
    pub fn text(&self, index: usize) -> &str {
        let at = index * 4;
        if at + 4 > self.body.len() {
            return "";
        }
        let length = self.word(index) as usize;
        if length == 0 || at + 4 + length > self.body.len() {
            return "";
        }
        // The length includes the NUL, which is not part of the string.
        core::str::from_utf8(&self.body[at + 4..at + 4 + length - 1]).unwrap_or("")
    }
}

/// A connection, with somewhere to buffer what has arrived.
///
/// Wayland is a stream, so a read can return half a message or three and a
/// half. Anything not yet whole is kept here until the rest arrives -- which is
/// the part every hand-written implementation gets wrong first, because it
/// works perfectly until two messages happen to be sent close enough together
/// to arrive in one read.
pub struct Connection {
    pub socket: u64,
    held: [u8; MAX_MESSAGE * 4],
    length: usize,
    /// Descriptors that arrived with `SCM_RIGHTS`, in the order they came.
    pub descriptors: [i32; 8],
    pub descriptor_count: usize,
}

impl Connection {
    #[must_use]
    pub fn new(socket: u64) -> Self {
        Self {
            socket,
            held: [0u8; MAX_MESSAGE * 4],
            length: 0,
            descriptors: [-1; 8],
            descriptor_count: 0,
        }
    }

    /// Send a message.
    pub fn send(&self, bytes: &[u8]) -> i64 {
        syscall3(
            call::WRITE,
            self.socket,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        )
    }

    /// Take the next whole message out of what has already arrived.
    ///
    /// `None` when there is not a whole one yet, which is the caller's cue to
    /// read more rather than to give up.
    ///
    /// Not an `Iterator`: the item borrows the buffer it is taken from, which
    /// no iterator can express, and the caller has to stop borrowing before it
    /// can reply on the same connection.
    pub fn take(&mut self) -> Option<Message<'_>> {
        if self.length < 8 {
            return None;
        }
        let object = u32::from_le_bytes([self.held[0], self.held[1], self.held[2], self.held[3]]);
        let second = u32::from_le_bytes([self.held[4], self.held[5], self.held[6], self.held[7]]);
        let size = (second >> 16) as usize;
        let opcode = (second & 0xFFFF) as u16;
        if !(8..=MAX_MESSAGE).contains(&size) || self.length < size {
            if !(8..=MAX_MESSAGE).contains(&size) {
                // A size that cannot be right means the stream is no longer
                // understood. Everything held is dropped rather than resynced:
                // there is no way to find the next header in a stream with no
                // framing beyond the sizes.
                self.length = 0;
            }
            return None;
        }
        // Moved out of the buffer before the rest is shuffled down, so the
        // borrow the caller gets does not overlap what is about to move.
        self.held.copy_within(0..size, MAX_MESSAGE * 3);
        self.held.copy_within(size..self.length, 0);
        self.length -= size;
        let body = &self.held[MAX_MESSAGE * 3 + 8..MAX_MESSAGE * 3 + size];
        Some(Message {
            object,
            opcode,
            body,
        })
    }

    /// Read whatever has arrived, with any descriptors that came with it.
    ///
    /// Returns false when the connection has gone.
    pub fn fill(&mut self) -> bool {
        let room = MAX_MESSAGE * 3 - self.length;
        if room == 0 {
            // Four messages' worth held and none of them whole: the stream is
            // not what this understands.
            return false;
        }
        let mut control = [0u8; 64];
        let vector = [
            // SAFETY: a pointer into this structure, which the caller owns.
            unsafe { self.held.as_mut_ptr().add(self.length) } as u64,
            room as u64,
        ];
        let mut message = [0u64; 7];
        message[2] = vector.as_ptr() as u64;
        message[3] = 1;
        message[4] = control.as_mut_ptr() as u64;
        message[5] = control.len() as u64;

        let got = syscall3(call::RECVMSG, self.socket, message.as_mut_ptr() as u64, 0);
        if got < 0 {
            return false;
        }
        self.length += got as usize;

        // Any descriptors that came with it. The control message is a length, a
        // level, a type and then the descriptors -- and `recvmsg` wrote back
        // how many bytes of it there are.
        let control_length = message[5] as usize;
        if control_length >= 16 {
            let count = (control_length - 16) / 4;
            for index in 0..count.min(self.descriptors.len() - self.descriptor_count) {
                let at = 16 + index * 4;
                let mut four = [0u8; 4];
                four.copy_from_slice(&control[at..at + 4]);
                self.descriptors[self.descriptor_count] = i32::from_le_bytes(four);
                self.descriptor_count += 1;
            }
        }
        got > 0 || control_length > 0
    }

    /// Take the oldest descriptor that arrived, if any.
    pub fn take_descriptor(&mut self) -> Option<i32> {
        if self.descriptor_count == 0 {
            return None;
        }
        let first = self.descriptors[0];
        self.descriptors.copy_within(1..self.descriptor_count, 0);
        self.descriptor_count -= 1;
        Some(first)
    }
}

/// Send a message with one descriptor attached.
///
/// The descriptor is not in the message: it travels beside it, in a
/// `SCM_RIGHTS` control message, which is the whole reason Wayland is on a Unix
/// socket rather than any other kind of connection.
pub fn send_with_descriptor(socket: u64, bytes: &[u8], descriptor: i32) -> i64 {
    let vector = [bytes.as_ptr() as u64, bytes.len() as u64];
    // `struct cmsghdr`: length, level, type, then the descriptors.
    let mut control = [0u8; 24];
    control[0..8].copy_from_slice(&20u64.to_le_bytes());
    control[8..12].copy_from_slice(&1u32.to_le_bytes()); // SOL_SOCKET
    control[12..16].copy_from_slice(&1u32.to_le_bytes()); // SCM_RIGHTS
    control[16..20].copy_from_slice(&(descriptor as u32).to_le_bytes());

    let mut message = [0u64; 7];
    message[2] = vector.as_ptr() as u64;
    message[3] = 1;
    message[4] = control.as_ptr() as u64;
    message[5] = 20;
    syscall3(call::SENDMSG, socket, message.as_ptr() as u64, 0)
}
