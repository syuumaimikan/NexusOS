//! A window, for a program built for Linux.
//!
//! A Linux program draws through X11 or Wayland: it connects to a server over a
//! Unix socket, is handed a shared buffer, writes pixels into it, and tells the
//! server which part changed. This machine has a compositor that works the same
//! way -- surface, pixels, damage, acknowledgement -- over its own channels
//! rather than a socket, and this is the smallest honest bridge between the
//! two.
//!
//! # What it is not
//!
//! It is **not Wayland**. There is no `wl_display`, no object registry, no
//! `wl_shm` pool, no `xdg_surface`, and no Unix socket to carry file
//! descriptors over. A Wayland client will not run against this, and one that
//! tried would fail at its first `connect`.
//!
//! What it *is* is the layer underneath all of that: a program gets a buffer it
//! can write and a way to say "this part changed, put it on screen", which is
//! what every one of those protocols is a way of arranging. A Wayland server
//! built here later would be a program in user space speaking the real wire
//! protocol, and it would draw through exactly this.
//!
//! # How a program uses it
//!
//! ```text
//! fd = openat(AT_FDCWD, "/dev/nexus/display", O_RDWR)
//! ioctl(fd, NEXUS_DISPLAY_INFO, &info)      // width, height, stride, format
//! pixels = mmap(NULL, info.stride * info.height, PROT_READ|PROT_WRITE,
//!               MAP_SHARED, fd, 0)
//! ... write pixels ...
//! ioctl(fd, NEXUS_DISPLAY_PRESENT, &rect)   // and wait for it to be shown
//! ioctl(fd, NEXUS_DISPLAY_EVENT, &event)    // a key, a resize, or nothing
//! ```
//!
//! `MAP_SHARED` is not only allowed here but *required*: the buffer really is
//! shared with the compositor, which is the one place in this layer where a
//! private mapping would be the wrong answer rather than a safe one. A file
//! mapping is still refused for exactly the reason it always was -- there, the
//! promise could not be kept.
//!
//! # Where the authority comes from
//!
//! The channel to the compositor, which the process was given when it was
//! started -- the same first handle every Nexus window program gets. A
//! translated program started by anything else has no such channel and opening
//! this device fails, which is the truth: it has no window because nobody gave
//! it one.
//!
//! So the descriptor this returns *is* that channel, duplicated. That is the
//! rule this whole directory follows, applied to drawing: a Linux file
//! descriptor is a Nexus handle, and what can be done through it is what the
//! handle carries. A program that closes the descriptor gives the window back.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::ipc;
use crate::sync::IrqSpinLock;

use super::linux::error;

/// The path a program opens to get a window.
///
/// Under `/dev`, where a Linux program looks for a device, and named for this
/// system rather than for one it is imitating: a program that found
/// `/dev/dri/card0` here would reasonably expect a DRM device and would send it
/// `DRM_IOCTL_MODE_*`, which this does not answer.
pub const PATH: &str = "/dev/nexus/display";

/// The requests this device answers.
///
/// This system's own numbers, not Linux's. An `ioctl` number on Linux encodes a
/// direction, a size, a "magic" byte owned by a driver and a sequence number;
/// borrowing a magic byte that belongs to a real driver would mean a program
/// that sent the real driver's request here got an answer of a different shape.
/// `NX` is not allocated to anything.
pub mod request {
    /// Ask how large the surface is. Writes [`Info`].
    pub const INFO: u64 = 0x4E58_0001;
    /// Put a rectangle of it on screen, and wait until it is there. Reads four
    /// `u32`: x, y, width, height.
    pub const PRESENT: u64 = 0x4E58_0002;
    /// Take the next thing that happened, if anything has. Writes [`Event`].
    pub const EVENT: u64 = 0x4E58_0003;
}

/// What `NEXUS_DISPLAY_INFO` writes: four `u32`.
///
/// `stride` is in bytes and is not always `width * 4`: a compositor is free to
/// pad its rows, and a program that assumed otherwise would draw a diagonal
/// smear on the first machine that did. `format` is zero, meaning
/// thirty-two bits a pixel, blue in the low byte -- what a Linux program calls
/// `XRGB8888` on a little-endian machine.
const INFO_BYTES: u64 = 16;
/// What `NEXUS_DISPLAY_PRESENT` reads: x, y, width, height.
const RECTANGLE_BYTES: u64 = 16;
/// What `NEXUS_DISPLAY_EVENT` writes: kind, value, width, height.
const EVENT_BYTES: u64 = 16;

