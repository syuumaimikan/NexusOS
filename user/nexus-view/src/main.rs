//! `view`: a window that shows pictures, and plays recordings.
//!
//! It is given a directory and nothing else. It looks through it for files it
//! recognises — by their first bytes, not by their names, because a file called
//! `.png` that is a bitmap is a file somebody renamed — and shows one at a
//! time.
//!
//! # Video, and why it is here rather than in a program of its own
//!
//! Motion-JPEG is a sequence of complete JPEGs, so a program that can show a
//! photograph is most of the way to a program that can play a recording: what
//! is missing is a container reader and a clock. Both are small. A separate
//! player would have duplicated the directory listing, the fitting, the
//! settings-following and the window handshake to gain nothing.
//!
//! # A recording is not held
//!
//! A picture is read whole. A recording is not, and must not be: the kernel
//! will not make a memory object larger than sixteen mebibytes, so a program
//! that read a film into memory would be a program with a running time
//! compiled into it.
//!
//! Instead the file stays open and only its **table of contents** is held —
//! sixteen bytes a frame, walked once when the file is opened, with an
//! eight-byte read apiece. Playing reads one frame's bytes, decodes them,
//! draws them, and lets both go. Memory is one compressed frame and one
//! decoded frame, whatever the length of the recording.
//!
//! # Why the decoding is not in here
//!
//! `shared/nexus-image` reads the pixels and `shared/nexus-inflate` does the
//! decompressing, and both are tested on the build machine against files a
//! reference encoder wrote. A decoder tested only inside the machine it runs on
//! is a decoder tested against nothing: the interesting failures are malformed
//! files, and producing those on a guest with no compressor would be harder
//! than the decoder.
//!
//! What is left here is what a window does: choose a picture, fit it to the
//! space, and draw it.
//!
//! # Fitting
//!
//! Nearest-neighbour, in integers, keeping the shape. A picture is scaled down
//! to fit and never scaled up past its own size, because a sixteen-pixel icon
//! blown up to fill a window is not a better view of it — and because this
//! system has no floating point, so a smooth scale would be a fixed-point
//! resampler, which is a real piece of work and belongs after there is
//! something to look at.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::{Handle, Kind};
use nexus_window::{App, Key, Movement, Window};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_3400_0000;

/// How much heap: a compressed picture, its decompressed rows, and its pixels.
///
/// Twelve mebibytes, and the ceiling is not this program's choice: the kernel
/// will not make a memory object larger than sixteen, because the size comes
/// from a process and an unbounded one would let a program ask the kernel to
/// set aside all of memory. Asking for more than that fails at startup with
/// nothing on screen to say why, which is how this number came to be measured
/// rather than guessed.
const HEAP: usize = 12 * 1024 * 1024;

/// The directory pictures are looked for in, inside the filesystem this was
/// lent.
const PICTURES: &str = "PICTURES";

/// The largest file this will read.
const MAX_FILE: usize = 16 * 1024 * 1024;

/// The most pixels a picture may have before this refuses it.
///
/// One megapixel, which is what the heap above allows: four bytes a pixel for
/// the picture, about the same again for the rows the decoder unfilters before
/// it makes them into pixels, and the compressed file on top. A larger picture
/// is refused with a sentence rather than by running out of memory.
const MOST_PIXELS: usize = 1024 * 1024;

/// The most files it will list.
const MOST_FILES: usize = 256;

/// How much of a recording to read before it will say what it is.
///
/// Enough to reach the start of the frame list. `nexus_image::avi` asks for
/// sixty-four kilobytes and says why: writers pad their headers to an
/// alignment, ffmpeg's come to about six kilobytes, and this is an order of
/// magnitude of room for one read of a file that is going to be megabytes.
const VIDEO_HEAD: usize = 64 * 1024;

