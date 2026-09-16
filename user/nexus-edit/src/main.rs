//! `edit`: a window you can write a file in.
//!
//! The roadmap named this as the first program that would actually need damage
//! rectangles -- "a text editor redrawing one line" -- and that turned out to
//! be the right way round: the rectangles were built first, and this is what
//! proves they do anything. Typing a character repaints one line of a window,
//! not the window.
//!
//! # What it is lent
//!
//! One directory, read and write, and nothing else. It cannot see the rest of
//! the store, cannot reach the network, and cannot start a program. A file name
//! typed into it that tries to leave that directory is refused by the kernel,
//! which is where that decision belongs -- this program does not parse paths
//! and so cannot get path parsing wrong.
//!
//! # What it is
//!
//! A list of lines, a cursor, and a file name. Insert, delete, split a line
//! with Enter, join with Backspace at the start of one. Ctrl is not forwarded
//! by the compositor, so the commands are on function keys:
//!
//! | F2 | save |
//! | F3 | save as, which is the name field |
//! | F5 | reload from the disk, losing what is unsaved |
//! | Escape | close, refusing once if there is unsaved work |
//!
//! # What it is not
//!
//! No selection, no clipboard, no undo, no search, no syntax colouring, no word
//! wrap -- a line longer than the window is scrolled to, not folded. Each of
//! those is a real piece of work and none of them is pretended at.
//!
//! The file is held whole in memory and written whole. That is what the
//! filesystem underneath offers, and a program that appeared to edit a file
//! larger than its heap would be a program that lost the end of it.

#![no_std]
#![no_main]

extern crate alloc;

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

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_2000_0000;

/// How much heap: the file, the lines it becomes, and the strings they are.
const HEAP: usize = 4 * 1024 * 1024;

/// The largest file this will open.
///
/// Held whole, edited whole, written whole. A bound rather than a guess: what
/// is above it is a file this program would lose the end of, and refusing to
/// open it is the only honest answer.
const MAX_FILE: usize = 256 * 1024;

/// The most lines it will hold.
///
/// A file of one very long line and a file of a million short ones are both
/// files; this bounds the second. Beyond it the file is refused rather than
/// truncated, because a truncated file saved back is a file destroyed.
const MAX_LINES: usize = 20_000;

/// Space around things.
const PAD: u32 = 8;

/// The name of the file a new window starts on.
const UNTITLED: &str = "NOTES.TXT";

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
        main_error("edit: FAILED: could not get a heap");
        finish();
    }

    // One handle: the directory it may read and write. A window without it can
    // still be typed in and cannot save, and says so rather than appearing to.
    let mut lent = [Handle(0); 1];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            main_error(&format!("edit: FAILED: {trouble}"));
            finish();
        }
    };
    let directory = if carried >= 1 { Some(lent[0]) } else { None };
    if directory.is_none() {
        nexus_user::log("edit: started without a directory; nothing can be opened or saved").ok();
    }

    let mut editor = Editor::new(directory);
    editor.open(UNTITLED);
    nexus_user::log(&format!(
        "edit: a window for writing a file, on {}",
        editor.name
    ))
    .ok();

    let outcome = window.run(&mut editor);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        main_error(&format!("edit: FAILED: {}", outcome.ended));
    }
    finish()
}

/// What has changed since the last frame.
///
/// Three states and not an `Option`, because an `Option` had two of them
/// confused: `None` read as both "nothing yet" and "all of it", so a keystroke
/// after a scroll narrowed the repaint back to one line and left the rest of
/// the window stale. Naming the third state makes that unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Touched {
    /// Nothing, so nothing need be sent.
    Nothing,
    /// These rows, inclusive, and the status line.
    Rows(usize, usize),
    /// All of it: a scroll, a resize, a reload.
    Everything,
}

/// What the keyboard is going into.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Typing {
    /// The file.
    Text,
    /// The name field, for save-as.
    Name,
}