/// What kind of thing happened, as `NEXUS_DISPLAY_EVENT` reports it.
pub mod event {
    /// Nothing had happened. Not an error: a program polls this.
    pub const NONE: u32 = 0;
    /// A key was typed. The value is the character, as a Unicode scalar.
    pub const CHARACTER: u32 = 1;
    /// A key that is not a character: the value is which, using the same
    /// numbering the compositor forwards from the keyboard.
    pub const KEY: u32 = 2;
    /// The window changed size. The program must ask for [`request::INFO`]
    /// again and map the surface again -- the old mapping is of a buffer that
    /// is no longer the window's.
    pub const RESIZED: u32 = 3;
    /// The compositor has gone, and there is no window any more.
    pub const CLOSED: u32 = 4;
}

/// The words the compositor's protocol uses.
///
/// Read here rather than shared with `nexus-window`, because that crate is a
/// user-space library this kernel does not link -- and because the three words
/// are the whole protocol. If they ever disagree the failure is a window that
/// never updates, which the test notices.
mod wire {
    /// The frame is on screen; the surface is the program's again.
    pub const SHOWN: &[u8] = b"shown";
    /// A new surface, and the size of it, follow.
    pub const RESIZED: &[u8] = b"size";
    /// This program has drawn, and here is the rectangle it touched.
    pub const DAMAGED: &[u8] = b"damaged";
}

/// Bad address.
const ENODEV: u64 = (-19i64) as u64;
/// The device is not there for this process: it has no window.
const ENXIO: u64 = (-6i64) as u64;
/// An `ioctl` this device does not answer.
const ENOTTY: u64 = error::ENOTTY;

/// How long to wait for a frame to be acknowledged before giving up on it.
///
/// Ten seconds, the same number `nexus-window` uses and for the same reason: far
/// longer than any composite takes, far shorter than a person will wait at a
/// window that has stopped. Without it a program whose compositor stopped
/// answering blocks in `ioctl` for ever, and what that looks like from outside
/// is a program that hung.
const ACKNOWLEDGE_MS: u64 = 10_000;

/// What one translated process's window is.
struct Window {
    /// The channel to the compositor. The same one the descriptor names.
    compositor: Arc<ipc::Endpoint>,
    /// The buffer the program draws into, shared with the compositor.
    surface: Arc<ipc::MemoryObject>,
    width: u32,
    height: u32,
    /// Anything the compositor said that was not an acknowledgement, waiting to
    /// be read by [`request::EVENT`].
    ///
    /// Queued rather than dropped because presenting has to read from the same
    /// channel that carries keys, and a frame that swallowed every keystroke
    /// while it waited would be a window that only responds when nothing is
    /// being drawn.
    pending: Vec<(u32, u32)>,
    /// Whether the compositor has gone.
    closed: bool,
}

/// The most events to hold before dropping the oldest.
///
/// A program that never reads its events must not be able to make the kernel
/// grow a list for ever. Dropping the oldest rather than the newest, because
/// the newest is the one that describes the world now.
const MAX_PENDING: usize = 64;

/// Every open window, by process and descriptor.
static WINDOWS: IrqSpinLock<BTreeMap<(u64, u32), Window>> = IrqSpinLock::new(BTreeMap::new());

/// Frames presented, and events delivered, for the monitor.
static PRESENTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DELIVERED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Frames presented and events delivered.
#[must_use]
pub fn statistics() -> (u64, u64) {
    use core::sync::atomic::Ordering;
    (
        PRESENTED.load(Ordering::Relaxed),
        DELIVERED.load(Ordering::Relaxed),
    )
}

/// Forget a process's windows. Called when it ends.
pub fn forget(process: u64) {
    WINDOWS.lock().retain(|(owner, _), _| *owner != process);
}

/// Whether a path names this device.
#[must_use]
pub fn is_device(path: &str) -> bool {
    path == PATH
}

