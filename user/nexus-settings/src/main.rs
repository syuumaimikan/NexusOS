//! `settings`: a window for the file that decides what this machine is.
//!
//! Everything it changes is a line in `system/settings.txt`, and everything
//! that reads that file goes on reading it the way it always did. There is no
//! settings service, no registry and nothing that has to be told: the wallpaper
//! looks every two seconds, the desktop on its tick, the kernel when it brings
//! the network up. Changing a setting here and watching the background change
//! without anything being restarted is the whole demonstration.
//!
//! # Why the rows are a table and not a screen each
//!
//! Because the interesting question about a setting is not how to present it,
//! it is whether anything reads it. Every row here names a key that something
//! in this system actually looks at — the style and the three colours, the
//! language, the timezone, whether updates install themselves, whether the
//! machine has a network at all, and whose machine it is. A window offering to
//! change something nothing reads would be a window that lies.
//!
//! # Two kinds of row
//!
//! A choice, which the left and right keys move through, and a typed value,
//! which is edited in place. A choice is written the moment it changes, because
//! there is nothing half-done about picking the next item in a list; a typed
//! value is written when Enter is pressed, because half a colour is not a
//! colour and saving on every keystroke would put `4`, `40`, `40d` into a file
//! three other programs are reading.

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
const SURFACE_AT: usize = 0x0000_0000_2800_0000;

/// How much heap: a settings file and the strings drawn from it.
const HEAP: usize = 512 * 1024;

/// What the settings file is called, inside the directory this was lent.
const SETTINGS_NAME: &str = "settings.txt";

/// The most of that file this will read.
const MAX_FILE: usize = 64 * 1024;

/// Space around the text.
const PAD: u32 = 10;

/// How far in from the left a value is drawn.
const VALUE_AT: u32 = 200;

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
        failed("settings: FAILED: could not get a heap");
        finish();
    }

    let mut lent = [Handle(0); 1];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("settings: FAILED: {trouble}"));
            finish();
        }
    };

    let directory = (carried >= 1).then_some(lent[0]);
    let mut settings = Settings::new(directory);
    nexus_user::log("settings: a window for what this machine is").ok();

    let outcome = window.run(&mut settings);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        // Reported rather than exited over: the window is gone either way, and
        // which way is the only thing that distinguishes a machine shutting
        // down from a compositor that stopped answering.
        failed(&format!("settings: FAILED: {}", outcome.ended));
    }
    if outcome.frames > 0 {
        nexus_user::log("settings: showed the machine its own settings").ok();
    }
    finish()
}

// -- the table ---------------------------------------------------------------

/// One line of the window.
enum Item {
    /// A heading, which nothing can be done to.
    Heading(&'static str),
    /// A setting.
    Row(Row),
}

/// A setting, and how it is changed.
struct Row {
    /// The key in the settings file.
    key: &'static str,
    /// The translation key for its label.
    label: &'static str,
    /// What it may be. Empty means it is typed rather than chosen.
    choices: Vec<String>,
    /// What an unset value reads as.
    fallback: &'static str,
    /// Whether it is a colour, which decides whether a swatch is drawn beside
    /// it. Nothing else about it is different: a colour is text in a file.
    colour: bool,
}

impl Row {
    /// A row with a list of answers. The fallback is what an unset key reads
    /// as, and it matters more here than anywhere: a choice showing nothing
    /// because the file has never mentioned it is a choice whose first press
    /// lands somewhere nobody expected.
    fn choice(
        key: &'static str,
        label: &'static str,
        choices: Vec<String>,
        fallback: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            choices,
            fallback,
            colour: false,
        }
    }

    fn typed(key: &'static str, label: &'static str, fallback: &'static str) -> Self {
        Self {
            key,
            label,
            choices: Vec::new(),
            fallback,
            colour: false,
        }
    }

    fn into_colour(mut self) -> Self {
        self.colour = true;
        self
    }
}

/// The window.
struct Settings {
    /// Where the settings file lives, if this program was lent a directory.
    directory: Option<Handle>,
    /// What the file says, as it was last read or written.
    values: nexus_config::Settings,
    /// Every line, headings included.
    items: Vec<Item>,
    /// Which line the cursor is on. Always a `Row`.
    cursor: usize,
    /// The value being typed, if one is.
    editing: Option<String>,
    /// What happened last, shown at the bottom.
    said: String,
    /// Whether the last thing that happened went wrong, which decides its
    /// colour. A message that went wrong and one that did not, in the same
    /// colour, is a message nobody reads twice.
    trouble: bool,
    /// What the machine looks like, for drawing this window in its own colours.
    look: nexus_look::Look,
    /// Whether the settings file could be read when this window opened.
    ///
    /// False turns every save into a refusal. A window that could not read the
    /// file has nothing to merge a change into, and saving anyway would replace
    /// a file it never saw.
    readable: bool,
}

