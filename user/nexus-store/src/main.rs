//! `store`: a window for the packages this machine has, and the ones it could.
//!
//! The mechanism has been here for a while and there was nowhere to see it. A
//! package is a signed archive in `PKG/`; the installer verifies it, writes it,
//! reads back what it wrote, and puts everything back if any of that fails; the
//! updater does the same thing for the packages that are newer than what is on
//! record. None of it had a window, so the only way to install something was to
//! type a command or to wait for the updater to decide.
//!
//! # What it shows
//!
//! One row per package in `PKG/`, with what it says it is, whether it is signed
//! by the key this machine trusts, and how its version compares with what is
//! recorded as installed. The comparison is the interesting column: a list of
//! files tells somebody nothing they could not get from the directory.
//!
//! # What installing here means
//!
//! Exactly what it means anywhere else on this machine, because it is the same
//! code: [`nexus_install::install`]. This window is not a second implementation
//! of installing, it is a list with a key bound to the first one. A package
//! manager whose window installed things a different way from its updater would
//! be a machine with two ideas about what is on it.
//!
//! Afterwards the record in `installed.txt` is updated, which is what makes the
//! next run of the updater agree with what this window just did.
//!
//! # What it is given
//!
//! Two handles: the filesystem, to read packages and write the files in them,
//! and the settings directory, where the record of what is installed lives.
//! Both are authority and neither is ambient — without the first this program
//! cannot open a file at all, and what it can do with it is bounded by the
//! rights on the handle rather than by who started it.

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
const SURFACE_AT: usize = 0x0000_0000_2C00_0000;

/// How much heap: a package, the files inside it, and the copies an install
/// keeps in case it has to put them back.
const HEAP: usize = 4 * 1024 * 1024;

/// The directory packages live in, inside the filesystem this was handed.
const PACKAGES: &str = "PKG";

/// What the record of installed packages is called, in the settings directory.
const INSTALLED_NAME: &str = "installed.txt";

/// And the settings file itself, read only for the colours this window is drawn
/// in. A store that did not match the rest of the machine would be a store that
/// looks like it came from somewhere else.
const SETTINGS_NAME: &str = "settings.txt";

/// The largest package this will read.
const MAX_PACKAGE: usize = 2 * 1024 * 1024;

/// The most packages it will list.
///
/// A bound rather than a policy: every row costs a package read and verified,
/// and a directory somebody filled would otherwise be a window that takes a
/// minute to open.
const MAX_ROWS: usize = 64;

/// Space around the text.
const PAD: u32 = 10;

/// Where each column starts, in pixels from the left of the list.
const NAME_AT: u32 = 8;
const VERSION_AT: u32 = 220;
const SIGNATURE_AT: u32 = 330;
const VERDICT_AT: u32 = 460;
const FILE_AT: u32 = 640;

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
        failed("store: FAILED: could not get a heap");
        finish();
    }

    let mut lent = [Handle(0); 2];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("store: FAILED: {trouble}"));
            finish();
        }
    };

    let mut store = Store::new(
        (carried >= 1).then_some(lent[0]),
        (carried >= 2).then_some(lent[1]),
    );
    nexus_user::log("store: the packages this machine has, and the ones it could").ok();

    let outcome = window.run(&mut store);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        // Reported rather than exited over: the window is gone either way, and
        // which way is the only thing that distinguishes a machine shutting
        // down from a compositor that stopped answering.
        failed(&format!("store: FAILED: {}", outcome.ended));
    }
    if outcome.frames > 0 {
        nexus_user::log("store: listed what there is to install").ok();
    }
    finish()
}

/// What this machine thinks of a package it found.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Signed by the key this machine trusts.
    Trusted,
    /// Nobody claimed anything about it.
    Unsigned,
    /// Somebody claimed something that does not hold.
    Refused,
    /// It would not even parse.
    Unreadable,
}