/// The window.
struct Editor {
    /// The directory it may read and write, if it was lent one.
    directory: Option<Handle>,
    /// The file's name.
    name: String,
    /// What is being typed into.
    typing: Typing,
    /// The file, one string per line. Always at least one, because a file with
    /// no lines has nowhere to put a cursor.
    lines: Vec<String>,
    /// Where the cursor is: a line, and a character within it.
    ///
    /// A character, not a byte. The lines are `String`s and the text may be
    /// Japanese, so a byte offset would land in the middle of a character and
    /// the first `insert` there would panic.
    row: usize,
    column: usize,
    /// The first line on screen.
    scroll: usize,
    /// And the first column, for a line wider than the window.
    across: usize,
    /// How many lines the window can show, from the last draw.
    visible: usize,

    /// Whether there are changes that are not on the disk.
    dirty: bool,
    /// Whether Escape has been pressed once with unsaved work.
    warned: bool,
    /// Whether the window is still open.
    open: bool,
    /// What to say along the bottom.
    status: String,

    /// What changed since the last frame.
    ///
    /// The whole point of the program as far as the compositor is concerned.
    touched: Touched,
    /// The window's size, from the last draw.
    width: u32,
    height: u32,
}

impl Editor {
    fn new(directory: Option<Handle>) -> Self {
        Self {
            directory,
            name: String::from(UNTITLED),
            typing: Typing::Text,
            lines: alloc::vec![String::new()],
            row: 0,
            column: 0,
            scroll: 0,
            across: 0,
            visible: 1,
            dirty: false,
            warned: false,
            open: true,
            status: String::new(),
            // Everything, because the first frame has to draw all of it.
            touched: Touched::Everything,
            width: 0,
            height: 0,
        }
    }

    /// Note that one row changed, so only that row is repainted.
    ///
    /// Widening only. A row noted after `touch_all` leaves it at
    /// [`Touched::Everything`] -- narrowing there is exactly the bug the enum
    /// exists to prevent.
    fn touch(&mut self, row: usize) {
        self.touched = match self.touched {
            Touched::Everything => Touched::Everything,
            Touched::Nothing => Touched::Rows(row, row),
            Touched::Rows(first, last) => Touched::Rows(first.min(row), last.max(row)),
        };
    }

    /// Note that all of it changed.
    fn touch_all(&mut self) {
        self.touched = Touched::Everything;
    }

    /// Read a file in, replacing what is here.
    fn open(&mut self, name: &str) {
        let Some(directory) = self.directory else {
            self.status = nexus_i18n::text("edit.nodisk").to_string();
            return;
        };
        // As typed, then uppercased: names on this store are uppercase because
        // the kernel seeds them from a FAT image, and somebody typing a name
        // should not have to know that. The same rule the wallpaper follows.
        let opened = nexus_user::open(directory, name)
            .or_else(|_| nexus_user::open(directory, &name.to_uppercase()));
        let Ok(file) = opened else {
            // Not an error: a name that is not there is a new file, which is
            // how every editor since the first one has behaved.
            self.name = name.to_string();
            self.lines = alloc::vec![String::new()];
            self.row = 0;
            self.column = 0;
            self.scroll = 0;
            self.dirty = false;
            self.status = nexus_i18n::format("edit.new", &[("name", &self.name)]);
            self.touch_all();
            return;
        };

        let size = nexus_user::size(file).unwrap_or(0);
        if size > MAX_FILE {
            nexus_user::close(file).ok();
            self.status = nexus_i18n::format(
                "edit.toobig",
                &[("name", &name.to_string()), ("size", &size)],
            );
            self.touch_all();
            return;
        }
        let mut bytes = alloc::vec![0u8; size];
        let read = nexus_user::read(file, &mut bytes).unwrap_or(0);
        bytes.truncate(read);
        nexus_user::close(file).ok();

        // Not UTF-8 is not an error either -- it is a file this program cannot
        // show, and saying so beats showing replacement characters and then
        // writing them back over somebody's data.
        let Ok(text) = core::str::from_utf8(&bytes) else {
            self.status = nexus_i18n::format("edit.notext", &[("name", &name.to_string())]);
            self.touch_all();
            return;
        };

        let lines: Vec<String> = text
            .split('\n')
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect();
        if lines.len() > MAX_LINES {
            self.status = nexus_i18n::format(
                "edit.toomany",
                &[("name", &name.to_string()), ("lines", &lines.len())],
            );
            self.touch_all();
            return;
        }

        self.name = name.to_string();
        self.lines = if lines.is_empty() {
            alloc::vec![String::new()]
        } else {
            lines
        };
        self.row = 0;
        self.column = 0;
        self.scroll = 0;
        self.across = 0;
        self.dirty = false;
        self.warned = false;
        self.status = nexus_i18n::format(
            "edit.opened",
            &[("name", &self.name), ("lines", &self.lines.len())],
        );
        // Logged as well as shown, because what a later boot can check is the
        // log: "the file came back with the lines that went into it" is the
        // claim a test can hold this program to.
        nexus_user::log(&format!(
            "edit: {} read, {} lines",
            self.name,
            self.lines.len()
        ))
        .ok();
        self.touch_all();
    }