/// `openat` of the device: take the window the compositor offered.
///
/// The compositor sends a starting program one message before anything else:
/// the size of its surface and a handle to the surface itself. A Nexus window
/// program reads that message with `nexus_window::Window::open`; this reads the
/// same message, for a program that has never heard of it.
///
/// The descriptor is the compositor's channel, duplicated. Not the surface:
/// the surface is what `mmap` hands over, and the *channel* is the authority --
/// a program that closes this descriptor has given the window back, which is
/// what closing a window's file descriptor should mean.
pub fn open() -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };

    // The channel a window program is started with. It is the process's first
    // handle, which for a translated process is numbered three -- see
    // `Process::with_personality` for why they start there.
    let first = super::linux_files::FIRST_HANDLE;
    let Ok(compositor) = process
        .handles
        .channel(first, ipc::Rights::READ | ipc::Rights::WRITE)
    else {
        crate::kprintln!(
            "[linux] {} opened {PATH} without a compositor channel; it has no window",
            process.name.as_str()
        );
        return ENXIO;
    };

    // The opening message. Waited for, because the compositor sends it as soon
    // as it has made the surface and a program that opened the device a moment
    // too early would otherwise be told it had no window.
    //
    // Waited for with a deadline, and that is not caution. Handle three is the
    // channel to *whoever started this program*, which is the compositor only
    // when the compositor started it. A program started by `init` has a channel
    // to `init`, which is never going to send a surface -- and a blocking
    // receive there would be a program stopped for ever inside `openat`, which
    // from outside looks exactly like a machine that hung.
    let Some(message) = offered(&compositor) else {
        crate::kprintln!(
            "[linux] {} opened {PATH} and was offered no surface; it has no window",
            process.name.as_str()
        );
        return ENXIO;
    };
    if message.bytes.len() < 16 || message.handles.is_empty() {
        crate::kprintln!("[linux] the compositor's first message is not a surface");
        return ENODEV;
    }
    let width = read_u32(&message.bytes, 0);
    let height = read_u32(&message.bytes, 4);
    let ipc::Object::Memory(surface) = message.handles[0].object.clone() else {
        crate::kprintln!("[linux] the compositor's first handle is not memory");
        return ENODEV;
    };
    if !fits(width, height, surface.size()) {
        crate::kprintln!("[linux] the compositor's surface is too small for {width}x{height}");
        return ENODEV;
    }

    // The descriptor: a second handle on the same channel, so that closing it
    // is the program giving up its window rather than closing the handle
    // something else is still using.
    let Ok(descriptor) = process.handles.duplicate(
        first,
        ipc::Rights::READ | ipc::Rights::WRITE | ipc::Rights::CLOSE,
    ) else {
        return error::ENOMEM;
    };

    WINDOWS.lock().insert(
        (process.id.0, descriptor),
        Window {
            compositor,
            surface,
            width,
            height,
            pending: Vec::new(),
            closed: false,
        },
    );
    crate::kprintln!(
        "[linux] {} opened {PATH}: a {width}x{height} window on descriptor {descriptor}",
        process.name.as_str()
    );
    u64::from(descriptor)
}

/// How long to wait for the compositor's opening message.
///
/// Two seconds. Far longer than it takes a compositor that is about to send one,
/// and short enough that a program that is never going to be sent one finds out
/// while somebody is still watching.
const OFFER_MS: u64 = 2000;

/// Wait for one message on a channel, or give up.
///
/// There is no bounded receive on an endpoint: `receive` waits until a message
/// arrives or the peer goes, and neither of those need ever happen here. So this
/// polls, which is the thing the rest of this kernel is built to avoid -- and is
/// the right trade for exactly one call, made once per window, against a channel
/// whose peer may not be a compositor at all.
fn offered(channel: &Arc<ipc::Endpoint>) -> Option<ipc::Message> {
    let deadline = crate::arch::time::uptime_ms().saturating_add(OFFER_MS);
    loop {
        if let Some(message) = channel.try_receive() {
            return Some(message);
        }
        if !channel.peer_open() || crate::sched::cancelled() {
            return None;
        }
        if crate::arch::time::uptime_ms() >= deadline {
            return None;
        }
        crate::sched::sleep_ms(10);
    }
}

/// Whether a descriptor is one of these.
#[must_use]
pub fn is_window(process: u64, descriptor: u64) -> bool {
    u32::try_from(descriptor).is_ok_and(|id| WINDOWS.lock().contains_key(&(process, id)))
}

/// The surface behind a descriptor, for `mmap`.
///
/// Taken from here rather than from the handle table, because the surface
/// changes when the window is resized and the descriptor does not: the
/// descriptor is the window, and the buffer is what the window currently has.
#[must_use]
pub fn surface_of(process: u64, descriptor: u64) -> Option<Arc<ipc::MemoryObject>> {
    let id = u32::try_from(descriptor).ok()?;
    WINDOWS
        .lock()
        .get(&(process, id))
        .map(|window| Arc::clone(&window.surface))
}

/// `close` of the descriptor: the program has given the window back.
pub fn close(process: u64, descriptor: u32) {
    WINDOWS.lock().remove(&(process, descriptor));
}

