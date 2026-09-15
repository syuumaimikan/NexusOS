//! `wall`: what is behind the windows.
//!
//! An ordinary client. It is handed a surface and draws into it exactly as a
//! window does; what differs is only where the compositor puts it, which is
//! underneath everything else.
//!
//! # Why the wallpaper is a program
//!
//! Because the alternative is the compositor deciding what a desktop looks
//! like, and the compositor is the one program on this machine whose decisions
//! nothing else can replace. A background is *taste* — a colour, a pattern,
//! one day a picture or a video — and taste in the program that owns the
//! framebuffer is the same mistake as policy in the kernel, one layer up.
//!
//! So this draws, and the compositor composites a surface it knows nothing
//! about. What that buys is concrete: replacing the wallpaper is replacing one
//! program, and a wallpaper that crashes is a black rectangle rather than a
//! machine with no display.
//!
//! # What it cannot do yet
//!
//! Pictures and video. A picture needs a decoder for whatever format it is in;
//! a video needs one that runs thirty times a second. Neither is here, and the
//! patterns below are drawn rather than loaded — which is why they cost a few
//! hundred lines instead of a few hundred thousand.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::panic::PanicInfo;

use nexus_look::{Colour, Look, Style};
use nexus_ui::Canvas;
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_4800_0000;

/// How much heap: the settings file, and nothing else.
const HEAP: usize = 12 * 1024 * 1024;

/// The directory a wallpaper picture is looked for in.
///
/// One directory, and the same one the picture viewer shows. The wallpaper is
/// lent `PICTURES` read-only and nothing else, so a setting naming anything
/// outside it names a file this program could not open if it tried -- which is
/// why `nexus_look` refuses a value with a separator in it rather than leaving
/// that to be discovered here.
const PICTURES: &str = "PICTURES";

/// The largest wallpaper file this will read.
///
/// A recording is read a frame at a time and is not bounded by this; a still
/// picture is read whole, and eight mebibytes is a very large photograph.
const MAX_PICTURE: usize = 8 * 1024 * 1024;

/// The most pixels a wallpaper may have.
///
/// A 4K screen is 8.3 megapixels, so this allows a picture rather larger than
/// any screen this runs on. Four bytes each, which is most of the heap above --
/// and the reason the heap is what it is.
const MOST_PIXELS: usize = 12 * 1024 * 1024 / 4;

/// How often a recording's frames are shown, at most.
///
/// It used to be four a second, and the reason was that there were no
/// per-client damage rectangles: a wallpaper frame cost a composite of the
/// whole display whatever had changed. There are now, and a recording declares
/// the rectangle its picture covers -- so a frame costs that picture's area
/// and the cap can go.
///
/// What is left is the file's own interval, floored here. Thirty a second is
/// what a recording of any kind asks for and is as fast as this is willing to
/// wake up for a decoration.
const VIDEO_FLOOR_MS: u64 = 33;

/// And a ceiling, for a file whose header asks for something absurd or says
/// nothing usable at all.
const VIDEO_CEILING_MS: u64 = 1_000;

/// What to use when the recording does not say.
const VIDEO_MS: u64 = 250;

/// What the settings file is called.
const SETTINGS_NAME: &str = "settings.txt";

/// The longest settings file this will read.
const SETTINGS_MAX: usize = 16 * 1024;

/// How often a moving pattern draws a new frame.
///
/// Four a second, and slow on purpose. A background fills the whole area
/// windows may occupy, so every frame of it is a full-screen composite -- two
/// million pixels copied by the compositor, which is the cost that matters and
/// not the drawing. Four is enough for drift to read as drift and few enough
/// that a moving background is a small fraction of a processor rather than a
/// steady load.
///
/// Damage rectangles exist now and do not help here: a drifting pattern changes
/// every part of the surface, so its damage really is the whole of it. What
/// would help is a pattern that knew which points it moved -- a different piece
/// of work, and one worth doing only if somebody wants a faster background.
const FRAME_MS: u64 = 250;

/// How often the settings are looked at again, in milliseconds.
///
/// Two seconds. A wallpaper that re-read the file every frame would be a
/// wallpaper opening a file four times a second for an answer that changes when
/// somebody types a command.
///
/// Milliseconds and not frames: a still pattern has no frames, and a moving one
/// turned out not to be counting them reliably either.
const RECHECK_MS: u64 = 2_000;

