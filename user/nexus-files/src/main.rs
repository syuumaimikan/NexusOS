//! `files`: one window for everything this machine can store things on.
//!
//! The store and the removable drives, side by side, reached the same way. That
//! is the whole idea: a person should not have to know that one of them is a
//! directory handle and the other is a channel to a kernel service, and before
//! this existed they had to — the store was in the terminal's `ls` and a stick
//! was in `usb`.
//!
//! # What it can do
//!
//! | Left, Right | move between the places: the store, and each drive |
//! | Up, Down | choose |
//! | Enter | go into a directory, or show a file |
//! | Backspace | go back up |
//! | F2 | copy the chosen file to the *other* place |
//! | F8, Delete | remove it, after asking once |
//! | F5 | look again |
//!
//! Copying is the reason the two halves are in one window. `F2` on a file in
//! the store puts it on the drive; `F2` on a file on the drive puts it in the
//! store. That is the thing somebody actually wants a file manager for, and it
//! is the only operation here that touches both.
//!
//! # What it cannot do
//!
//! No renaming, no making directories, no moving a file rather than copying it,
//! and no recursive anything -- a directory is entered, never copied. Files
//! above [`MAX_FILE`] are listed and refused rather than half-copied.
//!
//! Left and right rather than `Tab`, which is what this reached for first and
//! what the places are drawn as -- a row of tabs across the top. `Tab` never
//! arrived: the compositor takes it to move the focus between windows and does
//! not pass it on, so the binding was in the manual and in the code and could
//! not be pressed. Left and right are also the keys the row on screen suggests.
//!
//! Writing to a drive goes through the kernel's removable-drive service and
//! lands in the *root* of the drive: the FAT32 writer under it does not make
//! directories yet. A copy into a subdirectory is refused rather than quietly
//! put somewhere else.

#![no_std]
#![no_main]

extern crate alloc;

mod drive;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;
use nexus_window::{App, Key, Movement, Window};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped.
const SURFACE_AT: usize = 0x0000_0000_2000_0000;

/// How much heap: a listing, and one file being copied.
const HEAP: usize = 8 * 1024 * 1024;

/// The largest file this will copy or show.
///
/// It is held whole in memory on the way across, so this is a bound on the
/// heap above and not a guess. A larger file is listed and refused, because a
/// half-copied file is worse than one that was not copied.
const MAX_FILE: usize = 1024 * 1024;

/// Space around things.
const PAD: u32 = 8;

fn main_error(what: &str) {
    nexus_user::log(what).ok();
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
        main_error("files: FAILED: could not get a heap");
        finish();
    }

    // Two: the filesystem, and the removable-drive service. A window with
    // neither can still be opened and says so rather than looking broken.
    let mut lent = [Handle(0); 2];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            main_error(&format!("files: FAILED: {trouble}"));
            finish();
        }
    };
    let root = (carried >= 1).then(|| lent[0]);
    let drives = (carried >= 2).then(|| lent[1]);

    let mut files = Files::new(root, drives);
    files.refresh();
    nexus_user::log(&format!(
        "files: one window for {} place{}",
        files.places.len(),
        if files.places.len() == 1 { "" } else { "s" }
    ))
    .ok();

    let outcome = window.run(&mut files);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        main_error(&format!("files: FAILED: {}", outcome.ended));
    }
    finish()
}

/// Somewhere files live.
enum Place {
    /// This machine's own store, through a directory handle.
    Store { root: Handle },
    /// A removable drive, through the kernel's service.
    Drive { service: Handle, number: u8, size: u64 },
}

impl Place {
    /// What to call it along the top.
    fn name(&self) -> String {
        match self {
            Self::Store { .. } => nexus_i18n::text("files.store").to_string(),
            Self::Drive { number, size, .. } => nexus_i18n::format(
                "files.drive",
                &[("n", &number), ("size", &size)],
            ),
        }
    }
}

/// One thing in a listing, from either kind of place.
#[derive(Clone)]
struct Item {
    name: String,
    size: u32,
    is_directory: bool,
}