impl Settings {
    fn new(directory: Option<Handle>) -> Self {
        // What the file says, and separately whether it could be read at all.
        // A window that could not read the settings must not offer to write
        // them: the write would be of what it managed to parse, which is
        // nothing.
        let read = match directory {
            Some(handle) => read_text(handle, SETTINGS_NAME),
            None => Ok(None),
        };
        let (text, unreadable) = match read {
            Ok(Some(text)) => (text, None),
            Ok(None) => (String::new(), None),
            Err(why) => (String::new(), Some(why)),
        };
        let values = nexus_config::Settings::parse(&text);
        let look = nexus_look::Look::parse(&text);

        let styles: Vec<String> = nexus_look::Style::ALL
            .iter()
            .map(|style| style.name().to_string())
            .collect();
        let languages: Vec<String> = nexus_i18n::LOCALES
            .iter()
            .map(|locale| locale.tag.to_string())
            .collect();
        let zones: Vec<String> = nexus_time::ZONES
            .iter()
            .map(|zone| zone.name.to_string())
            .collect();

        use nexus_config::key;
        let items = alloc::vec![
            Item::Heading("settings.look"),
            Item::Row(Row::choice(
                nexus_look::key::STYLE,
                "settings.style",
                styles,
                "gradient"
            )),
            Item::Row(Row::typed(nexus_look::key::TOP, "settings.top", "0b1428").into_colour()),
            Item::Row(
                Row::typed(nexus_look::key::BOTTOM, "settings.bottom", "040814").into_colour()
            ),
            Item::Row(
                Row::typed(nexus_look::key::ACCENT, "settings.accent", "388be8").into_colour()
            ),
            Item::Row(Row::choice(
                nexus_look::key::FONT,
                "settings.font",
                nexus_look::Font::ALL
                    .iter()
                    .map(|font| font.name().to_string())
                    .collect(),
                "crisp"
            )),
            Item::Row(Row::choice(
                nexus_look::key::SMOOTH,
                "settings.smooth",
                alloc::vec!["no".to_string(), "yes".to_string()],
                "no"
            )),
            Item::Heading("settings.machine"),
            Item::Row(Row::choice(
                key::LANGUAGE,
                "settings.language",
                languages,
                nexus_i18n::LOCALES[0].tag
            )),
            Item::Row(Row::choice(
                key::TIMEZONE,
                "settings.timezone",
                zones,
                nexus_time::ZONES[0].name
            )),
            Item::Row(Row::choice(
                key::UPDATES,
                "settings.updates",
                alloc::vec!["automatic".to_string(), "ask".to_string()],
                "automatic"
            )),
            Item::Heading("settings.network"),
            Item::Row(Row::choice(
                key::NETWORK,
                "settings.netmode",
                alloc::vec!["dhcp".to_string(), "off".to_string()],
                "dhcp"
            )),
            Item::Heading("settings.you"),
            Item::Row(Row::typed(key::USER_NAME, "settings.name", "")),
        ];

        let mut window = Self {
            directory,
            values,
            items,
            cursor: 0,
            editing: None,
            said: String::new(),
            trouble: false,
            look,
            readable: true,
        };
        window.cursor = window.next_row(0, 1).unwrap_or(0);
        if directory.is_none() {
            window.complain(nexus_i18n::text("settings.nofile"));
        } else if let Some(why) = unreadable {
            window.readable = false;
            window.complain(&why);
        }
        window
    }

    /// The row the cursor is on, if it is on one.
    fn row(&self) -> Option<&Row> {
        match self.items.get(self.cursor) {
            Some(Item::Row(row)) => Some(row),
            _ => None,
        }
    }

    /// The next row at or after `from`, walking by `step`. Headings are skipped
    /// because there is nothing to do to one, and a cursor that could sit on a
    /// heading would be a cursor that sometimes does nothing when pressed.
    fn next_row(&self, from: usize, step: isize) -> Option<usize> {
        let mut at = from as isize;
        while at >= 0 && (at as usize) < self.items.len() {
            if matches!(self.items[at as usize], Item::Row(_)) {
                return Some(at as usize);
            }
            at += step;
        }
        None
    }

    /// What a row says now: what is being typed, or the file's value, or the
    /// fallback.
    fn shown(&self, row: &Row) -> String {
        self.values.get(row.key).unwrap_or(row.fallback).to_string()
    }

    /// Put a message at the bottom.
    fn say(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = false;
    }

    fn complain(&mut self, text: &str) {
        self.said = text.to_string();
        self.trouble = true;
    }