/// How long a still pattern waits before looking, since it has nothing else to
/// wake it.
const STILL_RECHECK_MS: u64 = 2_000;

/// What the compositor says.
mod wire {
    pub const SHOWN: &[u8] = b"shown";
    pub const RESIZED: &[u8] = b"size";
    /// Followed, when there is one, by four little-endian `u32`s saying which
    /// part of the surface changed. See `damage_message`.
    pub const DAMAGED: &[u8] = b"damaged";
}

/// The `damaged` message, with the rectangle on it when there is one.
///
/// The same shape `nexus_window` sends; this program does not use that crate
/// because it is handed the bottom surface directly rather than being given a
/// window.
fn damage_message(rectangle: Option<(u32, u32, u32, u32)>) -> [u8; 23] {
    let mut out = [0u8; 23];
    out[..7].copy_from_slice(wire::DAMAGED);
    if let Some((x, y, width, height)) = rectangle {
        out[7..11].copy_from_slice(&x.to_le_bytes());
        out[11..15].copy_from_slice(&y.to_le_bytes());
        out[15..19].copy_from_slice(&width.to_le_bytes());
        out[19..23].copy_from_slice(&height.to_le_bytes());
    }
    out
}

/// How much of it to send: the word alone when there is no rectangle.
const fn damage_length(rectangle: Option<(u32, u32, u32, u32)>) -> usize {
    if rectangle.is_some() {
        23
    } else {
        7
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
    if !nexus_user::heap::init(HEAP) {
        failed("wall: FAILED: could not get a heap");
        finish();
    }

    // Two handles: the surface, and the settings directory to read what to
    // draw. Without the second it draws the default, which is what this system
    // looked like before it could be changed.
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 3];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("wall: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles < 1 || received.bytes < 16 {
        failed("wall: FAILED: no surface came with the message");
        finish();
    }

    let mut width = read_u32(&buffer, 0);
    let mut height = read_u32(&buffer, 4);
    let mut surface = handles[0];
    let settings = (received.handles >= 2).then(|| handles[1]);
    // The pictures directory, read-only, when the compositor had one to lend.
    // Without it a picture cannot be drawn and the pattern is what there is,
    // which is what this program did before it could draw one at all.
    let pictures = (received.handles >= 3).then(|| handles[2]);

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("wall: FAILED: could not map its surface");
        finish();
    };
    if width as usize * height as usize * 4 > mapped {
        failed("wall: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    // The defaults only when there is genuinely nothing to read, which on a
    // machine nobody has configured is the truth.
    let mut look = read_look(settings).unwrap_or_default();
    nexus_user::log(&alloc::format!(
        "wall: {width}x{height} behind the windows, {} on {}",
        look.style.name(),
        look.top.to_text()
    ))
    .ok();

    let Ok(set) = nexus_user::wait_set() else {
        failed("wall: FAILED: could not make a wait set");
        finish();
    };
    const SAID: u64 = 1;
    if nexus_user::watch(set, COMPOSITOR, SAID).is_err() {
        failed("wall: FAILED: could not watch the compositor");
        finish();
    }

    let mut frame = 0u32;
    // When the settings were last read, on the machine's own clock.
    //
    // A clock and not a count of timed-out waits, which is what this was and
    // which was wrong in a way that only showed up on a moving pattern: the
    // recheck sat inside the `ready == 0` branch, so it happened only when a
    // wait *expired*. An animated wallpaper is answered by the compositor on
    // nearly every wait, so the branch almost never ran and a setting changed
    // while the stars were drifting went unnoticed for over a minute.
    let mut last_check = nexus_user::uptime();
    let mut stale = true;
    let mut in_flight = false;
    // Whether the *whole* surface changed, rather than just the recording's
    // rectangle. True for the first frame, for a moving pattern, for a resize
    // and for a settings change -- and false for the ordinary case this exists
    // for, which is one video frame replacing the last while the pattern
    // behind it stays exactly as it was.
    let mut whole = true;

    // Whatever the settings name, opened once. Reopened only when the name or
    // the fit changes -- decoding a wallpaper on every settings re-read would
    // be decoding it every two seconds for the life of the machine.
    let mut behind = open_picture(pictures, &look);
    // When the next frame of a recording is due.
    let mut next_due = nexus_user::uptime();
    // Whether the rate it managed has been said. Once, after enough frames to
    // mean anything: a wallpaper that reported its frame rate every second
    // would be a wallpaper filling the log.
    let mut said_rate = false;

    // Bounded, so a wallpaper whose compositor stops answering cannot spin.
    // Large, because at four frames a second this is most of a day and the
    // thing it draws is meant to be there all of it.
    for _ in 0..16_000_000u64 {
        if stale && !in_flight {
            let covered = draw(&look, behind.as_ref(), width, height, frame);
            // The picture's rectangle when that is all that changed, and the
            // whole surface otherwise. This is the difference between a
            // recording costing its own area and costing the display.
            let damage = if whole { None } else { covered };
            let message = damage_message(damage);
            let length = damage_length(damage);
            if nexus_user::send(COMPOSITOR, &message[..length], &[]).is_err() {
                break;
            }
            stale = false;
            whole = false;
            in_flight = true;
        }

        // A still pattern waits to be woken and costs nothing; a moving one
        // wakes on its own. What decides is the pattern, which is the only
        // thing that knows whether anything would change.
        // A recording wakes on its own clock; a moving pattern on the frame
        // clock; a still one waits to be woken and costs nothing. What decides
        // is what is actually being drawn.
        let wait = if let Some(Behind::Moving { interval_us, .. }) = behind.as_ref() {
            // What the file asks for, held between a floor and a ceiling. A
            // recording claiming a microsecond a frame is a recording this
            // will play at thirty.
            (u64::from(*interval_us) / 1000).clamp(VIDEO_FLOOR_MS, VIDEO_CEILING_MS)
        } else if look.style.moves() {
            FRAME_MS
        } else {
            STILL_RECHECK_MS
        };
        let mut keys = [0u64; 2];
        let Ok(ready) = nexus_user::wait_any_until(set, &mut keys, wait) else {
            break;
        };

        // Looked at again every so often, so that changing a setting shows up
        // without anything having to tell this program. Before the wake is
        // dealt with, and whatever woke it: how long it has been is a question
        // about the clock, not about why this loop is running.
        let now = nexus_user::uptime();
        if now.saturating_sub(last_check) >= RECHECK_MS {
            last_check = now;
            // Nothing when it could not be read, and the look is left alone.
            let Some(fresh) = read_look(settings) else {
                continue;
            };
            if fresh != look {
                nexus_user::log(&alloc::format!(
                    "wall: the look changed to {} on {}",
                    fresh.style.name(),
                    fresh.top.to_text()
                ))
                .ok();
                // Reopened only when the file or the fit actually changed.
                // Everything else -- a colour, the style, the font -- leaves a
                // decoded wallpaper alone, and re-decoding it on every settings
                // change would cost a full decode every time somebody moved a
                // slider.
                let different = fresh.picture != look.picture || fresh.fit != look.fit;
                look = fresh;
                if different {
                    if let Some(old) = behind.take() {
                        old.close();
                    }
                    behind = open_picture(pictures, &look);
                    next_due = nexus_user::uptime();
                }
                stale = true;
                // A different picture, a different fit or a different pattern:
                // all of the surface is new.
                whole = true;
            }
        }

        if ready == 0 {
            frame = frame.wrapping_add(1);
            if look.style.moves() {
                stale = true;
                // The pattern itself moved, so the rectangle the picture
                // covers is not the whole of what changed.
                whole = true;
            }
            // A recording, if it is time. The deadline moves by one interval
            // rather than to "now", so that a decode taking most of an interval
            // does not make every frame later than the last -- and is reset if
            // it has fallen more than a second behind, because a machine that
            // cannot keep up should play slowly rather than sprint for ever
            // after a schedule it will never meet.
            if matches!(behind, Some(Behind::Moving { .. })) && now >= next_due {
                if next_frame(behind.as_mut().expect("just matched")) {
                    stale = true;
                }
                let interval = match behind.as_ref() {
                    Some(Behind::Moving { interval_us, .. }) => {
                        (u64::from(*interval_us) / 1000).clamp(VIDEO_FLOOR_MS, VIDEO_CEILING_MS)
                    }
                    _ => VIDEO_MS,
                };
                next_due = if now.saturating_sub(next_due) > 1000 {
                    now + interval
                } else {
                    next_due + interval
                };
                said_rate = say_rate(behind.as_ref(), said_rate);
            }
            continue;
        }

        let mut message = [0u8; 64];
        let mut incoming = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut incoming) else {
            break;
        };
        let bytes = &message[..received.bytes];

        if bytes == wire::SHOWN {
            in_flight = false;
            continue;
        }

        if received.bytes >= 12 && bytes.starts_with(wire::RESIZED) && received.handles == 1 {
            nexus_user::memory_unmap(surface, SURFACE_AT).ok();
            nexus_user::close(surface).ok();
            surface = incoming[0];
            width = read_u32(bytes, 4);
            height = read_u32(bytes, 8);
            let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
                failed("wall: FAILED: could not map the surface it was given");
                break;
            };
            if width as usize * height as usize * 4 > mapped {
                failed("wall: FAILED: the new surface is smaller than its size");
                break;
            }
            stale = true;
            // A new surface of a different size. Nothing on it is what was
            // there, and the compositor has a different rectangle to fill.
            whole = true;
        }
    }

    finish()
}