/// The most frames one recording may have.
///
/// The table of contents is sixteen bytes a frame, so this is a hundred and
/// ninety kilobytes of the heap at the limit -- and the limit is over twenty
/// minutes at ten frames a second, which is longer than anything this machine
/// has the storage to hold.
const MOST_FRAMES: usize = 12_000;

/// Space around things.
const PAD: u32 = 10;

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
        // Said with the number, because the number is the thing that is wrong:
        // the kernel refuses a memory object past its own bound, and a program
        // that asked for too much dies before it can draw a word.
        failed(heapless_message());
        finish();
    }

    let mut lent = [Handle(0); 1];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("view: FAILED: {trouble}"));
            finish();
        }
    };

    let mut viewer = Viewer::new((carried >= 1).then_some(lent[0]));
    nexus_user::log("view: a window for looking at pictures").ok();

    let outcome = window.run(&mut viewer);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        failed(&format!("view: FAILED: {}", outcome.ended));
    }
    if outcome.frames > 0 {
        nexus_user::log("view: showed what it was given to show").ok();
    }
    finish()
}

/// What to say when there is no heap, without a heap to say it with.
///
/// Formatting needs an allocator and there is not one, so this is assembled in
/// a fixed buffer. A program that could not report its own failure to start
/// would be a program that disappears.
fn heapless_message() -> &'static str {
    "view: FAILED: could not get a heap; the kernel will not make a memory object this large"
}

/// A file that might be a picture, or a recording.
struct Found {
    name: String,
    /// What its first bytes say it is, for showing beside the name.
    kind: &'static str,
}

impl Found {
    /// Whether this is something to play rather than something to show.
    fn moves(&self) -> bool {
        self.kind == "avi"
    }
}

/// A recording, open and being played.
///
/// The file handle is held for as long as this is: it is what every frame is
/// read through. Dropping it without closing the handle would leak it, so
/// [`Viewer::close_reel`] is the only way it goes.
struct Reel {
    /// The open file. Every frame is read through this.
    file: Handle,
    /// Microseconds between frames, from the file's own header.
    interval_us: u32,
    /// Where every frame is. Sixteen bytes each, and no frame data.
    frames: Vec<nexus_image::avi::Frame>,
    /// Which frame is on screen.
    at: usize,
    playing: bool,
    /// When the frame now on screen was due, on this machine's clock.
    ///
    /// The next frame's deadline is this plus the interval, rather than "now
    /// plus the interval". The difference is drift: a decode that takes sixty
    /// milliseconds at ten frames a second would otherwise make every frame
    /// late by sixty more than the last, and a minute of recording would take
    /// two minutes to play.
    due_ms: u64,
    /// Somewhere to decode into, kept between frames so that playing does not
    /// allocate and free a buffer thirty times a second.
    compressed: Vec<u8>,
    /// How many frames have been decoded and how long that has taken, so the
    /// machine can say what it actually managed rather than what was asked
    /// for.
    decoded: u32,
    decoding_ms: u64,
}

impl Reel {
    /// Milliseconds between frames.
    fn interval_ms(&self) -> u64 {
        // At least one: a header claiming a microsecond a frame would
        // otherwise ask this window to redraw in zero milliseconds for ever.
        (u64::from(self.interval_us) / 1000).max(1)
    }

    /// How long until the next frame is due, or zero if it is overdue.
    fn due_in_ms(&self) -> u64 {
        let next = self.due_ms + self.interval_ms();
        next.saturating_sub(nexus_user::uptime())
    }

    /// Frames a second, times a thousand.
    fn milli_fps(&self) -> u32 {
        if self.interval_us == 0 {
            return 0;
        }
        (1_000_000_000u64 / u64::from(self.interval_us)) as u32
    }

    /// What it actually managed, in frames a second times a thousand.
    ///
    /// Asked for is one thing and achieved is another, and a machine that only
    /// reported the first would be a machine that says every recording plays
    /// perfectly.
    fn measured_milli_fps(&self) -> u32 {
        if self.decoding_ms == 0 || self.decoded == 0 {
            return 0;
        }
        (u64::from(self.decoded) * 1_000_000 / self.decoding_ms) as u32
    }
}