    /// Write one setting into the file, and say what happened.
    ///
    /// The file is read again first rather than written from what this program
    /// remembers: something else may have changed a different line since this
    /// window opened, and a settings window that quietly reverted somebody
    /// else's change would be worse than one that could not write at all.
    fn save(&mut self, key: &'static str, value: &str) {
        let Some(directory) = self.directory else {
            self.complain(nexus_i18n::text("settings.nofile"));
            return;
        };

        if !self.readable {
            self.complain(nexus_i18n::text("settings.unreadable"));
            return;
        }
        // Read again before writing, and refuse if that read fails.
        //
        // Something else may have changed a different line since this window
        // opened -- the terminal writes the same file -- so the change is
        // merged into what is there now. And if what is there now cannot be
        // read, nothing is written: a settings window that answered an I/O
        // error by replacing the file with one line would destroy every setting
        // on the machine in order to change one of them.
        let mut latest = match read_text(directory, SETTINGS_NAME) {
            Ok(Some(text)) => nexus_config::Settings::parse(&text),
            Ok(None) => nexus_config::Settings::new(),
            Err(why) => {
                self.complain(&why);
                return;
            }
        };
        latest.set(key, value);
        match write_text(directory, SETTINGS_NAME, &latest.to_text()) {
            Ok(()) => {
                self.values = latest;
                self.look = nexus_look::Look::parse(&self.values.to_text());
                let said =
                    nexus_i18n::format("settings.saved", &[("key", &key), ("value", &value)]);
                self.say(&said);
                nexus_user::log(&format!("settings: {key} is now {value}")).ok();
            }
            Err(why) => self.complain(&why),
        }
    }
}

impl App for Settings {
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
        let mut column = nexus_ui::Column::new(area, 4);

        let title = column.row(nexus_ui::LINE_HEIGHT + 6);
        canvas.text(
            title.x,
            title.y + 3,
            nexus_i18n::text("settings.title"),
            ink,
        );

        for index in 0..self.items.len() {
            let line = column.line();
            if line.width == 0 {
                break;
            }
            match &self.items[index] {
                Item::Heading(label) => {
                    canvas.text(line.x, line.y, nexus_i18n::text(label), accent);
                }
                Item::Row(row) => {
                    let chosen = index == self.cursor;
                    if chosen {
                        // A band behind the row rather than a different ink, so
                        // that the row the keys act on is obvious from across a
                        // desk and not only from a foot away.
                        canvas.fill_rounded(
                            Rect::new(line.x, line.y, line.width, line.height),
                            4,
                            blend(bottom, accent, 64),
                        );
                    }
                    canvas.text(line.x + 8, line.y, nexus_i18n::text(row.label), ink);

                    let value = match (&self.editing, chosen) {
                        (Some(typed), true) => format!("{typed}\u{2588}"),
                        _ => self.shown(row),
                    };
                    // `text` returns how far it advanced, not where it
                    // stopped, so anything drawn after it has to add the two.
                    let value_at = line.x + VALUE_AT;
                    let after = value_at + canvas.text(value_at, line.y, &value, ink);

                    if row.colour {
                        // The colour itself, beside what it is written as. A
                        // hex value nobody can picture is a hex value people
                        // change by trying.
                        if let Some(swatch) = nexus_look::Colour::parse(&value) {
                            canvas.fill(
                                Rect::new(after + 8, line.y + 2, 28, line.height.saturating_sub(4)),
                                Colour(swatch.packed()),
                            );
                        }
                    } else if !row.choices.is_empty() && chosen {
                        canvas.text(value_at - 16, line.y, "<", quiet);
                        canvas.text(after + 8, line.y, ">", quiet);
                    }
                }
            }
        }