/// What the settings say this machine should look like.
fn read_look(settings: Option<Handle>) -> Option<Look> {
    let directory = settings?;
    // `None` means "could not read it", not "it says the defaults".
    //
    // The difference is not academic. Replacing a file here means removing the
    // name and making it again, because there is no truncate -- so there is a
    // window, short but real, in which the name does not exist. A reader that
    // answered that window with the defaults would throw away somebody's
    // wallpaper because another program was half-way through saving it, and
    // then throw away the *new* setting too by treating the defaults as the
    // current state.
    //
    // That is exactly what happened: the wallpaper reported changing to the
    // default gradient in the middle of a save, and then never noticed the
    // style that was actually written.
    let file = nexus_user::open(directory, SETTINGS_NAME).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(SETTINGS_MAX);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    // An empty file is a file being written, not a file asking for defaults.
    if bytes.is_empty() {
        return None;
    }
    String::from_utf8(bytes).ok().map(|text| Look::parse(&text))
}

/// A picture or a recording, opened and ready to draw.
///
/// A still picture is decoded once and kept. A recording keeps its file open
/// and its table of contents -- sixteen bytes a frame -- and decodes one frame
/// at a time, which is the same discipline the picture viewer follows and for
/// the same reason: a recording is not something that fits in memory.
enum Behind {
    /// One picture, decoded.
    Still(nexus_image::Picture),
    /// A recording, and where it has got to.
    Moving {
        file: Handle,
        frames: alloc::vec::Vec<nexus_image::avi::Frame>,
        at: usize,
        /// The frame on screen now, decoded.
        showing: Option<nexus_image::Picture>,
        /// Somewhere to read a compressed frame into, kept between frames.
        compressed: alloc::vec::Vec<u8>,
        /// How many have been decoded and how long that took, so the machine
        /// can say what it managed rather than what was asked for.
        decoded: u32,
        decoding_ms: u64,
        /// What the file asks for, in microseconds between frames.
        interval_us: u32,
    },
}

