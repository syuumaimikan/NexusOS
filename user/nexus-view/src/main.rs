//! `view`: a window that shows pictures.
//!
//! It is given a directory and nothing else. It looks through it for files it
//! recognises — by their first bytes, not by their names, because a file called
//! `.png` that is a bitmap is a file somebody renamed — and shows one at a
//! time.
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

/// A file that might be a picture.
struct Found {
    name: String,
    /// What its first bytes say it is, for showing beside the name.
    kind: &'static str,
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
    /// The picture, once it has been decoded.
    picture: Option<nexus_image::Picture>,
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

    /// Decode whatever is selected.
    fn show(&mut self) {
        self.picture = None;
        let (Some(directory), Some(found)) = (self.directory, self.files.get(self.at)) else {
            return;
        };
        let name = found.name.clone();

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

        let stage = Rect::new(
            area.x,
            area.y + header,
            area.width,
            area.height.saturating_sub(header + footer),
        );

        match &self.picture {
            Some(picture) => draw_fitted(canvas, picture, stage),
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
            nexus_i18n::text("view.keys"),
            quiet,
        );
    }

    fn key(&mut self, key: Key) -> bool {
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
fn draw_fitted(canvas: &mut Canvas, picture: &nexus_image::Picture, into: Rect) {
    if into.width == 0 || into.height == 0 || picture.width == 0 || picture.height == 0 {
        return;
    }

    // The scale, as a fraction of 65536, so the arithmetic stays in integers.
    // Never above one: a picture smaller than the window is shown at its own
    // size in the middle, which is what looking at a small picture should do.
    const ONE: u64 = 65536;
    let by_width = ONE * u64::from(into.width) / u64::from(picture.width);
    let by_height = ONE * u64::from(into.height) / u64::from(picture.height);
    let scale = by_width.min(by_height).clamp(1, ONE);

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