/// A rate like 10000 as "10.0", without a floating-point unit.
fn rate(milli: u32) -> String {
    format!("{}.{}", milli / 1000, (milli % 1000) / 100)
}

/// The window.
struct Viewer {
    /// The directory pictures live in, opened once and kept: unlike the
    /// settings file, nothing here rewrites it, so holding it open costs
    /// nothing and saves an open per picture.
    directory: Option<Handle>,
    /// What was found.
    files: Vec<Found>,
    /// Which one is being shown.
    at: usize,
    /// The picture, once it has been decoded. A recording puts each frame
    /// here in turn, so everything that draws a picture draws a frame too.
    picture: Option<nexus_image::Picture>,
    /// The recording, if what is selected is one.
    reel: Option<Reel>,
    /// What happened last, shown at the bottom.
    said: String,
    trouble: bool,
    /// What the machine looks like, so this window matches it.
    look: nexus_look::Look,
}

impl Viewer {
    fn new(root: Option<Handle>) -> Self {
        let mut viewer = Self {
            directory: None,
            files: Vec::new(),
            at: 0,
            picture: None,
            reel: None,
            said: String::new(),
            trouble: false,
            look: nexus_look::Look::default(),
        };

        let Some(root) = root else {
            viewer.complain(nexus_i18n::text("view.nofiles"));
            return viewer;
        };
        match nexus_user::open(root, PICTURES) {
            Ok(directory) => {
                viewer.directory = Some(directory);
                viewer.look_through();
                viewer.show();
            }
            Err(_) => viewer.complain(&nexus_i18n::format(
                "view.nodirectory",
                &[("name", &PICTURES)],
            )),
        }
        viewer
    }

    /// Find every file in the directory whose first bytes say it is a picture.
    ///
    /// Read rather than guessed from the name. The first bytes are the only
    /// thing that knows, and a viewer that listed a text file because it ended
    /// in `.png` would be a viewer that shows an error where a picture should
    /// be.
    fn look_through(&mut self) {
        let Some(directory) = self.directory else {
            return;
        };

        let mut packed = alloc::vec![0u8; 16 * 1024];
        let listed = nexus_user::list(directory, &mut packed).unwrap_or(0);
        packed.truncate(listed);

        for entry in nexus_user::entries(&packed) {
            if self.files.len() >= MOST_FILES {
                break;
            }
            if entry.kind != Kind::File {
                continue;
            }
            // Only the first bytes, so that listing a directory of large
            // pictures does not mean reading all of them.
            let Some(head) = read_some(directory, entry.name, 16) else {
                continue;
            };
            let Some(kind) = nexus_image::kind(&head) else {
                continue;
            };
            self.files.push(Found {
                name: entry.name.to_string(),
                kind,
            });
        }

        self.files.sort_by(|one, other| one.name.cmp(&other.name));
        if self.files.is_empty() {
            self.complain(nexus_i18n::text("view.nothing"));
        }
    }

    /// Decode whatever is selected, or open it and start playing.
    fn show(&mut self) {
        self.close_reel();
        self.picture = None;
        let (Some(directory), Some(found)) = (self.directory, self.files.get(self.at)) else {
            return;
        };
        let name = found.name.clone();
        if found.moves() {
            self.play(&name);
            return;
        }

        let Some(bytes) = read_some(directory, &name, MAX_FILE) else {
            self.complain(&nexus_i18n::format("view.unreadable", &[("name", &name)]));
            return;
        };

        // The background the picture is flattened onto is this window's own, so
        // that a transparent picture sits on the window rather than on black.
        let behind = self.look.bottom.packed();
        match nexus_image::decode(&bytes, behind, MOST_PIXELS) {
            Ok(picture) => {
                let said = nexus_i18n::format(
                    "view.showing",
                    &[
                        ("name", &name),
                        ("width", &picture.width),
                        ("height", &picture.height),
                    ],
                );
                self.picture = Some(picture);
                self.say(&said);
                nexus_user::log(&format!("view: showed {name}")).ok();
            }
            Err(why) => {
                self.complain(&format!("{name}: {why}"));
                nexus_user::log(&format!("view: could not show {name}: {why}")).ok();
            }
        }
    }