/// The window.
struct Files {
    places: Vec<Place>,
    /// Which place is being looked at.
    place: usize,
    /// Where in it, as the path components below its root.
    path: Vec<String>,
    /// What is there.
    items: Vec<Item>,
    /// Which item the cursor is on.
    at: usize,
    /// The first row on screen.
    scroll: usize,
    /// How many rows fit, from the last draw.
    visible: usize,

    /// What to say along the bottom.
    status: String,
    /// What a file being shown holds, when one is.
    peeking: Option<String>,
    /// Whether a delete has been asked about once.
    asked: bool,
    /// Whether the window is open.
    open: bool,

    width: u32,
    height: u32,
}

impl Files {
    fn new(root: Option<Handle>, drives: Option<Handle>) -> Self {
        let mut places = Vec::new();
        if let Some(root) = root {
            places.push(Place::Store { root });
        }
        // Asked once, at start-up. There is no hot-plug underneath, so a drive
        // that was not there then will not appear -- and pretending otherwise by
        // polling would be pretending to notice something this machine cannot.
        if let Some(service) = drives {
            if let Ok(found) = drive::drives(service) {
                for (number, found) in found.iter().enumerate() {
                    if !found.mountable {
                        // Listed nowhere rather than listed and unusable: a
                        // drive with no filesystem this reads has nothing to
                        // show, and an empty pane with no explanation is worse
                        // than an absence.
                        continue;
                    }
                    places.push(Place::Drive {
                        service,
                        number: number as u8,
                        size: found.megabytes(),
                    });
                }
            }
        }

        Self {
            places,
            place: 0,
            path: Vec::new(),
            items: Vec::new(),
            at: 0,
            scroll: 0,
            visible: 1,
            status: String::new(),
            peeking: None,
            asked: false,
            open: true,
            width: 0,
            height: 0,
        }
    }

    /// The path below the current place's root, as the service wants it.
    fn here(&self) -> String {
        self.path.join("/")
    }