impl Behind {
    /// Whatever should be drawn now.
    fn picture(&self) -> Option<&nexus_image::Picture> {
        match self {
            Self::Still(picture) => Some(picture),
            Self::Moving { showing, .. } => showing.as_ref(),
        }
    }

    /// Give the file back, if there is one.
    fn close(self) {
        if let Self::Moving { file, .. } = self {
            nexus_user::close(file).ok();
        }
    }
}

/// Open whatever `look.picture` names, if anything.
///
/// `None` for every reason: no directory lent, no name set, a name that will
/// not open, bytes that are not a picture. Each says so in the log *except*
/// "no name set", which is not a problem and would be noise repeated every
/// time the settings are re-read.
fn open_picture(pictures: Option<Handle>, look: &Look) -> Option<Behind> {
    if look.picture.is_empty() {
        return None;
    }
    let root = pictures?;
    let Ok(directory) = nexus_user::open(root, PICTURES) else {
        nexus_user::log("wall: there is no PICTURES directory to take a wallpaper from").ok();
        return None;
    };
    // As typed, and then uppercased.
    //
    // Names on this store are uppercase -- the kernel seeds them from an image
    // whose directory is FAT, where they are -- so `nexus.jpg` typed into the
    // settings names nothing. Uppercasing is following the filesystem's own
    // convention rather than being clever: somebody typing a filename should
    // not have to know that.
    let opened = nexus_user::open(directory, &look.picture)
        .or_else(|_| nexus_user::open(directory, &look.picture.to_uppercase()));
    nexus_user::close(directory).ok();
    let Ok(file) = opened else {
        nexus_user::log(&alloc::format!("wall: {} will not open", look.picture)).ok();
        return None;
    };

    let size = nexus_user::size(file).unwrap_or(0);
    let mut head = alloc::vec![0u8; size.min(64 * 1024)];
    let read = nexus_user::read_at(file, 0, &mut head).unwrap_or(0);
    head.truncate(read);

    if nexus_image::kind(&head) == Some("avi") {
        let reel = match nexus_image::avi::read(&head) {
            Ok(reel) => reel,
            Err(why) => {
                nexus_user::log(&alloc::format!(
                    "wall: {} is not a recording: {why}",
                    look.picture
                ))
                .ok();
                nexus_user::close(file).ok();
                return None;
            }
        };
        if reel.movi_at == 0 {
            nexus_user::log(&alloc::format!(
                "wall: {}'s headers run further in than this reads",
                look.picture
            ))
            .ok();
            nexus_user::close(file).ok();
            return None;
        }
        let frames = walk_frames(file, reel.movi_at, reel.movi_end);
        if frames.is_empty() {
            nexus_user::log(&alloc::format!("wall: {} has no frames", look.picture)).ok();
            nexus_user::close(file).ok();
            return None;
        }
        nexus_user::log(&alloc::format!(
            "wall: playing {} behind everything, {}x{}, {} frames",
            look.picture,
            reel.width,
            reel.height,
            frames.len()
        ))
        .ok();
        let mut behind = Behind::Moving {
            file,
            frames,
            at: 0,
            showing: None,
            compressed: alloc::vec::Vec::new(),
            decoded: 0,
            decoding_ms: 0,
            interval_us: reel.interval_us,
        };
        next_frame(&mut behind);
        return Some(behind);
    }

    // A still picture, read whole.
    if size > MAX_PICTURE {
        nexus_user::log(&alloc::format!(
            "wall: {} is {} bytes, which is more than this will read",
            look.picture,
            size
        ))
        .ok();
        nexus_user::close(file).ok();
        return None;
    }
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);

    match nexus_image::decode(&bytes, look.bottom.packed(), MOST_PIXELS) {
        Ok(picture) => {
            nexus_user::log(&alloc::format!(
                "wall: showing {} behind everything, {}x{}",
                look.picture,
                picture.width,
                picture.height
            ))
            .ok();
            Some(Behind::Still(picture))
        }
        Err(why) => {
            nexus_user::log(&alloc::format!("wall: {}: {why}", look.picture)).ok();
            None
        }
    }
}