    /// Open a recording and show its first frame.
    ///
    /// The file stays open afterwards. What is read here is its head -- enough
    /// to learn the size and the frame rate -- and then its table of contents,
    /// one eight-byte read per frame. The frames themselves are not read.
    fn play(&mut self, name: &str) {
        let Some(directory) = self.directory else {
            return;
        };
        let Ok(file) = nexus_user::open(directory, name) else {
            self.complain(&nexus_i18n::format("view.unreadable", &[("name", &name)]));
            return;
        };

        let size = nexus_user::size(file).unwrap_or(0);
        let mut head = alloc::vec![0u8; size.min(VIDEO_HEAD)];
        let read = nexus_user::read_at(file, 0, &mut head).unwrap_or(0);
        head.truncate(read);

        let reel = match nexus_image::avi::read(&head) {
            Ok(reel) => reel,
            Err(why) => {
                nexus_user::close(file).ok();
                self.complain(&format!("{name}: {why}"));
                nexus_user::log(&format!("view: could not play {name}: {why}")).ok();
                return;
            }
        };
        if reel.movi_at == 0 {
            // The head did not reach the frames. Said rather than treated as
            // an empty recording, because those are very different things and
            // only one of them is the file's fault.
            nexus_user::close(file).ok();
            self.complain(&nexus_i18n::format("view.videohead", &[("name", &name)]));
            return;
        }

        let frames = walk_frames(file, reel.movi_at, reel.movi_end);
        if frames.is_empty() {
            nexus_user::close(file).ok();
            self.complain(&nexus_i18n::format(
                "view.novideoframes",
                &[("name", &name)],
            ));
            return;
        }

        let said = nexus_i18n::format(
            "view.video",
            &[
                ("name", &name),
                ("width", &reel.width),
                ("height", &reel.height),
                ("count", &frames.len()),
                ("fps", &rate(reel.milli_fps())),
            ],
        );
        nexus_user::log(&format!(
            "view: playing {name}, {}x{}, {} frames at {} a second",
            reel.width,
            reel.height,
            frames.len(),
            rate(reel.milli_fps())
        ))
        .ok();

        self.reel = Some(Reel {
            file,
            interval_us: reel.interval_us,
            frames,
            at: 0,
            playing: true,
            due_ms: nexus_user::uptime(),
            compressed: Vec::new(),
            decoded: 0,
            decoding_ms: 0,
        });
        self.say(&said);
        self.decode_frame();
    }

    /// Read and decode the frame the reel is on.
    fn decode_frame(&mut self) -> bool {
        let Some(reel) = self.reel.as_mut() else {
            return false;
        };
        let Some(frame) = reel.frames.get(reel.at).copied() else {
            return false;
        };

        // One buffer, resized, rather than a fresh allocation thirty times a
        // second. `resize` keeps the capacity it already has.
        reel.compressed.resize(frame.bytes as usize, 0);
        let read = nexus_user::read_at(reel.file, frame.at, &mut reel.compressed).unwrap_or(0);
        if read != reel.compressed.len() {
            // A frame that is not all there. The recording stops rather than
            // showing half a picture, and says so.
            reel.playing = false;
            self.complain(nexus_i18n::text("view.videostopped"));
            return true;
        }

        let behind = self.look.bottom.packed();
        let began = nexus_user::uptime();
        let decoded = nexus_image::decode(&reel.compressed, behind, MOST_PIXELS);
        let took = nexus_user::uptime().saturating_sub(began);

        match decoded {
            Ok(picture) => {
                reel.decoded += 1;
                reel.decoding_ms += took;
                self.picture = Some(picture);
                true
            }
            Err(why) => {
                let at = reel.at;
                reel.playing = false;
                self.complain(&nexus_i18n::format(
                    "view.videoframe",
                    &[("at", &(at + 1)), ("why", &why)],
                ));
                true
            }
        }
    }