    /// Write it back.
    fn save(&mut self) -> bool {
        let Some(directory) = self.directory else {
            self.status = nexus_i18n::text("edit.nodisk").to_string();
            self.touch_all();
            return true;
        };
        if self.name.is_empty() {
            self.status = nexus_i18n::text("edit.noname").to_string();
            self.touch_all();
            return true;
        }

        // Joined with a newline and no trailing one added: what was read is
        // what is written, so a file that round-trips through this program
        // unchanged is byte-for-byte unchanged.
        let text = self.lines.join("\n");

        let existing = nexus_user::open(directory, &self.name)
            .or_else(|_| nexus_user::open(directory, &self.name.to_uppercase()));
        let file = match existing {
            Ok(file) => file,
            Err(_) => match nexus_user::create(directory, &self.name, nexus_user::Kind::File) {
                Ok(file) => file,
                Err(error) => {
                    self.status =
                        nexus_i18n::format("edit.cannotsave", &[("why", &format!("{error}"))]);
                    self.touch_all();
                    return true;
                }
            },
        };

        let written = nexus_user::write(file, text.as_bytes());
        nexus_user::close(file).ok();
        match written {
            Ok(count) if count == text.len() => {
                self.dirty = false;
                self.warned = false;
                self.status =
                    nexus_i18n::format("edit.saved", &[("name", &self.name), ("bytes", &count)]);
                nexus_user::log(&format!("edit: wrote {}, {count} bytes", self.name)).ok();
            }
            // A short write is the one failure that must not look like success:
            // the file on the disk is now neither what was there nor what is on
            // screen.
            Ok(count) => {
                self.status = nexus_i18n::format(
                    "edit.shortwrite",
                    &[("wrote", &count), ("wanted", &text.len())],
                );
                nexus_user::log(&format!(
                    "edit: FAILED: wrote {count} of {} bytes to {}",
                    text.len(),
                    self.name
                ))
                .ok();
            }
            Err(error) => {
                self.status =
                    nexus_i18n::format("edit.cannotsave", &[("why", &format!("{error}"))]);
            }
        }
        self.touch_all();
        true
    }

    /// The line the cursor is on, as characters.
    fn characters(&self, row: usize) -> Vec<char> {
        self.lines
            .get(row)
            .map(|line| line.chars().collect())
            .unwrap_or_default()
    }

    /// Put a character in.
    fn insert(&mut self, letter: char) -> bool {
        if self.lines.len() > MAX_LINES {
            return false;
        }
        let mut characters = self.characters(self.row);
        let at = self.column.min(characters.len());
        characters.insert(at, letter);
        self.lines[self.row] = characters.into_iter().collect();
        self.column = at + 1;
        self.dirty = true;
        self.warned = false;
        self.touch(self.row);
        true
    }