/// Move a recording on by one frame. Says whether anything changed.
fn next_frame(behind: &mut Behind) -> bool {
    let Behind::Moving {
        file,
        frames,
        at,
        showing,
        compressed,
        decoded,
        decoding_ms,
        ..
    } = behind
    else {
        return false;
    };
    let Some(frame) = frames.get(*at).copied() else {
        return false;
    };

    compressed.resize(frame.bytes as usize, 0);
    if nexus_user::read_at(*file, frame.at, compressed) != Ok(compressed.len()) {
        return false;
    }
    let began = nexus_user::uptime();
    let Ok(picture) = nexus_image::decode(compressed, 0, MOST_PIXELS) else {
        return false;
    };
    *decoding_ms += nexus_user::uptime().saturating_sub(began);
    *decoded += 1;
    *showing = Some(picture);
    *at = (*at + 1) % frames.len();
    true
}

/// Build a recording's table of contents by walking its chunk headers.
///
/// One eight-byte read per chunk and no frame data -- the same walk the picture
/// viewer does, and for the same reason: a wallpaper that read a recording into
/// memory would be a wallpaper with a running time compiled into it.
fn walk_frames(file: Handle, from: u64, to: u64) -> alloc::vec::Vec<nexus_image::avi::Frame> {
    let mut frames = alloc::vec::Vec::new();
    let mut at = from;
    let mut header = [0u8; 8];
    while at + 8 <= to && frames.len() < 12_000 {
        if nexus_user::read_at(file, at, &mut header) != Ok(header.len()) {
            break;
        }
        let (kind, length) = nexus_image::avi::chunk(&header);
        if at + 8 + u64::from(length) > to {
            break;
        }
        if nexus_image::avi::is_frame(kind) && length > 0 {
            frames.push(nexus_image::avi::Frame {
                at: at + 8,
                bytes: length,
            });
        }
        let next = nexus_image::avi::next_chunk(at, length);
        if next <= at {
            break;
        }
        at = next;
    }
    frames
}