    /// Move to the next frame if its time has come.
    ///
    /// Called whenever the window's wait times out, which is not only when
    /// this asked for it -- so the clock is checked here rather than assumed.
    fn advance_if_due(&mut self) -> bool {
        let Some(reel) = self.reel.as_mut() else {
            return false;
        };
        if !reel.playing || reel.frames.is_empty() {
            return false;
        }
        if reel.due_in_ms() > 0 {
            return false;
        }

        let interval = reel.interval_ms();
        reel.at = (reel.at + 1) % reel.frames.len();
        // The deadline moves by exactly one interval, not to "now". See the
        // note on `due_ms`: the difference is whether a slow decode makes the
        // recording play slowly or makes it play late.
        //
        // Unless it has fallen more than a second behind, at which point the
        // machine is not keeping up and pretending otherwise would have it
        // decode every frame as fast as it can for ever, trying to catch up
        // with a schedule it cannot meet.
        let now = nexus_user::uptime();
        reel.due_ms = if now.saturating_sub(reel.due_ms) > 1000 {
            now
        } else {
            reel.due_ms + interval
        };

        // Round the loop: say what it managed, once per pass, so that a
        // recording which plays at four frames a second when it asked for
        // thirty says so in the log rather than looking fine.
        if reel.at == 0 && reel.decoded > 0 {
            let asked = rate(reel.milli_fps());
            let got = rate(reel.measured_milli_fps());
            let decoded = reel.decoded;
            nexus_user::log(&format!(
                "view: played {decoded} frames, decoding at {got} a second, asked for {asked}"
            ))
            .ok();
        }

        self.decode_frame()
    }

    /// Stop playing, and give back the handle.
    fn close_reel(&mut self) {
        if let Some(reel) = self.reel.take() {
            nexus_user::close(reel.file).ok();
        }
    }

    fn say(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = false;
    }

    fn complain(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = true;
    }

    /// Move to another picture.
    fn step(&mut self, by: isize) -> bool {
        if self.files.len() < 2 {
            return false;
        }
        let count = self.files.len() as isize;
        self.at = ((self.at as isize + by).rem_euclid(count)) as usize;
        self.show();
        true
    }
}

impl App for Viewer {
    fn draw(&mut self, canvas: &mut Canvas) {
        // Both together, and from the settings file: the face and whether its
        // edges are blended are what a person chose, and a window that ignored
        // them would be a window that looks like it came from somewhere else.
        canvas.set_text_style(
            nexus_ui::font::Face::parse(Some(self.look.font.name())),
            self.look.smooth,
        );
        let top = Colour(self.look.top.packed());
        let bottom = Colour(self.look.bottom.packed());
        let accent = Colour(self.look.accent.packed());
        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x8A, 0x9A, 0xB4);

        canvas.gradient(canvas.bounds(), top, bottom);

        let area = canvas.bounds().inset(PAD);
        let header = nexus_ui::LINE_HEIGHT + 4;
        let footer = nexus_ui::LINE_HEIGHT * 2 + 4;

        // Which picture this is, out of how many.
        if let Some(found) = self.files.get(self.at) {
            let line = nexus_i18n::format(
                "view.which",
                &[
                    ("at", &(self.at + 1)),
                    ("of", &self.files.len()),
                    ("name", &found.name),
                    ("kind", &found.kind),
                ],
            );
            canvas.text(area.x, area.y, &line, ink);
        }