    /// Take one out, or join two lines.
    fn backspace(&mut self) -> bool {
        if self.column > 0 {
            let mut characters = self.characters(self.row);
            let at = self.column - 1;
            if at >= characters.len() {
                return false;
            }
            characters.remove(at);
            self.lines[self.row] = characters.into_iter().collect();
            self.column = at;
            self.dirty = true;
            self.warned = false;
            self.touch(self.row);
            return true;
        }
        if self.row == 0 {
            return false;
        }
        // Joining two lines moves everything below up, so everything below is
        // what changed.
        let line = self.lines.remove(self.row);
        self.row -= 1;
        self.column = self.lines[self.row].chars().count();
        self.lines[self.row].push_str(&line);
        self.dirty = true;
        self.warned = false;
        self.touch_all();
        true
    }

    /// Split the line at the cursor.
    fn enter(&mut self) -> bool {
        if self.lines.len() >= MAX_LINES {
            self.status = nexus_i18n::format(
                "edit.toomany",
                &[("name", &self.name), ("lines", &self.lines.len())],
            );
            self.touch_all();
            return true;
        }
        let characters = self.characters(self.row);
        let at = self.column.min(characters.len());
        let left: String = characters[..at].iter().collect();
        let right: String = characters[at..].iter().collect();
        self.lines[self.row] = left;
        self.lines.insert(self.row + 1, right);
        self.row += 1;
        self.column = 0;
        self.dirty = true;
        self.warned = false;
        self.touch_all();
        true
    }