/// Say, once, what a recording actually managed.
///
/// Asked for is one thing and achieved is another, and a machine that reported
/// only the first would be a machine where every recording plays perfectly.
/// There are no per-client damage rectangles here, so a full-screen wallpaper
/// frame costs a composite of the whole display -- which is why this is capped
/// at four a second before the decoder is even reached, and why the number
/// worth reporting is the *decode* rate rather than the frame rate.
fn say_rate(behind: Option<&Behind>, already: bool) -> bool {
    if already {
        return true;
    }
    let Some(Behind::Moving {
        decoded,
        decoding_ms,
        interval_us,
        ..
    }) = behind
    else {
        return already;
    };
    if *decoded < 20 || *decoding_ms == 0 {
        return false;
    }
    let milli = (u64::from(*decoded) * 1_000_000 / *decoding_ms) as u32;
    let asked = if *interval_us == 0 {
        0
    } else {
        (1_000_000_000u64 / u64::from(*interval_us)) as u32
    };
    // What it is actually shown at: the recording's own interval, clamped.
    // This used to say a flat four a second, which was true when a wallpaper
    // frame cost a composite of the whole display. It no longer does -- a
    // recording declares the rectangle its picture covers -- and a log line
    // still reporting four would be reporting a cap that has been removed.
    let interval = (u64::from(*interval_us) / 1000).clamp(VIDEO_FLOOR_MS, VIDEO_CEILING_MS);
    let shown = 1000 / interval.max(1);
    nexus_user::log(&alloc::format!(
        "wall: decoding at {}.{} frames a second; the recording asks for {}.{}, \
         and it is shown at {}",
        milli / 1000,
        (milli % 1000) / 100,
        asked / 1000,
        (asked % 1000) / 100,
        shown
    ))
    .ok();
    true
}