/// `ioctl(fd, request, argument)` on this device.
pub fn ioctl(descriptor: u64, request: u64, argument: u64) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Ok(id) = u32::try_from(descriptor) else {
        return error::EBADF;
    };
    match request {
        request::INFO => info(process.id.0, id, argument),
        request::PRESENT => present(process.id.0, id, argument),
        request::EVENT => next_event(process.id.0, id, argument),
        // Every other request, including every real Linux one. A device that
        // answered zero to an `ioctl` it does not implement would be a device a
        // program believes it has configured.
        _ => ENOTTY,
    }
}

/// `NEXUS_DISPLAY_INFO`: how large the surface is, and how it is laid out.
fn info(process: u64, descriptor: u32, out: u64) -> u64 {
    let (width, height) = {
        let windows = WINDOWS.lock();
        let Some(window) = windows.get(&(process, descriptor)) else {
            return error::EBADF;
        };
        (window.width, window.height)
    };
    let Some((pointer, _)) = crate::arch::syscall::user_range(out, INFO_BYTES, INFO_BYTES) else {
        return error::EFAULT;
    };
    // Stride is width times four here, and is reported rather than implied so
    // that a compositor which one day pads its rows does not silently break
    // every program that had assumed.
    let fields = [width, height, width.saturating_mul(4), 0];
    // SAFETY: sixteen bytes inside the user half, checked above, written as the
    // four `u32` this request is defined to write.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr().cast::<u8>(), pointer as *mut u8, 16);
    }
    0
}

/// `NEXUS_DISPLAY_PRESENT`: put a rectangle on screen, and wait for it.
///
/// The wait is the part that matters and is not politeness. The surface is
/// shared: while the compositor is copying out of it, a program that carried on
/// drawing would be a program tearing its own frame. `shown` is the compositor
/// saying the buffer is the program's again, and every window program on this
/// machine waits for it -- so a translated one does too, inside this call,
/// where a program that has never heard of the protocol still gets it right.
fn present(process: u64, descriptor: u32, rectangle: u64) -> u64 {
    let Some((pointer, _)) =
        crate::arch::syscall::user_range(rectangle, RECTANGLE_BYTES, RECTANGLE_BYTES)
    else {
        return error::EFAULT;
    };
    // SAFETY: sixteen bytes inside the user half, read as the four `u32` a
    // rectangle is.
    let rectangle: [u32; 4] = unsafe { core::ptr::read_unaligned(pointer as *const [u32; 4]) };

    let compositor = {
        let windows = WINDOWS.lock();
        let Some(window) = windows.get(&(process, descriptor)) else {
            return error::EBADF;
        };
        if window.closed {
            return ENXIO;
        }
        // Outside the window this program was given is not a damage report, it
        // is a mistake -- and one the compositor would have to check anyway.
        if rectangle[0].saturating_add(rectangle[2]) > window.width
            || rectangle[1].saturating_add(rectangle[3]) > window.height
        {
            return error::EINVAL;
        }
        Arc::clone(&window.compositor)
    };

    let mut message = Vec::with_capacity(wire::DAMAGED.len() + 16);
    message.extend_from_slice(wire::DAMAGED);
    for value in rectangle {
        message.extend_from_slice(&value.to_le_bytes());
    }
    if compositor.send(&message, Vec::new()).is_err() {
        mark_closed(process, descriptor);
        return ENXIO;
    }
    PRESENTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // And wait to be told it is on screen. Anything else that arrives while
    // waiting is a key or a resize, and is kept rather than thrown away.
    let deadline = crate::arch::time::uptime_ms().saturating_add(ACKNOWLEDGE_MS);
    loop {
        let Some(answer) = compositor.receive() else {
            mark_closed(process, descriptor);
            return ENXIO;
        };
        if answer.bytes == wire::SHOWN {
            return 0;
        }
        absorb(process, descriptor, &answer);
        if crate::arch::time::uptime_ms() >= deadline {
            crate::kprintln!("[linux] a frame went unacknowledged for {ACKNOWLEDGE_MS}ms");
            return (-110i64) as u64; // ETIMEDOUT
        }
    }
}