        // Where the recording is, drawn at the right of the same line so that
        // it does not move as the numbers change width.
        if let Some(reel) = &self.reel {
            let key = if reel.playing {
                "view.frame"
            } else {
                "view.paused"
            };
            let where_in = nexus_i18n::format(
                key,
                &[("at", &(reel.at + 1)), ("count", &reel.frames.len())],
            );
            let width = nexus_ui::font::measure(&where_in);
            canvas.text(
                area.x + area.width.saturating_sub(width),
                area.y,
                &where_in,
                accent,
            );
        }

        let stage = Rect::new(
            area.x,
            area.y + header,
            area.width,
            area.height.saturating_sub(header + footer),
        );

        match &self.picture {
            // A recording is made bigger to fill the window; a picture is not.
            // The difference is what the person wants: a photograph blown up
            // is a worse view of the photograph, and a recording shown at a
            // quarter of the window is not a view of it at all.
            Some(picture) => draw_fitted(canvas, picture, stage, self.reel.is_some()),
            None => {
                canvas.text_centred(stage, nexus_i18n::text("view.nopicture"), quiet);
            }
        }

        if !self.said.is_empty() {
            let colour = if self.trouble {
                Colour::rgb(0xE0, 0x80, 0x70)
            } else {
                accent
            };
            canvas.text(area.x, stage.y + stage.height + 2, &self.said, colour);
        }
        canvas.text(
            area.x,
            area.y + area.height.saturating_sub(nexus_ui::LINE_HEIGHT),
            nexus_i18n::text(if self.reel.is_some() {
                "view.videokeys"
            } else {
                "view.keys"
            }),
            quiet,
        );
    }

    /// When the next frame is due, or nothing when there is no recording
    /// playing.
    ///
    /// Asked again on every pass of the window's loop, so this is the time
    /// until the *next* frame rather than a fixed interval -- which is what
    /// lets a decode that took most of an interval be followed by a short
    /// wait rather than a whole one.
    fn tick_ms(&mut self) -> Option<u64> {
        let reel = self.reel.as_ref()?;
        if !reel.playing {
            return None;
        }
        // Never zero. A window asking to be woken in no time at all would spin
        // this program against the scheduler.
        Some(reel.due_in_ms().max(1))
    }

    fn ticked(&mut self) -> bool {
        self.advance_if_due()
    }

    fn key(&mut self, key: Key) -> bool {
        // The space bar plays and pauses, and does nothing when what is on
        // screen is a photograph.
        if key == Key::Character(' ') {
            let Some(reel) = self.reel.as_mut() else {
                return false;
            };
            reel.playing = !reel.playing;
            if reel.playing {
                // Starting again from now, not from whenever it was paused,
                // or the first frame after a pause would be overdue by the
                // length of the pause and the player would sprint to catch up.
                reel.due_ms = nexus_user::uptime();
            }
            let at = reel.at + 1;
            let count = reel.frames.len();
            let key = if reel.playing {
                "view.frame"
            } else {
                "view.paused"
            };
            let said = nexus_i18n::format(key, &[("at", &at), ("count", &count)]);
            self.say(&said);
            return true;
        }

        match key {
            Key::Move(Movement::Right | Movement::Down) => self.step(1),
            Key::Move(Movement::Left | Movement::Up) => self.step(-1),
            Key::Move(Movement::Home) => {
                if self.at == 0 {
                    return false;
                }
                self.at = 0;
                self.show();
                true
            }
            Key::Move(Movement::End) => {
                let last = self.files.len().saturating_sub(1);
                if self.at == last {
                    return false;
                }
                self.at = last;
                self.show();
                true
            }
            // Look again. The directory is a place other programs write.
            Key::Function(5) => {
                self.files.clear();
                self.at = 0;
                self.look_through();
                self.show();
                true
            }
            Key::Language => true,
            _ => false,
        }
    }
}