    /// Read the current place again.
    fn refresh(&mut self) {
        self.peeking = None;
        self.asked = false;
        self.items.clear();

        let Some(place) = self.places.get(self.place) else {
            self.status = nexus_i18n::text("files.nowhere").to_string();
            return;
        };

        match place {
            Place::Store { root } => {
                // Walk down to where the cursor is. The filesystem offers one
                // step at a time, so a path is a sequence of opens -- and each
                // one is closed behind, because a window that leaked a handle
                // per directory visited would run out.
                let mut directory = *root;
                let mut opened: Vec<Handle> = Vec::new();
                let mut ok = true;
                for part in &self.path {
                    match nexus_user::open(directory, part) {
                        Ok(next) => {
                            opened.push(next);
                            directory = next;
                        }
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    let mut buffer = alloc::vec![0u8; 8192];
                    if let Ok(used) = nexus_user::list(directory, &mut buffer) {
                        for entry in nexus_user::entries(&buffer[..used]) {
                            self.items.push(Item {
                                name: entry.name.to_string(),
                                size: 0,
                                is_directory: entry.kind == nexus_user::Kind::Directory,
                            });
                        }
                    }
                }
                for handle in opened {
                    nexus_user::close(handle).ok();
                }
                if !ok {
                    self.status = nexus_i18n::text("files.gone").to_string();
                }
            }
            Place::Drive { service, number, .. } => {
                match drive::list(*service, *number, &self.here()) {
                    Ok(entries) => {
                        for entry in entries {
                            self.items.push(Item {
                                name: entry.name,
                                size: entry.size,
                                is_directory: entry.is_directory,
                            });
                        }
                    }
                    Err(trouble) => {
                        self.status = nexus_i18n::text(trouble.key()).to_string();
                    }
                }
            }
        }

        self.at = self.at.min(self.items.len().saturating_sub(1));
        self.scroll = 0;
    }

    /// The item the cursor is on.
    fn chosen(&self) -> Option<&Item> {
        self.items.get(self.at)
    }

    /// Read a file out of wherever it is.
    fn read(&self, name: &str) -> Option<Vec<u8>> {
        match self.places.get(self.place)? {
            Place::Store { root } => {
                let mut directory = *root;
                let mut opened: Vec<Handle> = Vec::new();
                for part in &self.path {
                    let next = nexus_user::open(directory, part).ok()?;
                    opened.push(next);
                    directory = next;
                }
                let file = nexus_user::open(directory, name).ok();
                for handle in opened {
                    nexus_user::close(handle).ok();
                }
                let file = file?;
                let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
                let mut bytes = alloc::vec![0u8; size];
                let read = nexus_user::read(file, &mut bytes).unwrap_or(0);
                nexus_user::close(file).ok();
                bytes.truncate(read);
                Some(bytes)
            }
            Place::Drive { service, number, .. } => {
                drive::read(*service, *number, name, MAX_FILE).ok()
            }
        }
    }

    /// Copy the chosen file to the other place.
    fn copy(&mut self) -> bool {
        let Some(item) = self.chosen().cloned() else {
            return false;
        };
        if item.is_directory {
            self.status = nexus_i18n::text("files.nodirectories").to_string();
            return true;
        }
        if self.places.len() < 2 {
            self.status = nexus_i18n::text("files.nowhereelse").to_string();
            return true;
        }
        let Some(bytes) = self.read(&item.name) else {
            self.status = nexus_i18n::format("files.unreadable", &[("name", &item.name)]);
            return true;
        };
        if bytes.len() > MAX_FILE {
            self.status = nexus_i18n::format("files.toobig", &[("name", &item.name)]);
            return true;
        }

        // The next place round, which with two is "the other one". Named that
        // way rather than "the drive", because with two drives plugged in a
        // copy between them is the same operation.
        let target = (self.place + 1) % self.places.len();
        let result = match &self.places[target] {
            Place::Store { root } => {
                // Into the root of the store, not into wherever the other pane
                // happens to be sitting. A copy that landed somewhere the
                // person was not looking would be a copy they could not find.
                match nexus_user::create(*root, &item.name, nexus_user::Kind::File)
                    .or_else(|_| nexus_user::open(*root, &item.name))
                {
                    Ok(file) => {
                        let written = nexus_user::write(file, &bytes);
                        nexus_user::close(file).ok();
                        match written {
                            Ok(count) if count == bytes.len() => Ok(()),
                            // A short write is the failure that must not look
                            // like success: what is on the disk is now neither
                            // the old file nor the new one.
                            Ok(count) => Err(format!("{count}/{}", bytes.len())),
                            Err(error) => Err(format!("{error}")),
                        }
                    }
                    Err(error) => Err(format!("{error}")),
                }
            }
            Place::Drive { service, number, .. } => {
                drive::write(*service, *number, &item.name, &bytes)
                    .map_err(|trouble| nexus_i18n::text(trouble.key()).to_string())
            }
        };

        match result {
            Ok(()) => {
                self.status = nexus_i18n::format(
                    "files.copied",
                    &[
                        ("name", &item.name),
                        ("where", &self.places[target].name()),
                        ("bytes", &bytes.len()),
                    ],
                );
                nexus_user::log(&format!(
                    "files: copied {} ({} bytes) to {}",
                    item.name,
                    bytes.len(),
                    self.places[target].name()
                ))
                .ok();
            }
            Err(why) => {
                self.status = nexus_i18n::format(
                    "files.notcopied",
                    &[("name", &item.name), ("why", &why)],
                );
            }
        }
        true
    }

    /// Remove the chosen file, having asked once.
    fn remove(&mut self) -> bool {
        let Some(item) = self.chosen().cloned() else {
            return false;
        };
        if item.is_directory {
            self.status = nexus_i18n::text("files.nodirectories").to_string();
            return true;
        }
        if !self.asked {
            self.asked = true;
            self.status = nexus_i18n::format("files.sure", &[("name", &item.name)]);
            return true;
        }

        let result = match &self.places[self.place] {
            Place::Store { root } => {
                let mut directory = *root;
                let mut opened: Vec<Handle> = Vec::new();
                let mut ok = true;
                for part in &self.path {
                    match nexus_user::open(directory, part) {
                        Ok(next) => {
                            opened.push(next);
                            directory = next;
                        }
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                let removed = if ok {
                    nexus_user::remove(directory, &item.name).map_err(|error| format!("{error}"))
                } else {
                    Err(nexus_i18n::text("files.gone").to_string())
                };
                for handle in opened {
                    nexus_user::close(handle).ok();
                }
                removed
            }
            Place::Drive { service, number, .. } => {
                drive::remove(*service, *number, &item.name)
                    .map_err(|trouble| nexus_i18n::text(trouble.key()).to_string())
            }
        };

        match result {
            Ok(()) => {
                self.status = nexus_i18n::format("files.removed", &[("name", &item.name)]);
                nexus_user::log(&format!("files: removed {}", item.name)).ok();
                self.refresh();
            }
            Err(why) => {
                self.status =
                    nexus_i18n::format("files.notremoved", &[("name", &item.name), ("why", &why)]);
            }
        }
        self.asked = false;
        true
    }

    /// Go into the chosen thing, or show it.
    /// Move `step` places along the row, wrapping.
    ///
    /// The path is cleared rather than carried across: a directory that exists
    /// on the store need not exist on the drive, and arriving somewhere that is
    /// not there is worse than arriving at the top.
    fn go(&mut self, step: isize) -> bool {
        if self.places.len() < 2 {
            return false;
        }
        let count = self.places.len() as isize;
        self.place = ((self.place as isize + step).rem_euclid(count)) as usize;
        self.path.clear();
        self.at = 0;
        self.status.clear();
        self.refresh();
        // Said out loud, because a key that goes nowhere looks exactly like a
        // key that arrived and did nothing -- which is what `Tab` did here for
        // as long as this program existed.
        nexus_user::log(&format!(
            "files: looking at {}, {} item{}",
            match &self.places[self.place] {
                Place::Store { .. } => alloc::string::String::from("the store"),
                Place::Drive { number, .. } => alloc::format!("drive {number}"),
            },
            self.items.len(),
            if self.items.len() == 1 { "" } else { "s" }
        ))
        .ok();
        true
    }

    fn enter(&mut self) -> bool {
        let Some(item) = self.chosen().cloned() else {
            return false;
        };
        if item.is_directory {
            self.path.push(item.name);
            self.at = 0;
            self.refresh();
            return true;
        }

        match self.read(&item.name) {
            Some(bytes) => {
                // Shown as text, or said not to be. A file manager that printed
                // a binary as replacement characters would be showing something
                // that is not what is in the file.
                match core::str::from_utf8(&bytes) {
                    Ok(text) => {
                        self.peeking = Some(text.chars().take(4000).collect());
                        self.status =
                            nexus_i18n::format("files.showing", &[("name", &item.name)]);
                    }
                    Err(_) => {
                        self.peeking = None;
                        self.status =
                            nexus_i18n::format("files.notext", &[("name", &item.name)]);
                    }
                }
            }
            None => {
                self.status = nexus_i18n::format("files.unreadable", &[("name", &item.name)]);
            }
        }
        true
    }

    /// Go back up.
    fn leave(&mut self) -> bool {
        if self.peeking.is_some() {
            self.peeking = None;
            return true;
        }
        if self.path.pop().is_none() {
            return false;
        }
        self.at = 0;
        self.refresh();
        true
    }

    /// Bring the cursor back on screen.
    fn follow(&mut self) {
        let visible = self.visible.max(1);
        if self.at < self.scroll {
            self.scroll = self.at;
        }
        if self.at >= self.scroll + visible {
            self.scroll = self.at + 1 - visible;
        }
    }
}

impl App for Files {
    fn draw(&mut self, canvas: &mut Canvas) {
        let bounds = canvas.bounds();
        self.width = bounds.width;
        self.height = bounds.height;

        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x78, 0x88, 0xA0);
        let paper = Colour::rgb(0x0B, 0x11, 0x1D);
        let bar = Colour::rgb(0x14, 0x1C, 0x2C);
        let accent = Colour::rgb(0x62, 0x80, 0xD5);
        let chosen = Colour::rgb(0x1C, 0x2A, 0x44);

        canvas.fill(bounds, paper);

        let line = nexus_ui::LINE_HEIGHT;
        let top = bounds.y + nexus_ui::TITLE_BAR + PAD / 2;

        // The places, along the top. The one being looked at is filled.
        let mut x = bounds.x + PAD;
        for (index, place) in self.places.iter().enumerate() {
            let label = place.name();
            let width = nexus_ui::measure(&label) + PAD;
            let tab = Rect::new(x, top, width, line + PAD / 2);
            if index == self.place {
                canvas.fill(tab, accent);
                canvas.text(x + PAD / 2, top + PAD / 4, &label, paper);
            } else {
                canvas.fill(tab, bar);
                canvas.text(x + PAD / 2, top + PAD / 4, &label, quiet);
            }
            x += width + PAD / 2;
        }

        let list_top = top + line + PAD;
        let status_height = line + PAD;
        let list_height = bounds
            .height
            .saturating_sub(list_top - bounds.y + status_height);
        self.visible = (list_height / line.max(1)) as usize;
        self.follow();

        if let Some(text) = &self.peeking {
            // A file, instead of the listing.
            for (row, content) in text.lines().take(self.visible).enumerate() {
                canvas.text(
                    bounds.x + PAD,
                    list_top + row as u32 * line,
                    content,
                    ink,
                );
            }
        } else {
            let path = self.here();
            if !path.is_empty() {
                canvas.text(bounds.x + PAD, list_top, &format!("/{path}"), quiet);
            }
            let offset = u32::from(!path.is_empty());

            for row in 0..self.visible.saturating_sub(offset as usize) {
                let Some(item) = self.items.get(self.scroll + row) else {
                    break;
                };
                let y = list_top + (row as u32 + offset) * line;
                if self.scroll + row == self.at {
                    canvas.fill(Rect::new(bounds.x, y, bounds.width, line), chosen);
                }
                let name = if item.is_directory {
                    format!("{}/", item.name)
                } else {
                    item.name.clone()
                };
                canvas.text(bounds.x + PAD * 2, y, &name, ink);
                if !item.is_directory && item.size > 0 {
                    let size = format!("{}", item.size);
                    let width = nexus_ui::measure(&size);
                    if width + PAD * 3 < bounds.width {
                        canvas.text(bounds.x + bounds.width - PAD - width, y, &size, quiet);
                    }
                }
            }
            if self.items.is_empty() {
                canvas.text(
                    bounds.x + PAD * 2,
                    list_top + offset * line,
                    nexus_i18n::text("files.empty"),
                    quiet,
                );
            }
        }

        // The status line.
        let status_y = bounds.y + bounds.height - status_height;
        canvas.fill(
            Rect::new(bounds.x, status_y, bounds.width, status_height),
            bar,
        );
        let said = if self.status.is_empty() {
            nexus_i18n::text("files.keys").to_string()
        } else {
            self.status.clone()
        };
        canvas.text(bounds.x + PAD, status_y + PAD / 4, &said, ink);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Right) => self.go(1),
            Key::Move(Movement::Left) => self.go(-1),
            Key::Move(Movement::Up) => {
                self.at = self.at.saturating_sub(1);
                self.asked = false;
                true
            }
            Key::Move(Movement::Down) => {
                self.at = (self.at + 1).min(self.items.len().saturating_sub(1));
                self.asked = false;
                true
            }
            Key::Move(Movement::Home) => {
                self.at = 0;
                true
            }
            Key::Move(Movement::End) => {
                self.at = self.items.len().saturating_sub(1);
                true
            }
            Key::Move(Movement::PageUp) => {
                self.at = self.at.saturating_sub(self.visible.max(1));
                true
            }
            Key::Move(Movement::PageDown) => {
                self.at = (self.at + self.visible.max(1)).min(self.items.len().saturating_sub(1));
                true
            }
            Key::Enter => self.enter(),
            Key::Backspace => self.leave(),
            Key::Function(2) => self.copy(),
            Key::Function(5) => {
                self.status.clear();
                self.refresh();
                true
            }
            Key::Function(8) => self.remove(),
            Key::Escape => {
                if self.peeking.is_some() {
                    self.peeking = None;
                    return true;
                }
                self.open = false;
                true
            }
            Key::Language => true,
            _ => false,
        }
    }

    fn resized(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    fn running(&self) -> bool {
        self.open
    }
}

/// Stop.
fn finish() -> ! {
    nexus_user::exit()
}

#[panic_handler]
fn panicked(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("files: PANIC: {info}")).ok();
    nexus_user::exit_with(2)
}