/// Draw a picture over the whole background, fitted the way the settings ask.
///
/// Nearest-neighbour and integer-only, for each pixel of the destination
/// working out which source pixel it came from. Done this way round rather than
/// by walking the source, because walking the source leaves gaps when scaling
/// up and writes the same destination pixel repeatedly when scaling down.
/// Draw the picture, and say what part of the screen it covered.
///
/// The rectangle is what lets a recording cost its own area instead of the
/// whole display: between one frame and the next, the pattern behind it is
/// redrawn identically and only this changed.
fn draw_picture(
    canvas: &mut nexus_ui::Canvas,
    picture: &nexus_image::Picture,
    fit: nexus_look::Fit,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    if picture.width == 0 || picture.height == 0 || width == 0 || height == 0 {
        return None;
    }

    const ONE: u64 = 65536;
    let by_width = ONE * u64::from(width) / u64::from(picture.width);
    let by_height = ONE * u64::from(height) / u64::from(picture.height);
    let scale = match fit {
        // The smaller, so all of it fits and the pattern shows around it.
        nexus_look::Fit::Whole => by_width.min(by_height).max(1),
        // The larger, so it covers -- the long side runs off the edges, which
        // is what filling a screen means.
        nexus_look::Fit::Fill => by_width.max(by_height).max(1),
        nexus_look::Fit::Middle => ONE,
    };

    let across = ((u64::from(picture.width) * scale) / ONE).max(1) as u32;
    let down = ((u64::from(picture.height) * scale) / ONE).max(1) as u32;
    // Centred, which for Fill means the parts that run off do so evenly.
    let left = (width as i64 - across as i64) / 2;
    let top = (height as i64 - down as i64) / 2;

    // What of that lands on the screen. `Fill` puts the long side off both
    // edges, so the covered rectangle is not the picture's size.
    let covered_left = left.max(0).min(i64::from(width)) as u32;
    let covered_top = top.max(0).min(i64::from(height)) as u32;
    let covered_right = (left + i64::from(across)).max(0).min(i64::from(width)) as u32;
    let covered_bottom = (top + i64::from(down)).max(0).min(i64::from(height)) as u32;

    for row in 0..down {
        let y = top + i64::from(row);
        if y < 0 || y >= i64::from(height) {
            continue;
        }
        let source_row = (u64::from(row) * u64::from(picture.height) / u64::from(down)) as u32;
        for column in 0..across {
            let x = left + i64::from(column);
            if x < 0 || x >= i64::from(width) {
                continue;
            }
            let source_column =
                (u64::from(column) * u64::from(picture.width) / u64::from(across)) as u32;
            if let Some(pixel) = picture.at(source_column, source_row) {
                canvas.set(x as u32, y as u32, nexus_ui::Colour(pixel));
            }
        }
    }

    if covered_right <= covered_left || covered_bottom <= covered_top {
        return None;
    }
    Some((
        covered_left,
        covered_top,
        covered_right - covered_left,
        covered_bottom - covered_top,
    ))
}

/// Draw the whole background, and say what part of it the picture covers.
///
/// The rectangle is only *usable* as damage when nothing else changed -- see
/// the frame loop, which decides that.
fn draw(
    look: &Look,
    behind: Option<&Behind>,
    width: u32,
    height: u32,
    frame: u32,
) -> Option<(u32, u32, u32, u32)> {
    // SAFETY: the surface is mapped here, writable, and at least
    // `width * height * 4` bytes -- checked when it was taken and again after
    // every replacement.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, width, height) };

    let top = nexus_ui::Colour(look.top.packed());
    let bottom = nexus_ui::Colour(look.bottom.packed());

    match look.style {
        Style::Plain => canvas.fill(canvas.bounds(), top),
        Style::Gradient => canvas.gradient(canvas.bounds(), top, bottom),
        Style::Stars => {
            canvas.gradient(canvas.bounds(), top, bottom);
            stars(&mut canvas, look, width, height, frame);
        }
        Style::Rings => {
            canvas.gradient(canvas.bounds(), top, bottom);
            rings(&mut canvas, look, width, height, frame);
        }
        Style::Grid => {
            canvas.gradient(canvas.bounds(), top, bottom);
            grid(&mut canvas, look, width, height);
        }
    }

    // And the picture over it, when there is one. Over rather than instead:
    // a picture fitted whole, or one smaller than the screen, shows the
    // pattern around it -- which is better than a black border and is why the
    // pattern is drawn even when it will mostly be covered.
    if let Some(picture) = behind.and_then(Behind::picture) {
        draw_picture(&mut canvas, picture, look.fit, width, height)
    } else {
        None
    }
}

/// A field of drifting points.
///
/// Positions come from a hash of the point's number rather than from a stored
/// list: the pattern is then decided by one number, the same every boot, and
/// there is nothing to keep between frames. Two hundred points at four pixels
/// is a thousand writes a frame, which at eight frames a second is nothing.
fn stars(canvas: &mut Canvas, look: &Look, width: u32, height: u32, frame: u32) {
    const COUNT: u32 = 220;
    for star in 0..COUNT {
        let hash = star.wrapping_mul(2_654_435_761).wrapping_add(0x9E37_79B9);
        // Three fields out of one hash: where it is, how fast it drifts, and
        // how bright it is. Taken from different bits so they are independent.
        let speed = 1 + (hash >> 28) % 4;
        let x = (hash % width.max(1) + frame.wrapping_mul(speed) / 4) % width.max(1);
        let y = (hash >> 8) % height.max(1);
        let brightness = 90 + ((hash >> 20) % 160) as u8;
        let colour = nexus_ui::Colour(
            look.bottom
                .towards(Colour::new(255, 255, 255), brightness)
                .packed(),
        );
        // Twinkling, from the frame and the star's own number, so they are not
        // all bright at once.
        let phase = (frame / 2).wrapping_add(star) % 24;
        let size = if phase < 2 { 2 } else { 1 };
        canvas.fill(nexus_ui::Rect::new(x, y, size, size), colour);
    }
}