/// Draw a picture inside a rectangle, keeping its shape.
///
/// Nearest-neighbour and integer-only: for each pixel of the destination, work
/// out which source pixel it came from. Done this way round rather than by
/// walking the source, because walking the source leaves gaps when scaling up
/// and writes the same destination pixel repeatedly when scaling down.
///
/// `magnify` allows the result to be larger than the source. It is off for
/// pictures and on for recordings, and the asymmetry is deliberate: a
/// sixteen-pixel icon blown up to fill a window is not a better view of the
/// icon, while a recording is something a person is trying to watch.
///
/// Magnified, this is nearest-neighbour, which means blocky. A smooth scale
/// would be a fixed-point resampler -- this machine has no floating point --
/// and that is a real piece of work that belongs after there is something to
/// look at.
fn draw_fitted(canvas: &mut Canvas, picture: &nexus_image::Picture, into: Rect, magnify: bool) {
    if into.width == 0 || into.height == 0 || picture.width == 0 || picture.height == 0 {
        return;
    }

    // The scale, as a fraction of 65536, so the arithmetic stays in integers.
    // Never above one: a picture smaller than the window is shown at its own
    // size in the middle, which is what looking at a small picture should do.
    const ONE: u64 = 65536;
    // Sixteen times, so that a tiny recording in a large window is made
    // watchable without a single source pixel becoming a visible tile.
    const MOST: u64 = ONE * 16;
    let by_width = ONE * u64::from(into.width) / u64::from(picture.width);
    let by_height = ONE * u64::from(into.height) / u64::from(picture.height);
    let ceiling = if magnify { MOST } else { ONE };
    let scale = by_width.min(by_height).clamp(1, ceiling);

    let width = ((u64::from(picture.width) * scale) / ONE).max(1) as u32;
    let height = ((u64::from(picture.height) * scale) / ONE).max(1) as u32;
    let left = into.x + (into.width.saturating_sub(width)) / 2;
    let top = into.y + (into.height.saturating_sub(height)) / 2;

    for row in 0..height {
        // The source row this destination row samples. Multiplying first and
        // dividing after keeps the rounding even across the picture.
        let source_row = (u64::from(row) * u64::from(picture.height) / u64::from(height)) as u32;
        for column in 0..width {
            let source_column =
                (u64::from(column) * u64::from(picture.width) / u64::from(width)) as u32;
            if let Some(pixel) = picture.at(source_column, source_row) {
                canvas.set(left + column, top + row, Colour(pixel));
            }
        }
    }
}

/// Build a recording's table of contents by walking its chunk headers.
///
/// One eight-byte read per chunk, and no frame data. A minute of video is six
/// hundred reads of eight bytes -- about five kilobytes of I/O to learn where
/// eight megabytes of frames are, which is the whole reason this is done with a
/// handle rather than by reading the file.
///
/// Stops at the first header it cannot read. A recording cut off mid-copy is
/// still most of a recording, and the frames found before the cut all play.
fn walk_frames(file: Handle, from: u64, to: u64) -> Vec<nexus_image::avi::Frame> {
    let mut frames = Vec::new();
    let mut at = from;
    let mut header = [0u8; 8];
    while at + 8 <= to && frames.len() < MOST_FRAMES {
        if nexus_user::read_at(file, at, &mut header) != Ok(header.len()) {
            break;
        }
        let (kind, length) = nexus_image::avi::chunk(&header);
        // A chunk claiming to run past the end of the list it is in. Stopping
        // keeps what came before rather than seeking into whatever follows.
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
        // A zero-length chunk would leave `at` where it was and spin. Eight
        // bytes of header always move it, so this can only fail on overflow.
        if next <= at {
            break;
        }
        at = next;
    }
    frames
}

/// Up to `most` bytes of a file, if it is there and readable.
fn read_some(directory: Handle, name: &str, most: usize) -> Option<Vec<u8>> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(most);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    Some(bytes)
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
    nexus_user::log("view: PANIC").ok();
    nexus_user::exit_with(2)
}