    /// Move the cursor, and say whether anything changed.
    fn move_to(&mut self, movement: Movement) -> bool {
        let before = (self.row, self.column, self.scroll, self.across);
        let last = self.lines.len().saturating_sub(1);
        match movement {
            Movement::Up => {
                self.row = self.row.saturating_sub(1);
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            Movement::Down => {
                self.row = (self.row + 1).min(last);
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            Movement::Left => {
                if self.column > 0 {
                    self.column -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.column = self.lines[self.row].chars().count();
                }
            }
            Movement::Right => {
                let width = self.lines[self.row].chars().count();
                if self.column < width {
                    self.column += 1;
                } else if self.row < last {
                    self.row += 1;
                    self.column = 0;
                }
            }
            Movement::PageUp => {
                self.row = self.row.saturating_sub(self.visible.max(1));
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            Movement::PageDown => {
                self.row = (self.row + self.visible.max(1)).min(last);
                self.column = self.column.min(self.lines[self.row].chars().count());
            }
            Movement::Home => self.column = 0,
            Movement::End => self.column = self.lines[self.row].chars().count(),
        }
        self.follow();
        let after = (self.row, self.column, self.scroll, self.across);
        if before == after {
            return false;
        }
        // A scroll moves every line; a cursor moving inside the window changes
        // the row it left and the row it arrived at, and nothing else.
        if before.2 != after.2 || before.3 != after.3 {
            self.touch_all();
        } else {
            self.touch(before.0);
            self.touch(after.0);
        }
        true
    }

    /// Bring the cursor back on screen.
    fn follow(&mut self) {
        if self.row < self.scroll {
            self.scroll = self.row;
        }
        let visible = self.visible.max(1);
        if self.row >= self.scroll + visible {
            self.scroll = self.row + 1 - visible;
        }
        // Sideways, for a line wider than the window. In characters, which is
        // right for the fixed-width face this draws with.
        let columns = self.columns().max(1);
        if self.column < self.across {
            self.across = self.column;
        }
        if self.column >= self.across + columns {
            self.across = self.column + 1 - columns;
        }
    }

    /// Roughly how many characters fit across the window.
    ///
    /// Roughly, because the font is not fixed width. It is used only to decide
    /// when to scroll sideways, where being a few characters out means the
    /// cursor sits a little further from the edge than it might.
    fn columns(&self) -> usize {
        let usable = self.width.saturating_sub(PAD * 2 + gutter());
        (usable / nexus_ui::font::HALF_WIDTH.max(1)) as usize
    }
}

/// How wide the line-number gutter is.
///
/// Five digits and a space. `MAX_LINES` is five digits, and the numbers are
/// Latin, so the half-width advance is the right one here even though the text
/// beside it may not be.
const fn gutter() -> u32 {
    6 * nexus_ui::font::HALF_WIDTH
}

impl App for Editor {
    fn draw(&mut self, canvas: &mut Canvas) {
        let bounds = canvas.bounds();
        if bounds.width != self.width || bounds.height != self.height {
            self.width = bounds.width;
            self.height = bounds.height;
            self.touch_all();
        }

        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x70, 0x80, 0x98);
        let paper = Colour::rgb(0x0B, 0x11, 0x1D);
        let bar = Colour::rgb(0x14, 0x1C, 0x2C);
        let accent = Colour::rgb(0x62, 0x80, 0xD5);

        let line_height = nexus_ui::LINE_HEIGHT;
        // Below the strip the compositor paints its title bar over, or the
        // first line of the file would be half a title bar.
        let text_top = bounds.y + nexus_ui::TITLE_BAR + PAD / 2;
        let status_height = line_height + PAD;
        let text_height = bounds
            .height
            .saturating_sub(nexus_ui::TITLE_BAR + PAD / 2 + status_height);
        self.visible = (text_height / line_height.max(1)) as usize;
        self.follow();

        // Which rows to draw. The whole point: a keystroke repaints one line.
        let (first, last) = match self.touched {
            Touched::Everything | Touched::Nothing => (self.scroll, self.scroll + self.visible),
            Touched::Rows(first, last) => (
                first.max(self.scroll),
                (last + 1).min(self.scroll + self.visible),
            ),
        };

        if self.touched == Touched::Everything {
            canvas.fill(bounds, paper);
        }

        for row in first..last.min(self.lines.len().max(1)) {
            if row < self.scroll || row >= self.scroll + self.visible {
                continue;
            }
            let y = text_top + ((row - self.scroll) as u32) * line_height;
            if y + line_height > bounds.y + bounds.height - status_height {
                break;
            }
            // The row's own strip, cleared before it is drawn. Without this a
            // shorter line would leave the tail of the longer one it replaced.
            let strip = Rect::new(bounds.x, y, bounds.width, line_height);
            canvas.fill(strip, paper);

            // The number, quietly, so a line can be referred to.
            let number = format!("{:>5}", row + 1);
            canvas.text(bounds.x + PAD, y, &number, quiet);

            let characters: Vec<char> = self.characters(row);
            let from = self.across.min(characters.len());
            let shown: String = characters[from..].iter().collect();
            canvas.text(bounds.x + PAD + gutter(), y, &shown, ink);

            // The cursor. Placed by measuring the text to its left rather
            // than by multiplying a character count: this font is not fixed
            // width -- Japanese is sixteen pixels and Latin is eight -- so
            // arithmetic would put the caret in the wrong place on any line
            // with kana in it.
            if row == self.row && self.typing == Typing::Text {
                let before: String = characters[from..self.column.max(from)].iter().collect();
                canvas.fill(
                    Rect::new(
                        bounds.x + PAD + gutter() + nexus_ui::measure(&before),
                        y,
                        2,
                        line_height,
                    ),
                    accent,
                );
            }
        }

        // The status line, always: it carries the name, whether there is
        // unsaved work, and where the cursor is, all of which change with the
        // things above.
        let status_y = bounds.y + bounds.height - status_height;
        canvas.fill(
            Rect::new(bounds.x, status_y, bounds.width, status_height),
            bar,
        );
        let mark = if self.dirty { "*" } else { " " };
        let left = if self.typing == Typing::Name {
            nexus_i18n::format("edit.saveas", &[("name", &self.name)])
        } else {
            format!("{mark}{}  {}:{}", self.name, self.row + 1, self.column + 1)
        };
        canvas.text(bounds.x + PAD, status_y + PAD / 2, &left, ink);

        let right = if self.status.is_empty() {
            nexus_i18n::text("edit.keys").to_string()
        } else {
            self.status.clone()
        };
        let width = nexus_ui::measure(&right);
        if width + PAD * 2 < bounds.width {
            canvas.text(
                bounds.x + bounds.width - PAD - width,
                status_y + PAD / 2,
                &right,
                quiet,
            );
        }
    }

    fn damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        let line_height = nexus_ui::LINE_HEIGHT;
        let status_height = line_height + PAD;
        // Taken, so the next frame starts from nothing and a row that was
        // repainted is not repainted again for ever.
        let taken = core::mem::replace(&mut self.touched, Touched::Nothing);
        let (first, last) = match taken {
            // `None` to the compositor is "the whole surface".
            Touched::Everything => return None,
            // Nothing changed but the frame was drawn anyway; the status line
            // carries the cursor position, so that much is always new.
            Touched::Nothing => {
                return Some((
                    0,
                    self.height.saturating_sub(status_height),
                    self.width,
                    status_height,
                ))
            }
            Touched::Rows(first, last) => (first, last),
        };

        // The rows that changed, plus the status line, which changes whenever
        // anything else does -- the cursor position is on it.
        if first >= self.scroll + self.visible || last < self.scroll {
            // Nothing that changed is on screen; only the status line is.
            return Some((
                0,
                self.height.saturating_sub(status_height),
                self.width,
                status_height,
            ));
        }
        let top_row = first.max(self.scroll) - self.scroll;
        let y = nexus_ui::TITLE_BAR + PAD / 2 + top_row as u32 * line_height;
        // To the bottom, because the status line is down there and it changed
        // too. A second rectangle would be more precise and the compositor
        // takes one; two would be a list, and a list is the next piece of work
        // rather than this one.
        Some((
            0,
            y.min(self.height),
            self.width,
            self.height.saturating_sub(y),
        ))
    }

    fn key(&mut self, key: Key) -> bool {
        // The name field takes the keyboard when save-as is open.
        if self.typing == Typing::Name {
            return match key {
                Key::Character(letter) => {
                    self.name.push(letter);
                    self.touch_all();
                    true
                }
                Key::Backspace => {
                    self.name.pop();
                    self.touch_all();
                    true
                }
                Key::Enter => {
                    self.typing = Typing::Text;
                    self.save()
                }
                Key::Escape => {
                    self.typing = Typing::Text;
                    self.status = nexus_i18n::text("edit.cancelled").to_string();
                    self.touch_all();
                    true
                }
                _ => false,
            };
        }

        match key {
            Key::Character(letter) => self.insert(letter),
            Key::Backspace => self.backspace(),
            Key::Enter => self.enter(),
            Key::Tab => {
                // Four spaces, not a tab character. A tab would have to be
                // rendered with a width this program would then have to agree
                // about with every other program that opened the file.
                let mut changed = false;
                for _ in 0..4 {
                    changed |= self.insert(' ');
                }
                changed
            }
            Key::Move(movement) => self.move_to(movement),
            Key::Function(2) => self.save(),
            Key::Function(3) => {
                self.typing = Typing::Name;
                self.status = nexus_i18n::text("edit.namethefile").to_string();
                self.touch_all();
                true
            }
            Key::Function(5) => {
                let name = self.name.clone();
                self.open(&name);
                true
            }
            Key::Escape => {
                // Once, if there is unsaved work. A window that closed on the
                // first Escape would be one keystroke between somebody and
                // losing what they typed.
                if self.dirty && !self.warned {
                    self.warned = true;
                    self.status = nexus_i18n::text("edit.unsaved").to_string();
                    self.touch_all();
                    return true;
                }
                self.open = false;
                true
            }
            // Every string on screen is a different string now.
            Key::Language => {
                self.touch_all();
                true
            }
            _ => false,
        }
    }

    fn resized(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.touch_all();
    }

    fn running(&self) -> bool {
        self.open
    }
}

/// Stop, saying whether anything went wrong.
fn finish() -> ! {
    nexus_user::exit()
}

#[panic_handler]
fn panicked(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("edit: PANIC: {info}")).ok();
    nexus_user::exit_with(2)
}