impl Standing {
    /// The translation key for how it is said.
    const fn label(self) -> &'static str {
        match self {
            Self::Trusted => "store.trusted",
            Self::Unsigned => "store.unsigned",
            Self::Refused => "store.refused",
            Self::Unreadable => "store.unreadable",
        }
    }

    /// Whether this machine will install it.
    const fn installable(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

/// One package found in `PKG/`.
struct Found {
    /// The file it is in, which is how it is installed.
    file: String,
    /// What it says it is called.
    name: String,
    /// And what release, if it names one this system understands.
    release: Option<nexus_update::Version>,
    /// What was made of its signature.
    standing: Standing,
}

/// The window.
struct Store {
    /// The filesystem, if this program was lent one.
    root: Option<Handle>,
    /// Where the record of what is installed lives.
    settings: Option<Handle>,
    /// What is on record.
    installed: nexus_update::Installed,
    /// What was found in `PKG/`.
    rows: Vec<Found>,
    /// Which row the cursor is on.
    cursor: usize,
    /// What happened last, shown at the bottom.
    said: String,
    /// Whether that went wrong, which decides its colour.
    trouble: bool,
    /// What the machine looks like, so this window matches it.
    look: nexus_look::Look,
    /// Whether the record of what is installed could be read.
    ///
    /// False refuses installing. An install updates that record, and updating a
    /// record this program could not read means writing one that says nothing
    /// else is installed.
    recorded: bool,
}

impl Store {
    fn new(root: Option<Handle>, settings: Option<Handle>) -> Self {
        let look = settings
            .and_then(|handle| read_text(handle, SETTINGS_NAME))
            .map(|text| nexus_look::Look::parse(&text))
            .unwrap_or_default();

        let mut store = Self {
            root,
            settings,
            installed: nexus_update::Installed::new(),
            rows: Vec::new(),
            cursor: 0,
            said: String::new(),
            trouble: false,
            look,
            recorded: true,
        };
        store.reread();
        if root.is_none() {
            store.complain(nexus_i18n::text("store.nofiles"));
        }
        store
    }

    /// Read the record and look through `PKG/` again.
    ///
    /// Done at startup and after every install, because an install changes both
    /// halves of what this window shows: the record it compares against, and —
    /// if the package that was installed replaced the installer or the updater —
    /// possibly the files themselves.
    fn reread(&mut self) {
        match self
            .settings
            .map(|handle| read_record(handle, INSTALLED_NAME))
        {
            Some(Ok(Some(text))) => {
                self.installed = nexus_update::Installed::parse(&text);
                self.recorded = true;
            }
            Some(Ok(None)) | None => {
                self.installed = nexus_update::Installed::new();
                self.recorded = true;
            }
            Some(Err(why)) => {
                // Left as it was, and installing is refused. A record that will
                // not read is not an empty record, and writing a fresh one
                // would tell the updater that everything here is uninstalled.
                self.recorded = false;
                self.complain(&why);
            }
        }
        self.rows = self.look_through();
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Every package in `PKG/`, read and judged.
    fn look_through(&self) -> Vec<Found> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        let Ok(directory) = nexus_user::open(root, PACKAGES) else {
            return Vec::new();
        };

        let mut packed = alloc::vec![0u8; 8 * 1024];
        let listed = nexus_user::list(directory, &mut packed).unwrap_or(0);
        packed.truncate(listed);

        let mut found = Vec::new();
        for entry in nexus_user::entries(&packed) {
            if found.len() >= MAX_ROWS {
                break;
            }
            if entry.kind != Kind::File {
                continue;
            }
            let name = entry.name.to_string();
            found.push(judge(directory, &name));
        }
        nexus_user::close(directory).ok();

        // By name, then by version, so that two releases of one package sit
        // beside each other rather than wherever the directory happened to put
        // them.
        found.sort_by(|one, other| {
            one.name
                .cmp(&other.name)
                .then(one.release.cmp(&other.release))
        });
        found
    }

    /// What to say about a row's version, against what is installed.
    fn verdict(&self, row: &Found) -> Option<nexus_update::Verdict> {
        let release = row.release?;
        Some(nexus_update::judge(self.installed.get(&row.name), release))
    }

    fn say(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = false;
    }

    fn complain(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = true;
    }

    /// Install what the cursor is on.
    fn install(&mut self) -> bool {
        let Some(root) = self.root else {
            self.complain(nexus_i18n::text("store.nofiles"));
            return true;
        };
        let Some(row) = self.rows.get(self.cursor) else {
            return false;
        };
        if !self.recorded {
            self.complain(nexus_i18n::text("store.norecord"));
            return true;
        }
        if !row.standing.installable() {
            // Said here rather than attempted and reported, because the
            // installer would refuse it for the same reason and the message
            // would be about a signature rather than about why this machine
            // will not do it.
            self.complain(nexus_i18n::text("store.wontinstall"));
            return true;
        }

        let path = format!("{PACKAGES}/{}", row.file);
        let (name, release) = (row.name.clone(), row.release);
        self.say(&nexus_i18n::format("store.installing", &[("name", &name)]));

        match nexus_install::install(root, &path) {
            Ok(report) => {
                nexus_user::log(&format!("store: installed {name} from {path}")).ok();
                // The record, so that the updater and this window agree about
                // what is on the machine. Only when a version was actually
                // stated: a package with no release is installed but not
                // recorded, because there is nothing to record.
                if let Some(release) = release {
                    self.installed.set(&name, release);
                    if let Some(settings) = self.settings {
                        if let Err(why) =
                            write_text(settings, INSTALLED_NAME, &self.installed.to_text())
                        {
                            self.complain(&why);
                            return true;
                        }
                    }
                }
                self.reread();
                self.say(&report);
            }
            Err(nexus_install::Trouble::Refused(why)) => {
                nexus_user::log(&format!("store: refused {name}: {why}")).ok();
                self.complain(&nexus_i18n::format("store.norefuse", &[("why", &why)]));
            }
            Err(nexus_install::Trouble::Broken(why)) => {
                nexus_user::log(&format!("store: could not install {name}: {why}")).ok();
                self.complain(&nexus_i18n::format("store.nobroken", &[("why", &why)]));
            }
        }
        true
    }
}

/// Read one package and decide what this machine makes of it.
fn judge(directory: Handle, file: &str) -> Found {
    let unreadable = |file: &str| Found {
        file: file.to_string(),
        name: file.to_string(),
        release: None,
        standing: Standing::Unreadable,
    };

    let Some(bytes) = read_bytes(directory, file, MAX_PACKAGE) else {
        return unreadable(file);
    };
    let Ok(package) = nexus_pkg::Package::open(&bytes) else {
        return unreadable(file);
    };

    let name = package.name().unwrap_or(file).to_string();
    let release = package
        .release()
        .ok()
        .and_then(nexus_update::Version::parse);

    // Unsigned and wrongly signed are different facts and are reported
    // differently: one is nobody having claimed anything, the other is a claim
    // that does not hold. A window that said "bad signature" for both would
    // make an unsigned package look like a tampered one.
    let standing = if nexus_pkg::is_unsigned(&package.signature()) {
        Standing::Unsigned
    } else if package
        .verify_signed_by(&nexus_install::TRUSTED_KEY)
        .is_ok()
    {
        Standing::Trusted
    } else {
        Standing::Refused
    };

    Found {
        file: file.to_string(),
        name,
        release,
        standing,
    }
}

impl App for Store {
    fn draw(&mut self, canvas: &mut Canvas) {
        let top = Colour(self.look.top.packed());
        let bottom = Colour(self.look.bottom.packed());
        let accent = Colour(self.look.accent.packed());
        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x8A, 0x9A, 0xB4);
        let bad = Colour::rgb(0xE0, 0x80, 0x70);

        canvas.gradient(canvas.bounds(), top, bottom);

        let area = canvas.bounds().inset(PAD);
        let mut column = nexus_ui::Column::new(area, 4);

        let title = column.row(nexus_ui::LINE_HEIGHT + 6);
        canvas.text(title.x, title.y + 3, nexus_i18n::text("store.title"), ink);

        let heading = column.line();
        canvas.text(
            heading.x + NAME_AT,
            heading.y,
            nexus_i18n::text("store.package"),
            quiet,
        );
        canvas.text(
            heading.x + VERSION_AT,
            heading.y,
            nexus_i18n::text("store.version"),
            quiet,
        );
        canvas.text(
            heading.x + SIGNATURE_AT,
            heading.y,
            nexus_i18n::text("store.standing"),
            quiet,
        );
        canvas.text(
            heading.x + VERDICT_AT,
            heading.y,
            nexus_i18n::text("store.state"),
            quiet,
        );
        canvas.text(
            heading.x + FILE_AT,
            heading.y,
            nexus_i18n::text("store.file"),
            quiet,
        );

        if self.rows.is_empty() {
            let line = column.line();
            canvas.text(
                line.x + NAME_AT,
                line.y,
                nexus_i18n::text("store.nothing"),
                quiet,
            );
        }

        for index in 0..self.rows.len() {
            let line = column.line();
            if line.width == 0 {
                break;
            }
            let chosen = index == self.cursor;
            if chosen {
                canvas.fill(
                    Rect::new(line.x, line.y, line.width, line.height),
                    blend(bottom, accent, 64),
                );
            }

            let row = &self.rows[index];
            canvas.text(line.x + NAME_AT, line.y, &row.name, ink);

            let version = match row.release {
                Some(release) => release.to_text(),
                None => nexus_i18n::text("store.noversion").to_string(),
            };
            canvas.text(line.x + VERSION_AT, line.y, &version, ink);

            let colour = if row.standing.installable() { ink } else { bad };
            let standing = nexus_i18n::text(row.standing.label());
            canvas.text(line.x + SIGNATURE_AT, line.y, standing, colour);

            if let Some(verdict) = self.verdict(row) {
                let said = nexus_i18n::text(match verdict {
                    nexus_update::Verdict::New => "store.new",
                    nexus_update::Verdict::Newer => "store.newer",
                    nexus_update::Verdict::Current => "store.current",
                    nexus_update::Verdict::Older => "store.older",
                });
                let colour = if verdict.worth_doing() { accent } else { quiet };
                canvas.text(line.x + VERDICT_AT, line.y, said, colour);
            }

            canvas.text(line.x + FILE_AT, line.y, &row.file, quiet);
        }

        let footer = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(nexus_ui::LINE_HEIGHT * 2 + 4),
            area.width,
            nexus_ui::LINE_HEIGHT * 2 + 4,
        );
        if !self.said.is_empty() {
            let colour = if self.trouble { bad } else { accent };
            canvas.text(footer.x, footer.y, &self.said, colour);
        }
        canvas.text(
            footer.x,
            footer.y + nexus_ui::LINE_HEIGHT + 4,
            nexus_i18n::text("store.keys"),
            quiet,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Up) => match self.cursor.checked_sub(1) {
                Some(at) => {
                    self.cursor = at;
                    true
                }
                None => false,
            },
            Key::Move(Movement::Down) => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                    return true;
                }
                false
            }
            Key::Enter => self.install(),
            // Look again. Worth a key of its own because the directory is a
            // place other programs write: the updater puts packages there, and
            // so does anything that fetches one.
            Key::Function(5) => {
                self.reread();
                self.say(nexus_i18n::text("store.looked"));
                true
            }
            Key::Language => true,
            _ => false,
        }
    }
}