/// Rings spreading from the middle.
///
/// Drawn with the midpoint circle algorithm, which is integers all the way
/// down: a decision variable, two increments, and eight points per step from
/// one octant's worth of work. There is no floating point here and none is
/// wanted -- a circle made of twenty-four sampled directions is twenty-four
/// dots, and one made of trigonometry is a table nobody can check.
fn rings(canvas: &mut Canvas, look: &Look, width: u32, height: u32, frame: u32) {
    let centre_x = width as i32 / 2;
    let centre_y = height as i32 / 2;
    let spacing = 90u32;
    // The rings move outwards by moving where the first one starts.
    let drift = (frame * 3) % spacing;
    let furthest = width.max(height);

    let mut radius = drift.max(4);
    while radius < furthest {
        // Fading with distance, so a ring arrives and leaves rather than
        // stopping at the edge of the screen.
        let fade = 44u32.saturating_sub(radius * 44 / furthest.max(1)) as u8;
        let colour = nexus_ui::Colour(look.bottom.towards(look.accent, fade.max(5)).packed());
        circle(
            canvas,
            centre_x,
            centre_y,
            radius as i32,
            colour,
            width,
            height,
        );
        radius += spacing;
    }
}

/// One circle, by the midpoint algorithm.
fn circle(
    canvas: &mut Canvas,
    centre_x: i32,
    centre_y: i32,
    radius: i32,
    colour: nexus_ui::Colour,
    width: u32,
    height: u32,
) {
    let mut x = radius;
    let mut y = 0;
    // The decision variable: positive means the next step goes inwards.
    let mut error = 1 - radius;

    while x >= y {
        // Eight points, one per octant, from the one pair this has computed.
        for (dx, dy) in [
            (x, y),
            (y, x),
            (-y, x),
            (-x, y),
            (-x, -y),
            (-y, -x),
            (y, -x),
            (x, -y),
        ] {
            let px = centre_x + dx;
            let py = centre_y + dy;
            if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < height {
                canvas.set(px as u32, py as u32, colour);
            }
        }
        y += 1;
        if error < 0 {
            error += 2 * y + 1;
        } else {
            x -= 1;
            error += 2 * (y - x) + 1;
        }
    }
}

/// A grid, fading downwards.
fn grid(canvas: &mut Canvas, look: &Look, width: u32, height: u32) {
    const SPACING: u32 = 48;
    let line = look.bottom.towards(look.accent, 28);

    let mut x = 0;
    while x < width {
        for y in 0..height {
            // Fainter towards the bottom, which is where the strip is and where
            // a pattern competing with it would be most in the way.
            let fade = 255u32.saturating_sub(y * 200 / height.max(1)) as u8;
            let colour = nexus_ui::Colour(
                nexus_look::Colour::from_packed(canvas_pixel(look, y, height))
                    .towards(line, fade / 3)
                    .packed(),
            );
            canvas.set(x, y, colour);
        }
        x += SPACING;
    }

    let mut y = 0;
    while y < height {
        let fade = 255u32.saturating_sub(y * 200 / height.max(1)) as u8;
        let colour = nexus_ui::Colour(
            nexus_look::Colour::from_packed(canvas_pixel(look, y, height))
                .towards(line, fade / 3)
                .packed(),
        );
        for x in 0..width {
            canvas.set(x, y, colour);
        }
        y += SPACING;
    }
}

/// What the gradient is at this row.
///
/// Worked out rather than read back, because reading the surface would be a
/// read from mapped memory per pixel for a number this can compute.
fn canvas_pixel(look: &Look, y: u32, height: u32) -> u32 {
    let amount = (y * 255 / height.max(1)) as u8;
    look.top.towards(look.bottom, amount).packed()
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
    nexus_user::log("wall: PANIC").ok();
    nexus_user::exit_with(2)
}