        // The message, and then the keys, at the bottom of whatever room is
        // left. Drawn last so that a long list pushes them off rather than
        // drawing over them -- everything here clips.
        let footer = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(nexus_ui::LINE_HEIGHT * 2 + 4),
            area.width,
            nexus_ui::LINE_HEIGHT * 2 + 4,
        );
        if !self.said.is_empty() {
            let colour = if self.trouble {
                Colour::rgb(0xE0, 0x80, 0x70)
            } else {
                accent
            };
            canvas.text(footer.x, footer.y, &self.said, colour);
        }
        canvas.text(
            footer.x,
            footer.y + nexus_ui::LINE_HEIGHT + 4,
            nexus_i18n::text("settings.keys"),
            quiet,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Move(Movement::Up) => {
                self.editing = None;
                match self
                    .cursor
                    .checked_sub(1)
                    .and_then(|at| self.next_row(at, -1))
                {
                    Some(at) => {
                        self.cursor = at;
                        true
                    }
                    None => false,
                }
            }
            Key::Move(Movement::Down) => {
                self.editing = None;
                match self.next_row(self.cursor + 1, 1) {
                    Some(at) => {
                        self.cursor = at;
                        true
                    }
                    None => false,
                }
            }
            Key::Move(Movement::Left) => self.step(-1),
            Key::Move(Movement::Right) => self.step(1),
            Key::Character(character) => {
                let Some(row) = self.row() else {
                    return false;
                };
                if !row.choices.is_empty() {
                    return false;
                }
                // Editing starts from what is there, so that changing one digit
                // of a colour does not mean typing all six.
                let current = self.shown(row);
                let typed = self.editing.get_or_insert(current);
                typed.push(character);
                true
            }
            Key::Backspace => match &mut self.editing {
                Some(typed) => {
                    typed.pop();
                    true
                }
                None => {
                    let Some(row) = self.row() else {
                        return false;
                    };
                    if !row.choices.is_empty() {
                        return false;
                    }
                    let mut current = self.shown(row);
                    current.pop();
                    self.editing = Some(current);
                    true
                }
            },
            Key::Enter => {
                let Some(typed) = self.editing.take() else {
                    return false;
                };
                let Some(row) = self.row() else {
                    return false;
                };
                let (key, colour) = (row.key, row.colour);
                if colour && nexus_look::Colour::parse(&typed).is_none() {
                    // Kept, not thrown away. Somebody who mistyped one digit of
                    // a colour wants to fix that digit, and a field that
                    // emptied itself on a refusal would make them type the
                    // other five again.
                    self.editing = Some(typed);
                    self.complain(nexus_i18n::text("settings.notacolour"));
                    return true;
                }
                self.save(key, &typed);
                true
            }
            Key::Escape => {
                if self.editing.take().is_some() {
                    return true;
                }
                false
            }
            // The interface language changed under this window, so every label
            // in it is now the wrong words.
            Key::Language => true,
            _ => false,
        }
    }
}

impl Settings {
    /// Move a choice row along by one, and write it.
    fn step(&mut self, by: isize) -> bool {
        let Some(row) = self.row() else {
            return false;
        };
        if row.choices.is_empty() {
            return false;
        }
        let current = self.shown(row);
        let at = row
            .choices
            .iter()
            .position(|choice| choice == &current)
            .unwrap_or(0);
        let count = row.choices.len() as isize;
        // Wrapping, because a list of four things with ends is a list where two
        // of the four take three presses to reach.
        let next = ((at as isize + by).rem_euclid(count)) as usize;
        let (key, value) = (row.key, row.choices[next].clone());
        self.save(key, &value);
        true
    }
}

// -- the file ----------------------------------------------------------------

/// A whole text file out of a directory.
///
/// Three answers and not two. `Ok(None)` is "there is no such file", which is
/// ordinary on a fresh machine; `Err` is "there is one and it could not be
/// read", which is not ordinary at all and must never be mistaken for the
/// first. Mistaking them is how a one-key change becomes the deletion of every
/// other setting: the caller reads nothing, believes the file was empty, and
/// writes back a file with one line in it.
///
/// A short read is an error for the same reason. Half a settings file parses
/// perfectly well and is missing half the settings.
fn read_text(directory: Handle, name: &str) -> Result<Option<String>, String> {
    let file = match nexus_user::open(directory, name) {
        Ok(file) => file,
        Err(nexus_user::Error::NotFound) => return Ok(None),
        Err(error) => return Err(format!("{name}: {error}")),
    };
    let outcome = read_open(file, name);
    nexus_user::close(file).ok();
    outcome.map(Some)
}

/// The body of [`read_text`], with the handle already open.
fn read_open(file: Handle, name: &str) -> Result<String, String> {
    let size = nexus_user::size(file).map_err(|error| format!("{name}: {error}"))?;
    if size > MAX_FILE {
        // Refused rather than truncated. A file this program cannot read whole
        // is a file it must not rewrite, because rewriting it means writing
        // back the part it did read and losing the rest.
        return Err(nexus_i18n::format("file.toolarge", &[("name", &name)]));
    }
    let mut bytes = alloc::vec![0u8; size];
    let read =
        nexus_user::read_at(file, 0, &mut bytes).map_err(|error| format!("{name}: {error}"))?;
    if read != size {
        return Err(nexus_i18n::format("file.short", &[("name", &name)]));
    }
    String::from_utf8(bytes).map_err(|_| nexus_i18n::format("file.nottext", &[("name", &name)]))
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
    nexus_user::log("settings: PANIC").ok();
    nexus_user::exit_with(2)
}