// -- files -------------------------------------------------------------------

/// A whole file out of a directory, up to `most` bytes.
///
/// `None` covers both "no such file" and "it would not read", which is the
/// right shape *here* and is worth saying why: every caller of this either
/// lists a package it can then leave out, or reads colours it has a default
/// for. Neither rewrites the file it just read. The record of what is
/// installed does, and it goes through [`read_record`] instead.
fn read_bytes(directory: Handle, name: &str, most: usize) -> Option<Vec<u8>> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).ok()?;
    if size > most {
        nexus_user::close(file).ok();
        return None;
    }
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    if read != size {
        return None;
    }
    Some(bytes)
}

/// The same, as text.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    String::from_utf8(read_bytes(directory, name, 64 * 1024)?).ok()
}

/// The record of what is installed, with "absent" and "unreadable" kept apart.
///
/// This one is rewritten after every install, so the two have to be different
/// answers: writing a fresh record because the old one would not read would
/// tell the updater that everything on the machine is uninstalled.
fn read_record(directory: Handle, name: &str) -> Result<Option<String>, String> {
    match nexus_user::open(directory, name) {
        Err(nexus_user::Error::NotFound) => Ok(None),
        Err(error) => Err(format!("{name}: {error}")),
        Ok(file) => {
            nexus_user::close(file).ok();
            match read_text(directory, name) {
                Some(text) => Ok(Some(text)),
                None => Err(nexus_i18n::format("file.unreadable", &[("name", &name)])),
            }
        }
    }
}