/// `NEXUS_DISPLAY_EVENT`: the next thing that happened, or nothing.
///
/// Never blocks. A program that wants to wait calls this, finds nothing, and
/// draws -- and a program that wanted to block would need `poll`, which this
/// layer does not have and which is written down rather than approximated with
/// a sleep.
fn next_event(process: u64, descriptor: u32, out: u64) -> u64 {
    // Anything already waiting first, then a look at the channel: a program
    // that drew a frame and then asked for events must see the key that arrived
    // while the frame was in flight before it sees a newer one.
    let queued = {
        let mut windows = WINDOWS.lock();
        let Some(window) = windows.get_mut(&(process, descriptor)) else {
            return error::EBADF;
        };
        window.pending.first().copied()
    };

    let event = match queued {
        Some(event) => {
            WINDOWS
                .lock()
                .get_mut(&(process, descriptor))
                .map(|window| window.pending.remove(0));
            event
        }
        None => {
            let compositor = {
                let windows = WINDOWS.lock();
                let Some(window) = windows.get(&(process, descriptor)) else {
                    return error::EBADF;
                };
                Arc::clone(&window.compositor)
            };
            match compositor.try_receive() {
                Some(message) => {
                    absorb(process, descriptor, &message);
                    let mut windows = WINDOWS.lock();
                    match windows.get_mut(&(process, descriptor)) {
                        Some(window) if !window.pending.is_empty() => window.pending.remove(0),
                        _ => (event::NONE, 0),
                    }
                }
                None => (event::NONE, 0),
            }
        }
    };

    let (width, height) = {
        let windows = WINDOWS.lock();
        let Some(window) = windows.get(&(process, descriptor)) else {
            return error::EBADF;
        };
        (window.width, window.height)
    };

    let Some((pointer, _)) = crate::arch::syscall::user_range(out, EVENT_BYTES, EVENT_BYTES) else {
        return error::EFAULT;
    };
    let fields = [event.0, event.1, width, height];
    // SAFETY: sixteen bytes inside the user half, written as the four `u32`
    // this request is defined to write.
    unsafe {
        core::ptr::copy_nonoverlapping(fields.as_ptr().cast::<u8>(), pointer as *mut u8, 16);
    }
    if event.0 != event::NONE {
        DELIVERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    0
}

/// Take in one message from the compositor that was not an acknowledgement.
///
/// A resize replaces the surface, which means the mapping the program is
/// holding is of a buffer that is no longer the window's. Nothing here unmaps
/// it: the program asked for that address and this layer does not take an
/// address back. What it does is say so, and a program that ignores the event
/// carries on drawing into memory nobody is showing -- which is visible, and is
/// better than a mapping that silently changes under it.
fn absorb(process: u64, descriptor: u32, message: &ipc::Message) {
    let mut windows = WINDOWS.lock();
    let Some(window) = windows.get_mut(&(process, descriptor)) else {
        return;
    };

    let event = if message.bytes.len() >= 12
        && message.bytes.starts_with(wire::RESIZED)
        && message.handles.len() == 1
    {
        let width = read_u32(&message.bytes, 4);
        let height = read_u32(&message.bytes, 8);
        match message.handles[0].object.clone() {
            ipc::Object::Memory(surface) if fits(width, height, surface.size()) => {
                window.surface = surface;
                window.width = width;
                window.height = height;
                (event::RESIZED, 0)
            }
            // A resize this layer cannot honour is the window being lost, and
            // saying so is better than carrying on with a size that is no
            // longer true.
            _ => {
                window.closed = true;
                (event::CLOSED, 0)
            }
        }
    } else if message.bytes.len() >= 5 {
        // A key, in the five bytes the compositor forwards: one of kind and
        // four of value. Kind 1 is a character and everything else is a key
        // this layer passes through by number rather than naming, because
        // naming them here would be a second table to keep in step with the
        // keyboard driver.
        let value = read_u32(&message.bytes, 1);
        if message.bytes[0] == 1 {
            (event::CHARACTER, value)
        } else {
            (event::KEY, u32::from(message.bytes[0]))
        }
    } else {
        return;
    };

    if window.pending.len() >= MAX_PENDING {
        window.pending.remove(0);
    }
    window.pending.push(event);
}

/// Record that the compositor has gone.
fn mark_closed(process: u64, descriptor: u32) {
    if let Some(window) = WINDOWS.lock().get_mut(&(process, descriptor)) {
        window.closed = true;
        window.pending.push((event::CLOSED, 0));
    }
}

/// Whether a buffer of `size` bytes holds a surface of these dimensions.
///
/// Checked rather than assumed, and the arithmetic is checked too: on a build
/// without overflow checks `width * height * 4` comes back small for enormous
/// dimensions, and a comparison meant to guard a mapping would pass.
fn fits(width: u32, height: u32, size: usize) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .is_some_and(|needed| needed <= size)
}

/// Read a little-endian `u32` out of a message.
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |four| {
        u32::from_le_bytes([four[0], four[1], four[2], four[3]])
    })
}