/// Replace a text file with this content.
fn write_text(directory: Handle, name: &str, text: &str) -> Result<(), String> {
    // Removed first: the filesystem has no truncate, so a shorter file written
    // over a longer one would keep the old ending.
    match nexus_user::remove(directory, name) {
        Ok(()) | Err(nexus_user::Error::NotFound) => {}
        Err(error) => return Err(format!("{name}: {error}")),
    }
    let file = nexus_user::create(directory, name, Kind::File)
        .map_err(|error| format!("{name}: {error}"))?;
    let contents = text.as_bytes();
    let mut written = 0;
    while written < contents.len() {
        match nexus_user::write_at(file, written as u64, &contents[written..]) {
            Ok(0) | Err(_) => {
                nexus_user::close(file).ok();
                return Err(nexus_i18n::format(
                    "file.stopped",
                    &[("name", &name), ("bytes", &written)],
                ));
            }
            Ok(count) => written += count,
        }
    }
    nexus_user::close(file).ok();
    Ok(())
}

/// Part of the way from one colour to another; `amount` is 0..=255.
fn blend(from: Colour, to: Colour, amount: u32) -> Colour {
    let mix = |shift: u32| {
        let one = (from.0 >> shift) & 0xFF;
        let other = (to.0 >> shift) & 0xFF;
        (one * (255 - amount) + other * amount) / 255
    };
    Colour(mix(16) << 16 | mix(8) << 8 | mix(0))
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
    nexus_user::log("store: PANIC").ok();
    nexus_user::exit_with(2)
}
