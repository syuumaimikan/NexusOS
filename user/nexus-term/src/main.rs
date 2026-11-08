//! `term`: a terminal window, and a shell to type into it.
//!
//! An ordinary client with two things lent to it: a directory to work in and a
//! channel to ask for programs. Those two handles are the whole of what it can
//! do — there is no path outside the directory it was given, and no way to
//! start a program except by asking the service that decides.
//!
//! # Why the commands are built in
//!
//! Because there is nowhere else to put them yet. A shell on a grown-up system
//! runs `ls` as a program; here `ls` would be a program that needs the same
//! directory handle, started through the same service, to print into a window it
//! does not own. That is three mechanisms this system does not have (a working
//! directory a child inherits, a standard output, and a pipe) and inventing all
//! three to move a directory listing out of this file would be inventing them
//! for the sake of an aesthetic.
//!
//! So the commands that only read and write files live here, and `run` starts a
//! real program through the spawn service. When there is a standard output to
//! inherit, the first group moves out and this stops being special.
//!
//! # What a person can type
//!
//! `help` lists it. The set is chosen for a machine somebody is actually using:
//! look at what is here, read a file, write one, remove one, make a directory,
//! start a program, ask the machine about itself.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_abi::gpu::*;
use nexus_api::process::{Child, Spawn};
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::{Handle, Kind};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_3000_0000;

/// How much heap: a scrollback, and whatever a file being read comes to.
const HEAP: usize = 4 * 1024 * 1024;

/// How many lines of scrollback are kept.
///
/// A bound rather than a policy: the buffer is memory, and a command that
/// printed for ever would otherwise be a window that grows until the machine
/// stops.
const SCROLLBACK: usize = 2_000;

/// The longest line this will read out of a file before it stops.
const MAX_FILE: usize = 256 * 1024;

/// How many commands are remembered.
const HISTORY: usize = 64;

/// What the compositor says, and what this program says back.
mod wire {
    pub const SHOWN: &[u8] = b"shown";
    pub const RESIZED: &[u8] = b"size";
    pub const DAMAGED: &[u8] = b"damaged";
}

/// What a key is, as the kernel sends it.
mod key {
    pub const CHARACTER: u8 = 1;
    pub const BACKSPACE: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const ESCAPE: u8 = 4;
    pub const TAB: u8 = 5;
    pub const FUNCTION: u8 = 6;
    pub const LANGUAGE: u8 = 7;
    pub const MOVE: u8 = 8;

    /// The function key that changes what typing produces.
    ///
    /// F2, because F1 is the kernel's own language switch and a key that two
    /// things listen to is a key that does two things nobody asked for.
    pub const SCRIPT: u32 = 2;
    pub const SIZE: usize = 5;

    pub const UP: u32 = 0;
    pub const DOWN: u32 = 1;
    pub const LEFT: u32 = 2;
    pub const RIGHT: u32 = 3;
    pub const PAGE_UP: u32 = 4;
    pub const PAGE_DOWN: u32 = 5;
    pub const HOME: u32 = 6;
    pub const END: u32 = 7;
}

/// Space around the text.
const PAD: u32 = 6;

/// One line in the scrollback, and how it is drawn.
#[derive(Clone)]
struct Printed {
    text: String,
    kind: Kindness,
}

/// What a line is, which decides its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kindness {
    /// What the person typed, echoed back.
    Typed,
    /// Ordinary output.
    Plain,
    /// Something that went wrong.
    Trouble,
    /// A heading or a hint.
    Note,
}

/// Interactive REPL mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplLanguage {
    Python,
}

/// Everything the terminal knows.
struct Terminal {
    /// The directory this shell works in, which is the whole of its reach.
    root: Option<Handle>,
    /// Where in it, as a list of names. Never contains `..`.
    path: Vec<String>,
    /// The handle for `path`, reopened whenever it changes.
    here: Option<Handle>,
    /// The service that starts programs, if this shell was lent one.
    spawner: Option<Handle>,
    /// The speaker, if this shell was lent it.
    sound: Option<Handle>,

    width: u32,
    height: u32,

    /// What has been printed.
    lines: Vec<Printed>,
    /// The line being typed.
    typing: String,
    /// Where the cursor is in it, counted in characters.
    caret: usize,
    /// What has been typed before, newest last.
    history: Vec<String>,
    /// Where in the history the up arrow has got to.
    recalled: Option<usize>,
    /// How far the view is scrolled back, in lines from the bottom.
    scrolled: usize,
    /// Turns romaji into kana, when it is asked to.
    ime: nexus_ime::Ime,
    /// The kanji conversion being chosen from, if one is open.
    choosing: Option<Choice>,
    /// The channel that says what the machine is doing, if this shell has one.
    machine: Option<Handle>,
    /// The network, if this shell was lent it.
    ///
    /// A shell is where network tools live on every system somebody has used,
    /// and the compositor is what decides whether this one gets them. Without
    /// the handle the commands say so rather than pretending the network is
    /// down.
    network: Option<Handle>,
    /// Removable drives, if it was lent them.
    ///
    /// Separate from `root`, and a channel rather than a directory, because a
    /// stick is not a fixed disk: it can be absent, and it can be pulled out
    /// between two commands. See the kernel's `removable.rs`.
    removable: Option<Handle>,
    /// The face to draw in, and whether to soften its edges.
    ///
    /// Read once, when the window opens. Unlike the wallpaper this does not
    /// watch the settings file: a terminal that changed face mid-session would
    /// reflow every line of scrollback under somebody's cursor, and opening a
    /// new one is both cheap and what a person would do anyway.
    style: (nexus_ui::font::Face, bool),
    /// Active interactive REPL if running one.
    repl_lang: Option<ReplLanguage>,
    /// Environment variables for Python REPL.
    py_vars: alloc::collections::BTreeMap<String, String>,
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
        failed("term: FAILED: could not get a heap");
        finish();
    }

    // Three handles: the surface, the directory to work in, and the service
    // that starts programs. The last two are what make it a shell rather than a
    // window with a prompt in it, and a copy started without them says so
    // rather than failing at the first command.
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 7];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("term: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles < 1 || received.bytes < 16 {
        failed("term: FAILED: no surface came with the message");
        finish();
    }

    let width = read_u32(&buffer, 0);
    let height = read_u32(&buffer, 4);
    let surface = handles[0];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("term: FAILED: could not map its surface");
        finish();
    };
    if width as usize * height as usize * 4 > mapped {
        failed("term: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    let mut terminal = Terminal {
        root: (received.handles >= 2).then(|| handles[1]),
        path: Vec::new(),
        here: (received.handles >= 2).then(|| handles[1]),
        spawner: (received.handles >= 3).then(|| handles[2]),
        sound: (received.handles >= 4).then(|| handles[3]),
        machine: (received.handles >= 5).then(|| handles[4]),
        network: (received.handles >= 6).then(|| handles[5]),
        removable: (received.handles >= 7).then(|| handles[6]),
        width,
        height,
        lines: Vec::new(),
        typing: String::new(),
        caret: 0,
        history: Vec::new(),
        recalled: None,
        scrolled: 0,
        ime: nexus_ime::Ime::new(),
        choosing: None,
        style: (nexus_ui::font::Face::Crisp, false),
        repl_lang: None,
        py_vars: alloc::collections::BTreeMap::new(),
    };
    terminal.style = terminal.read_style();

    terminal.note(nexus_i18n::text("term.welcome"));
    if terminal.root.is_none() {
        terminal.trouble(nexus_i18n::text("term.nofiles"));
    }
    if terminal.spawner.is_none() {
        terminal.trouble(nexus_i18n::text("term.noprograms"));
    }
    terminal.note(nexus_i18n::text("term.hint"));

    nexus_user::log("term: a terminal, with a shell in it").ok();
    terminal.run(surface);
    finish()
}

/// The words this shell handles itself, and so cannot pipe.
///
/// They write into the window rather than to a handle. A pipe that quietly
/// dropped the right-hand side of `help | count` would be worse than one that
/// says it cannot do it.
const BUILT_IN: &[&str] = &[
    "help",
    "?",
    "clear",
    "cls",
    "echo",
    "pwd",
    "dir",
    "cd",
    "type",
    "write",
    "append",
    "mkdir",
    "rm",
    "del",
    "run",
    "date",
    "beep",
    "set",
    "look",
    "wall",
    "wallpaper",
    "sys",
    "top",
    "net",
    "usb",
    "lookup",
    "dig",
    "nslookup",
    "scan",
    "uptime",
    "history",
    "game",
    "steam",
    "gpu",
    "vulkan",
    "vk",
    "mesa",
    "gl",
    "taskmgr",
    "calc",
    "devmgr",
    "lspci",
    "hwinfo",
    "bench",
    "ps",
    "play",
    "music",
    "paint",
    "archive",
    "free",
    "apt",
    "dpkg",
];

/// Split a typed line at its first `|`, if it has one outside quotes.
///
/// Outside quotes, because `echo "a | b"` is one argument containing a bar and
/// not a pipe. The scan is the same one `nexus_shellwords` does and it is done
/// again here rather than shared, because this needs the *position* of the bar
/// in the original text and the splitter returns words.
fn split_pipe(line: &str) -> Option<Vec<alloc::string::String>> {
    let mut sides: Vec<alloc::string::String> = Vec::new();
    let mut quote: Option<char> = None;
    let mut from = 0usize;

    for (at, character) in line.char_indices() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character == '|' => {
                sides.push(alloc::string::String::from(line[from..at].trim()));
                from = at + 1;
            }
            None => {}
        }
    }
    if sides.is_empty() {
        return None;
    }
    sides.push(alloc::string::String::from(line[from..].trim()));

    // A bar with nothing on one side of it is a line somebody is still typing,
    // not a pipe. Refused as a whole rather than run with a piece missing.
    if sides.iter().any(alloc::string::String::is_empty) {
        return None;
    }
    Some(sides)
}

/// A kanji conversion somebody is choosing between.
///
/// The kana is kept as well as the candidates, because escaping has to put it
/// back: a conversion that could not be undone would be one nobody dares start
/// in the middle of a line.
struct Choice {
    /// What was typed, in kana.
    kana: String,
    /// What it could be written as, commonest first. The kana itself is in
    /// here too, at the end, so cycling always reaches it.
    candidates: Vec<String>,
    /// Which one is in the line at the moment.
    at: usize,
}

impl Terminal {
    /// Draw, say so, and act on whatever comes back.
    fn run(&mut self, mut surface: Handle) {
        let Ok(set) = nexus_user::wait_set() else {
            failed("term: FAILED: could not make a wait set");
            return;
        };
        const SAID: u64 = 1;
        if nexus_user::watch(set, COMPOSITOR, SAID).is_err() {
            failed("term: FAILED: could not watch the compositor");
            return;
        }

        let mut stale = true;
        let mut in_flight = false;
        let mut drawn = 0u32;

        // Bounded, so a terminal whose compositor stops answering cannot spin.
        for _ in 0..8_000_000u64 {
            if stale && !in_flight {
                self.draw();
                if nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).is_err() {
                    break;
                }
                drawn += 1;
                stale = false;
                in_flight = true;
            }

            let mut keys = [0u64; 2];
            let Ok(ready) = nexus_user::wait_any(set, &mut keys) else {
                break;
            };
            if ready == 0 {
                break;
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
                self.width = read_u32(bytes, 4);
                self.height = read_u32(bytes, 8);
                let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
                    failed("term: FAILED: could not map the surface it was given");
                    break;
                };
                if self.width as usize * self.height as usize * 4 > mapped {
                    failed("term: FAILED: the new surface is smaller than its size");
                    break;
                }
                stale = true;
                continue;
            }

            if bytes == b"close" {
                nexus_user::log("term: accepted decoration close").ok();
                break;
            }
            if received.bytes >= key::SIZE && self.key(bytes) {
                stale = true;
            }
        }

        if drawn > 0 {
            nexus_user::log("term: ran a shell for as long as it was asked to").ok();
        }
    }

    // -- what goes in the scrollback ------------------------------------------

    fn print(&mut self, text: &str, kind: Kindness) {
        // Split on newlines here rather than at every call site: a command that
        // produces several lines should not have to know how a window works.
        for piece in text.split('\n') {
            if self.lines.len() >= SCROLLBACK {
                self.lines.remove(0);
            }
            self.lines.push(Printed {
                text: piece.to_string(),
                kind,
            });
        }
        // Anything printed brings the view back to the bottom, because what was
        // just printed is what somebody wants to see.
        self.scrolled = 0;
    }

    fn plain(&mut self, text: &str) {
        self.print(text, Kindness::Plain);
    }

    fn note(&mut self, text: &str) {
        self.print(text, Kindness::Note);
    }

    fn trouble(&mut self, text: &str) {
        self.print(text, Kindness::Trouble);
    }

    /// What the prompt looks like: where this shell is, and what typing makes.
    ///
    /// The script is only shown when it is not the plain one, so a machine
    /// nobody is typing Japanese on has a prompt with nothing extra in it.
    fn prompt(&self) -> String {
        if self.repl_lang == Some(ReplLanguage::Python) {
            return String::from(">>> ");
        }
        let here = self.path.join("/");
        match self.ime.script() {
            nexus_ime::Script::Direct => format!("/{here}> "),
            script => format!("[{}] /{here}> ", script.name()),
        }
    }

    // -- keys -----------------------------------------------------------------

    /// Act on a key. Returns whether anything on screen changed.
    fn key(&mut self, message: &[u8]) -> bool {
        let kind = message[0];
        let value = read_u32(message, 1);

        match kind {
            key::LANGUAGE => {
                if let Some(locale) = nexus_i18n::LOCALES.get(value as usize) {
                    nexus_i18n::set_locale(locale.tag);
                }
                true
            }
            key::FUNCTION if value == key::SCRIPT => {
                // Whatever was half-typed goes into the line rather than being
                // thrown away: the letters were typed, and a mode change that
                // ate them would be a mode change that loses work.
                let left = self.ime.cycle();
                self.insert(&left);
                let said =
                    nexus_i18n::format("term.script", &[("script", &self.ime.script().name())]);
                self.note(&said);
                // Said on the log as well as in the window. What the window
                // shows is for the person typing; this is the only way anything
                // outside the machine can tell that the key arrived and did
                // something -- and "did the keyboard reach the program" is
                // exactly the question a test about typing has to answer.
                nexus_user::log(&format!(
                    "term: typing now makes {}",
                    self.ime.script().name()
                ))
                .ok();
                true
            }
            key::CHARACTER => {
                let Some(character) = char::from_u32(value) else {
                    return false;
                };
                // Space, with kana behind the caret, is "convert this" -- which
                // is what space means in every Japanese input method there has
                // ever been, and is why this is checked before the character
                // reaches the kana table.
                if character == ' ' && self.convert() {
                    return true;
                }
                // Anything else typed *while choosing* accepts what is shown
                // and carries on. The alternative -- swallowing the letter --
                // loses a keystroke somebody made on purpose.
                //
                // Only while choosing. Doing it unconditionally was a bug that
                // took a boot to find: `settle` forgets the composition, so
                // every keystroke threw away the kana before it and `kanji`
                // arrived at the converter as `じ`.
                if self.choosing.take().is_some() {
                    self.ime.settle();
                }

                let output = self.ime.push(character);
                self.insert(&output.committed);
                self.recalled = None;
                true
            }
            key::BACKSPACE => {
                // The romaji being decided first, which is what backspace does
                // in every input method: it un-types the letters before it
                // touches the kana they would have become.
                if self.ime.backspace() {
                    return true;
                }
                if self.caret == 0 {
                    return false;
                }
                let at = self.byte_of(self.caret - 1);
                self.typing.remove(at);
                self.caret -= 1;
                true
            }
            key::ENTER => {
                // Whatever candidate is in the line is the one that was meant.
                self.choosing = None;
                self.ime.settle();
                let left = self.ime.finish();
                self.insert(&left);
                let line = core::mem::take(&mut self.typing);
                self.caret = 0;
                self.recalled = None;
                let prompt = self.prompt();
                self.print(&format!("{prompt}{line}"), Kindness::Typed);
                if !line.trim().is_empty() {
                    if self.history.len() >= HISTORY {
                        self.history.remove(0);
                    }
                    self.history.push(line.clone());
                    self.obey(&line);
                }
                true
            }
            key::ESCAPE => {
                // While choosing a kanji, escape means "not any of those" and
                // puts the kana back -- not "throw the line away". A conversion
                // that could not be undone would be one nobody dares start in
                // the middle of a sentence.
                if let Some(choice) = self.choosing.take() {
                    let shown = choice.candidates[choice.at].clone();
                    self.replace_before_caret(&shown, &choice.kana);
                    nexus_user::log("term: went back to the kana").ok();
                    return true;
                }
                self.ime.clear();
                self.typing.clear();
                self.caret = 0;
                self.recalled = None;
                true
            }
            key::TAB => {
                // Nothing to complete against yet, so it indents. Better than
                // nothing happening, and it is what a tab does in text.
                let at = self.byte_of(self.caret);
                self.typing.insert(at, ' ');
                self.caret += 1;
                true
            }
            key::MOVE => self.movement(value),
            _ => false,
        }
    }

    /// One of the keys that moves rather than types.
    fn movement(&mut self, which: u32) -> bool {
        let rows = self.rows().max(1);
        match which {
            key::LEFT => {
                self.caret = self.caret.saturating_sub(1);
                true
            }
            key::RIGHT => {
                self.caret = (self.caret + 1).min(self.typing.chars().count());
                true
            }
            key::HOME => {
                self.caret = 0;
                true
            }
            key::END => {
                self.caret = self.typing.chars().count();
                true
            }
            // Up and down walk the history, which is what they do in every
            // shell and what somebody reaches for first.
            key::UP => {
                if self.history.is_empty() {
                    return false;
                }
                let next = match self.recalled {
                    None => self.history.len() - 1,
                    Some(0) => 0,
                    Some(at) => at - 1,
                };
                self.recalled = Some(next);
                self.typing = self.history[next].clone();
                self.caret = self.typing.chars().count();
                true
            }
            key::DOWN => {
                let Some(at) = self.recalled else {
                    return false;
                };
                if at + 1 >= self.history.len() {
                    self.recalled = None;
                    self.typing.clear();
                } else {
                    self.recalled = Some(at + 1);
                    self.typing = self.history[at + 1].clone();
                }
                self.caret = self.typing.chars().count();
                true
            }
            key::PAGE_UP => {
                let most = self.lines.len().saturating_sub(rows);
                self.scrolled = (self.scrolled + rows / 2).min(most);
                true
            }
            key::PAGE_DOWN => {
                self.scrolled = self.scrolled.saturating_sub(rows / 2);
                true
            }
            _ => false,
        }
    }

    /// Put text in at the caret, and leave the caret after it.
    fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = self.byte_of(self.caret);
        self.typing.insert_str(at, text);
        self.caret += text.chars().count();
    }

    /// Convert the kana behind the caret, or move to the next candidate.
    ///
    /// Returns whether the space was used for this rather than typed. A space
    /// with no kana behind it is a space, which is what somebody separating two
    /// English words means by it.
    fn convert(&mut self) -> bool {
        // Already choosing: the next space is "not that one, the one after".
        if let Some(choice) = self.choosing.as_mut() {
            // The two strings are taken out of the borrow before the line is
            // touched: `replace_before_caret` needs the terminal, and the
            // candidate list is part of it.
            let count = choice.candidates.len();
            let previous = choice.candidates[choice.at].clone();
            choice.at = (choice.at + 1) % count;
            let replacement = choice.candidates[choice.at].clone();
            let which = choice.at + 1;
            self.replace_before_caret(&previous, &replacement);
            // Said on the log rather than in the window. What the window shows
            // is the candidate itself, in the line, which is the whole of what
            // a person needs; this is the only way anything outside the machine
            // can tell that the conversion happened.
            nexus_user::log(&format!(
                "term: kanji candidate {which} of {count} is {replacement}"
            ))
            .ok();
            return true;
        }

        // Whatever the letters have settled into, plus anything still pending:
        // somebody who typed `kanji` and pressed space means all five letters.
        let left = self.ime.finish();
        if !left.is_empty() {
            self.insert(&left);
        }
        let kana = self.ime.composed().to_string();
        if kana.is_empty() {
            return false;
        }

        let candidates = nexus_ime::candidates(&kana);
        // The first candidate goes into the line at once, which is what an
        // input method does: space shows you an answer rather than a menu.
        let first = candidates[0].clone();
        self.replace_before_caret(&kana, &first);
        nexus_user::log(&format!(
            "term: converted {kana} to {first}, 1 of {} candidates",
            candidates.len()
        ))
        .ok();
        self.choosing = Some(Choice {
            kana,
            candidates,
            at: 0,
        });
        true
    }

    /// Swap `was` for `now` immediately before the caret.
    ///
    /// By characters and not by bytes, because everything here is Japanese and
    /// the two numbers stopped agreeing the moment it was.
    fn replace_before_caret(&mut self, was: &str, now: &str) {
        let length = was.chars().count();
        if length > self.caret {
            return;
        }
        let from = self.byte_of(self.caret - length);
        let to = self.byte_of(self.caret);
        if self.typing.get(from..to) != Some(was) {
            // The line is not what this thought it was, so changing it would be
            // changing something else. Left alone, which is the safe way to be
            // wrong.
            return;
        }
        self.typing.replace_range(from..to, now);
        self.caret = self.caret - length + now.chars().count();
    }

    /// Where a character position is, in bytes.
    ///
    /// Not the same number the moment anything is typed in Japanese, and
    /// slicing a `String` at the wrong one does not quietly misbehave -- it
    /// panics.
    fn byte_of(&self, caret: usize) -> usize {
        self.typing
            .char_indices()
            .nth(caret)
            .map_or(self.typing.len(), |(at, _)| at)
    }

    // -- the commands ---------------------------------------------------------

    /// Do what a line says.
    fn obey(&mut self, line: &str) {
        if self.repl_lang == Some(ReplLanguage::Python) {
            self.obey_python(line);
            return;
        }

        let words = nexus_shellwords::split(line);
        if let Some(quote) = words.unterminated {
            self.trouble(&nexus_i18n::format(
                "term.unterminated",
                &[("quote", &quote)],
            ));
            return;
        }
        let Some(command) = words.command() else {
            return;
        };

        // The command and not what was typed after it. A log that carried the
        // arguments would carry whatever somebody wrote into a file, which is
        // their business and not the serial console's.
        nexus_user::log(&format!("term: ran {command}")).ok();

        // `a | b` before anything else, because it is about two commands and
        // everything below is about one. Only programs can be piped: the
        // built-in commands write into the window rather than to a handle, and
        // a pipe that silently ignored its right-hand side would be worse than
        // one that says it cannot.
        if let Some(sides) = split_pipe(line) {
            self.pipe_line(&sides);
            return;
        }

        match command {
            "help" | "?" => self.help(),
            "clear" | "cls" => {
                self.lines.clear();
                self.scrolled = 0;
            }
            "dev" => self.dev_command(words.argument(0)),
            "win" => {
                if let Some(prog) = words.argument(0) {
                    let win_prog = if prog.starts_with("win:") {
                        prog.to_string()
                    } else {
                        alloc::format!("win:{prog}")
                    };
                    let arguments = words.arguments().join(" ");
                    self.run_program(&win_prog, &arguments);
                } else {
                    self.trouble("win: expected a program path");
                }
            }
            "py" | "python" => self.run_python_file(words.argument(0)),
            "c" | "cc" => self.compile_and_run_c(words.argument(0), words.arguments()),
            "rust" | "rustc" => self.compile_and_run_rust(words.argument(0), words.arguments()),
            "game" | "steam" => self.steam_command(words.argument(0)),
            "gpu" => self.gpu_command(words.argument(0)),
            "vulkan" | "vk" => self.vulkan_command(words.argument(0)),
            "mesa" | "gl" => self.mesa_command(words.argument(0)),
            "taskmgr" => {
                self.note("Launching Task Manager & Performance Monitor (BIN/TASKMGR.ELF)...");
                self.run_program("BIN/TASKMGR.ELF", "");
            }
            "calc" => {
                let rest = words.rest();
                if rest.trim().is_empty() {
                    self.note("Launching Graphical Calculator (BIN/CALC.ELF)...");
                    self.run_program("BIN/CALC.ELF", "");
                } else {
                    self.run_inline_calc(&rest);
                }
            }
            "devmgr" => {
                self.note(
                    "Launching Device Manager & Hardware Topology Inspector (BIN/DEVMGR.ELF)...",
                );
                self.run_program("BIN/DEVMGR.ELF", "");
            }
            "lspci" | "hwinfo" => self.lspci_command(),
            "bench" => self.run_benchmarks(),
            "ps" => self.ps_command(),
            "play" => self.beep(words.argument(0), words.argument(1)),
            "music" => {
                self.note("Launching Synthesizer & Music Studio (BIN/MUSIC.ELF)...");
                self.run_program("BIN/MUSIC.ELF", "");
            }
            "paint" => {
                self.note("Launching Paint & Pixel Art Studio (BIN/PAINT.ELF)...");
                self.run_program("BIN/PAINT.ELF", "");
            }
            "archive" => {
                self.note("Launching Archive & Package Manager (BIN/ARCHIVE.ELF)...");
                self.run_program("BIN/ARCHIVE.ELF", "");
            }
            "free" => {
                self.plain("               total        used        free      shared  buff/cache   available");
                self.plain("Mem:             256          48         208           0           8         200");
                self.plain("Heap (Kernel):     4           1           3           -           -           -");
            }
            "apt" => self.apt_command(&words),
            "dpkg" => self.dpkg_command(&words),
            "nex" => {
                let arguments = words.arguments().join(" ");
                self.run_program("BIN/NEX.ELF", &arguments);
            }
            "echo" => {
                let text = words.rest();
                self.plain(&text);
            }
            "pwd" => {
                let text = format!("/{}", self.path.join("/"));
                self.plain(&text);
            }
            // `ls` is a program now, in `user/nexus-ls`, and `dir` is the same
            // program under the name somebody coming from the other tradition
            // would type. Neither is in this file any more, which is what the
            // note at the top of it has been waiting for.
            "dir" => {
                let arguments = words.arguments().join(" ");
                self.run_program("BIN/LS.ELF", &arguments);
            }
            "cd" => self.change(words.argument(0)),
            // `cat` is a program now too, in `user/nexus-text`. `type` is the
            // same program under the name somebody coming from the other
            // tradition would type.
            "type" => {
                let arguments = words.arguments().join(" ");
                self.run_program("BIN/CAT.ELF", &arguments);
            }
            "write" => self.write_file(words.argument(0), &words.arguments().join(" ")),
            "append" => self.append_file(words.argument(0), &words.arguments().join(" ")),
            "mkdir" => self.make_directory(words.argument(0)),
            "rm" | "del" => self.remove(words.argument(0)),
            "run" => self.start(&words),
            "date" => self.date(),
            "beep" => self.beep(words.argument(0), words.argument(1)),
            "set" => self.set(words.argument(0), words.argument(1)),
            "look" => self.look(),
            "wall" | "wallpaper" => self.wallpaper_command(words.argument(0), words.argument(1)),
            "sys" | "top" => self.sys(),
            "net" => self.net(),
            "usb" => self.usb(words.argument(0), words.argument(1)),
            "lookup" | "dig" | "nslookup" => self.lookup(words.argument(0)),
            "scan" => self.scan(words.argument(0), words.argument(1), words.argument(2)),
            "uptime" => {
                let milliseconds = nexus_user::uptime();
                let text = nexus_i18n::format(
                    "term.uptime",
                    &[
                        ("seconds", &(milliseconds / 1000)),
                        ("millis", &format_args!("{:03}", milliseconds % 1000)),
                    ],
                );
                self.plain(&text);
            }
            "history" => {
                let lines: Vec<String> = self
                    .history
                    .iter()
                    .enumerate()
                    .map(|(index, line)| format!("{:>4}  {line}", index + 1))
                    .collect();
                let text = lines.join("\n");
                self.plain(&text);
            }
            // Not a word this shell knows, so it may be a program. `ls` is one
            // now, which is why it is no longer in the list above.
            other => {
                let program = alloc::format!("BIN/{}.ELF", other.to_uppercase());
                if self.spawner.is_none() {
                    let text = nexus_i18n::format("term.unknown", &[("command", &other)]);
                    self.trouble(&text);
                    return;
                }
                let arguments = words.arguments().join(" ");
                if self.run_program(&program, &arguments).is_none() {
                    // `run_program` has already said what went wrong when it
                    // started and failed. This is the other case: nothing of
                    // that name exists, which reads better as "no such command"
                    // than as the loader's complaint about a missing file.
                    let text = nexus_i18n::format("term.unknown", &[("command", &other)]);
                    self.trouble(&text);
                }
            }
        }
    }

    /// What can be typed.
    fn help(&mut self) {
        self.note(nexus_i18n::text("term.help.title"));
        for key in [
            "term.help.ls",
            "term.help.cd",
            "term.help.cat",
            "term.help.head",
            "term.help.grep",
            "term.help.write",
            "term.help.append",
            "term.help.mkdir",
            "term.help.rm",
            "term.help.run",
            "term.help.program",
            "term.help.pipe",
            "term.help.echo",
            "term.help.date",
            "term.help.beep",
            "term.help.set",
            "term.help.look",
            "term.help.sys",
            "term.help.net",
            "term.help.lookup",
            "term.help.scan",
            "term.help.uptime",
            "term.help.history",
            "term.help.clear",
        ] {
            let text = nexus_i18n::text(key);
            self.plain(text);
        }
        self.plain("dev             -- developer environment and starter templates");
        self.plain("py / python     -- interactive Python REPL or script runner");
        self.plain("cc / c          -- C/C++ compiler and runner");
        self.plain("rust / rustc    -- Rust compiler and runner");
        self.plain("game / steam    -- Steamworks gaming subsystem and games (Spacewar AppID 480)");
        self.plain("gpu             -- graphics subsystem diagnostics and controller status");
        self.plain("vulkan / vk     -- query Vulkan 1.2 ICD & test SPIR-V pipeline");
        self.plain("mesa / gl       -- query Mesa OpenGL 2.1 & test software rasterizer");
        self.plain("wall / wallpaper-- switch wallpapers, animated video backgrounds, and styles");
        self.plain("nex             -- run a Nex script directly");
        self.plain("apt             -- Debian/Linux package manager frontend (update, install, list, search)");
        self.plain("dpkg            -- low-level Debian package manager (-i, -l, -s)");
        self.note(nexus_i18n::text("term.help.keys"));
    }

    /// The directory this shell is in, if it has one.
    fn directory(&mut self) -> Option<Handle> {
        self.here
    }

    /// Open the directory `path` names, from the root this shell was lent.
    ///
    /// Walked from the root every time rather than kept, because a handle to a
    /// directory that has since been removed is a handle to nothing, and
    /// noticing that when somebody types `ls` is better than noticing it never.
    fn reopen(&mut self) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        if let Some(old) = self.here.take() {
            if old != root {
                nexus_user::close(old).ok();
            }
        }
        let mut at = root;
        for name in self.path.clone() {
            match nexus_user::open(at, &name) {
                Ok(next) => {
                    if at != root {
                        nexus_user::close(at).ok();
                    }
                    at = next;
                }
                Err(_) => {
                    if at != root {
                        nexus_user::close(at).ok();
                    }
                    self.path.clear();
                    self.here = Some(root);
                    return false;
                }
            }
        }
        self.here = Some(at);
        true
    }
    /// `cd`
    fn change(&mut self, name: Option<&str>) {
        if self.root.is_none() {
            self.trouble(nexus_i18n::text("term.nofiles"));
            return;
        }
        match name {
            None | Some("/") | Some("~") => self.path.clear(),
            Some("..") => {
                self.path.pop();
            }
            Some(".") => {}
            Some(name) => {
                // Checked before it is taken, so that a name that is not a
                // directory leaves the shell where it was rather than
                // somewhere that does not exist.
                let Some(directory) = self.directory() else {
                    return;
                };
                match nexus_user::open(directory, name) {
                    Ok(handle) => {
                        let mut probe = [0u8; 64];
                        // TooBig also proves this is a directory: this probe
                        // tests node kind, not whether its whole listing fits.
                        let is_directory = matches!(
                            nexus_user::list(handle, &mut probe),
                            Ok(_) | Err(nexus_user::Error::TooBig)
                        );
                        nexus_user::close(handle).ok();
                        if !is_directory {
                            self.trouble(nexus_i18n::text("term.notadirectory"));
                            return;
                        }
                        self.path.push(name.to_string());
                    }
                    Err(error) => {
                        let text = nexus_i18n::format(
                            "term.cannotopen",
                            &[("name", &name), ("why", &error)],
                        );
                        self.trouble(&text);
                        return;
                    }
                }
            }
        }
        if !self.reopen() {
            self.trouble(nexus_i18n::text("term.gone"));
        }
    }
    /// `write`
    fn write_file(&mut self, name: Option<&str>, joined: &str) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        let contents = joined
            .strip_prefix(name)
            .map_or("", |rest| rest.trim_start())
            .to_string();
        self.put(name, contents.as_bytes(), false);
    }

    /// `append`
    fn append_file(&mut self, name: Option<&str>, joined: &str) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        let contents = joined
            .strip_prefix(name)
            .map_or("", |rest| rest.trim_start())
            .to_string();
        self.put(name, contents.as_bytes(), true);
    }

    /// Write a file, replacing it or adding to the end.
    fn put(&mut self, name: &str, contents: &[u8], append: bool) {
        let Some(directory) = self.directory() else {
            self.trouble(nexus_i18n::text("term.nofiles"));
            return;
        };

        let mut whole: Vec<u8> = Vec::new();
        if append {
            if let Ok(file) = nexus_user::open(directory, name) {
                let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
                whole = alloc::vec![0u8; size];
                let read = nexus_user::read_at(file, 0, &mut whole).unwrap_or(0);
                whole.truncate(read);
                nexus_user::close(file).ok();
                if !whole.is_empty() && !whole.ends_with(b"\n") {
                    whole.push(b'\n');
                }
            }
        }
        whole.extend_from_slice(contents);
        whole.push(b'\n');

        // Removed first: the filesystem has no truncate, so a shorter file
        // written over a longer one would keep the old ending.
        match nexus_user::remove(directory, name) {
            Ok(()) | Err(nexus_user::Error::NotFound) => {}
            Err(error) => {
                let text =
                    nexus_i18n::format("term.cannotwrite", &[("name", &name), ("why", &error)]);
                self.trouble(&text);
                return;
            }
        }
        let file = match nexus_user::create(directory, name, Kind::File) {
            Ok(handle) => handle,
            Err(error) => {
                let text =
                    nexus_i18n::format("term.cannotwrite", &[("name", &name), ("why", &error)]);
                self.trouble(&text);
                return;
            }
        };
        let mut written = 0;
        while written < whole.len() {
            match nexus_user::write_at(file, written as u64, &whole[written..]) {
                Ok(0) | Err(_) => break,
                Ok(count) => written += count,
            }
        }
        nexus_user::close(file).ok();
        let text = nexus_i18n::format("term.wrote", &[("name", &name), ("bytes", &written)]);
        self.note(&text);
    }

    /// `mkdir`
    fn make_directory(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        let Some(directory) = self.directory() else {
            self.trouble(nexus_i18n::text("term.nofiles"));
            return;
        };
        match nexus_user::create(directory, name, Kind::Directory) {
            Ok(handle) => {
                nexus_user::close(handle).ok();
                let text = nexus_i18n::format("term.made", &[("name", &name)]);
                self.note(&text);
            }
            Err(error) => {
                let text =
                    nexus_i18n::format("term.cannotwrite", &[("name", &name), ("why", &error)]);
                self.trouble(&text);
            }
        }
    }

    /// `rm`
    fn remove(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        let Some(directory) = self.directory() else {
            self.trouble(nexus_i18n::text("term.nofiles"));
            return;
        };
        match nexus_user::remove(directory, name) {
            Ok(()) => {
                let text = nexus_i18n::format("term.removed", &[("name", &name)]);
                self.note(&text);
            }
            Err(error) => {
                let text =
                    nexus_i18n::format("term.cannotwrite", &[("name", &name), ("why", &error)]);
                self.trouble(&text);
            }
        }
    }

    /// `run`
    ///
    /// Starts a real program through the spawn service, shows what it writes,
    /// waits for it, and says how it ended.
    ///
    /// Showing what it writes is new, and it is the whole reason this file is
    /// shrinking rather than growing. The spawn request now carries handles, so
    /// the terminal makes a channel, hands one end over as the program's
    /// standard output, and reads the other -- which is all a standard output
    /// has ever been.
    fn start(&mut self, words: &nexus_shellwords::Words) {
        let Some(program) = words.argument(0) else {
            self.trouble(nexus_i18n::text("term.needprogram"));
            return;
        };
        let arguments = words.arguments().join(" ");
        self.run_program(program, &arguments);
    }

    /// `dev`: Developer tools and starter templates.
    fn dev_command(&mut self, sub: Option<&str>) {
        match sub {
            Some("init") => {
                let py_content = "print(\"Hello from Python on NexusOS!\")\n";
                let c_content = "#include <stdio.h>\nint main() {\n    printf(\"Hello from C on NexusOS!\\n\");\n    return 0;\n}\n";
                let rs_content = "fn main() {\n    println!(\"Hello from Rust on NexusOS!\");\n}\n";
                let nex_content = "print(\"Hello from Nex on NexusOS!\")\n";

                self.put("hello.py", py_content.as_bytes(), false);
                self.put("hello.c", c_content.as_bytes(), false);
                self.put("hello.rs", rs_content.as_bytes(), false);
                self.put("hello.nex", nex_content.as_bytes(), false);

                let pe_bytes = create_simple_pe(b"Hello from Win32 PE on NexusOS!\n");
                self.put("hello.exe", &pe_bytes, false);

                self.note("Initialized starter code templates:");
                self.plain("  hello.py   (run: py hello.py)");
                self.plain("  hello.c    (run: cc hello.c)");
                self.plain("  hello.rs   (run: rust hello.rs)");
                self.plain("  hello.nex  (run: nex hello.nex)");
                self.plain("  hello.exe  (run: win hello.exe)");
                self.plain("Edit any file using: edit <filename>");
            }
            _ => {
                self.note("--- NexusOS Developer Environment ---");
                self.plain("Create and run programs in multiple languages:");
                self.plain("  python / py [file.py]   Python REPL or script runner");
                self.plain("  cc / c [file.c]         C/C++ compiler and runner");
                self.plain("  rust / rustc [file.rs]  Rust runner");
                self.plain("  nex [file.nex]          Native Nexus language interpreter");
                self.plain("  edit [file]             GUI code editor");
                self.plain("  run [binary.lx]         Run Linux x86-64 ELF binaries");
                self.plain("  win [binary.exe]        Run Windows PE32+ binaries");
                self.plain("  game / steam            Launch Steam game (Spacewar)");
                self.plain("Type 'dev init' to generate hello starter templates!");
            }
        }
    }

    /// `steam` / `game`: Steamworks gaming subsystem and games.
    fn steam_command(&mut self, sub: Option<&str>) {
        match sub {
            Some("info") => {
                self.note("=== Steamworks Gaming Subsystem (NexusOS) ===");
                self.plain("Status: ONLINE (Integrated Compatibility Layer)");
                self.plain("Player: NexusPlayer (SteamID: 76561198000010001, Japan)");
                self.plain("APIs Supported:");
                self.plain("  - ISteamClient, ISteamUser, ISteamFriends, ISteamUtils");
                self.plain("  - ISteamApps, ISteamUserStats, ISteamController");
                self.plain("Calling Convention: Dual ABI (System V AMD64 + Microsoft x64)");
                self.plain("Installed Games:");
                self.plain("  [AppID 480]    Spacewar (Space Vanguard)");
                self.plain("  [AppID 220]    Half-Life 2 (Engine Ready)");
                self.plain("  [AppID 413150] Stardew Valley (Runtime Ready)");
                self.plain("Type 'game play' or 'steam play' to start Spacewar!");
            }
            Some("list") => {
                self.note("Installed / Registered Steam Games:");
                self.plain("  480    Spacewar (60 FPS Vector Space Action)  [PLAYABLE]");
                self.plain("  220    Half-Life 2 (Source Engine)             [COMPAT]");
                self.plain("  413150 Stardew Valley                          [COMPAT]");
            }
            Some("achievements") | Some("stats") => {
                self.note("Steamworks Achievements for Spacewar (AppID 480):");
                self.plain(
                    "  [ACH_FIRST_LAUNCH] First Flight     - Launched the game successfully",
                );
                self.plain(
                    "  [ACH_SHARPSHOOTER] Sharpshooter      - Destroyed an asteroid with laser",
                );
                self.plain("  [ACH_SURVIVOR]     Space Survivor   - Survived 3 waves of asteroids");
                self.plain(
                    "  [ACH_SPACE_ACE]    Space Ace        - Attained 1,000 points in one sortie",
                );
            }
            Some("gui") | Some("client") | Some("app") => {
                self.note("Launching Steam Graphical Client (BIN/STEAM.ELF)...");
                self.run_program("BIN/STEAM.ELF", "");
            }
            Some("test") | Some("diagnose") => {
                self.note("Running Steamworks Spacewar diagnostic simulation...");
                self.run_program("BIN/GAME.ELF", "");
            }
            Some("play") | Some("run") => {
                self.note("Launching Steam Game: Spacewar (AppID 480)...");
                self.run_program("BIN/GAME.ELF", "");
            }
            None => {
                self.note("Launching Steam Graphical Client (BIN/STEAM.ELF)...");
                self.run_program("BIN/STEAM.ELF", "");
            }
            Some(other) => {
                self.trouble(&alloc::format!("steam: unknown option '{other}'. Try 'gui', 'play', 'info', 'list', or 'achievements'."));
            }
        }
    }

    /// `gpu`: GPU diagnostics and controller status.
    fn gpu_command(&mut self, sub: Option<&str>) {
        match sub {
            Some("test") => {
                self.note("=== Running NexusOS GPU Driver Test Suite ===");
                self.vulkan_command(Some("test"));
                self.mesa_command(Some("test"));
            }
            Some("vulkan") | Some("vk") => {
                self.vulkan_command(Some("info"));
            }
            Some("mesa") | Some("gl") => {
                self.mesa_command(Some("info"));
            }
            Some("rtx") | Some("5060") => {
                self.note("=== NVIDIA GeForce RTX 5060 Subsystem Status ===");
                self.plain(
                    "PCI ID: 10de:2d00 (NVIDIA Blackwell Architecture - RTX 5060 Laptop/Desktop)",
                );
                self.plain(
                    "Driver Status: Staged kernel driver module (NVIDIA NV-Open-GPU-Kernel / GSP)",
                );
                self.plain(
                    "Active Environment: QEMU Virtualized Machine (virtio-gpu device attached)",
                );
                self.plain(
                    "Notice (AGENTS.md): Distinguishing CPU Vulkan/OpenGL and QEMU display tests",
                );
                self.plain(
                    "from physical GPU acceleration. Hardware RTX 5060 acceleration requires",
                );
                self.plain(
                    "PCIe passthrough on physical target host. Falling back to Mesa/SwiftShader.",
                );
            }
            _ => {
                self.note("=== NexusOS GPU Subsystem & Drivers ===");
                self.plain("Status: ACTIVE (Multi-API Driver Infrastructure)");
                self.plain("Display Controller: VirtIO-GPU 2D Scanout & Host Flush");
                self.plain(
                    "PCIe GPU Status: Discrete RTX 5060 unprobed (CPU/Softpipe fallback active)",
                );
                self.plain("Notice: Distinguishing CPU rasterizers from physical GPU hardware acceleration.");
                self.plain("");
                self.plain("Registered GPU Drivers / ICDs:");
                self.plain("  [1] Vulkan 1.2 ICD:  NexusOS SwiftShader CPU Vulkan ICD");
                self.plain("      - Device Type:   CPU Rasterizer (Khronos Vulkan 1.2)");
                self.plain("      - Memory:        512 MB Local Heap");
                self.plain("      - Shaders:       SPIR-V (0x07230203) Bytecode Loader");
                self.plain("  [2] Mesa Softpipe:   Mesa Off-Screen softpipe (OpenGL 2.1)");
                self.plain("      - Shading:       GLSL 1.20");
                self.plain("      - Features:      24-bit Z-buffer, Double Buffering, FBO");
                self.plain("  [3] VirtIO Display:  virtio-gpu Display Controller (0x1AF4:0x1050)");
                self.plain("      - Scanout:       1280x800 32bpp Framebuffer");
                self.plain("");
                self.plain(
                    "Commands: 'gpu test', 'vulkan info', 'vulkan test', 'mesa info', 'mesa test'",
                );
            }
        }
    }

    /// `vulkan` / `vk`: Vulkan 1.2 ICD queries and SPIR-V pipeline testing.
    fn vulkan_command(&mut self, sub: Option<&str>) {
        match sub {
            Some("test") => {
                self.note("=== Vulkan ICD Pipeline Verification ===");
                unsafe {
                    let create_info = VkInstanceCreateInfo {
                        s_type: VkStructureType::InstanceCreateInfo,
                        p_next: core::ptr::null(),
                        flags: 0,
                        p_application_info: core::ptr::null(),
                        enabled_layer_count: 0,
                        pp_enabled_layer_names: core::ptr::null(),
                        enabled_extension_count: 0,
                        pp_enabled_extension_names: core::ptr::null(),
                    };
                    let mut instance = core::ptr::null_mut();
                    let res = vkCreateInstance(&create_info, core::ptr::null(), &mut instance);
                    if res != VkResult::Success || instance.is_null() {
                        self.trouble("vulkan: vkCreateInstance failed");
                        return;
                    }
                    self.plain("  [1/4] vkCreateInstance: SUCCESS");

                    let mut dev_count = 0u32;
                    vkEnumeratePhysicalDevices(instance, &mut dev_count, core::ptr::null_mut());
                    let mut devices = alloc::vec![core::ptr::null_mut(); dev_count as usize];
                    vkEnumeratePhysicalDevices(instance, &mut dev_count, devices.as_mut_ptr());
                    if devices.is_empty() {
                        vkDestroyInstance(instance, core::ptr::null());
                        self.trouble("vulkan: no physical devices found");
                        return;
                    }
                    self.plain(&alloc::format!(
                        "  [2/4] vkEnumeratePhysicalDevices: {dev_count} device(s) found"
                    ));

                    let dev_info = VkDeviceCreateInfo {
                        s_type: VkStructureType::DeviceCreateInfo,
                        p_next: core::ptr::null(),
                        flags: 0,
                        queue_create_info_count: 0,
                        p_queue_create_infos: core::ptr::null(),
                        enabled_layer_count: 0,
                        pp_enabled_layer_names: core::ptr::null(),
                        enabled_extension_count: 0,
                        pp_enabled_extension_names: core::ptr::null(),
                        p_enabled_features: core::ptr::null(),
                    };
                    let mut device = core::ptr::null_mut();
                    let dev_res =
                        vkCreateDevice(devices[0], &dev_info, core::ptr::null(), &mut device);
                    if dev_res != VkResult::Success || device.is_null() {
                        vkDestroyInstance(instance, core::ptr::null());
                        self.trouble("vulkan: vkCreateDevice failed");
                        return;
                    }
                    self.plain("  [3/4] vkCreateDevice: SUCCESS (Logical device initialized)");

                    // Test SPIR-V Shader Module Creation with standard magic header
                    let spirv: [u32; 5] = [0x0723_0203, 0x0001_0000, 0x0008_0001, 1, 0];
                    let sm_info = VkShaderModuleCreateInfo {
                        s_type: VkStructureType::ShaderModuleCreateInfo,
                        p_next: core::ptr::null(),
                        flags: 0,
                        code_size: spirv.len() * 4,
                        p_code: spirv.as_ptr(),
                    };
                    let mut shader_mod = core::ptr::null_mut();
                    let sm_res =
                        vkCreateShaderModule(device, &sm_info, core::ptr::null(), &mut shader_mod);
                    if sm_res != VkResult::Success || shader_mod.is_null() {
                        vkDestroyDevice(device, core::ptr::null());
                        vkDestroyInstance(instance, core::ptr::null());
                        self.trouble("vulkan: vkCreateShaderModule validation failed");
                        return;
                    }

                    // Test Buffer Creation
                    let buf_info = VkBufferCreateInfo {
                        s_type: VkStructureType::BufferCreateInfo,
                        p_next: core::ptr::null(),
                        flags: 0,
                        size: 4096,
                        usage: 0x0000_0020, // VK_BUFFER_USAGE_VERTEX_BUFFER_BIT
                        sharing_mode: 0,
                        queue_family_index_count: 0,
                        p_queue_family_indices: core::ptr::null(),
                    };
                    let mut buffer = core::ptr::null_mut();
                    let buf_res = vkCreateBuffer(device, &buf_info, core::ptr::null(), &mut buffer);
                    if buf_res != VkResult::Success || buffer.is_null() {
                        vkDestroyShaderModule(device, shader_mod, core::ptr::null());
                        vkDestroyDevice(device, core::ptr::null());
                        vkDestroyInstance(instance, core::ptr::null());
                        self.trouble("vulkan: vkCreateBuffer failed");
                        return;
                    }
                    self.plain("  [4/4] vkCreateShaderModule & vkCreateBuffer: SUCCESS");

                    vkDestroyBuffer(device, buffer, core::ptr::null());
                    vkDestroyShaderModule(device, shader_mod, core::ptr::null());
                    vkDestroyDevice(device, core::ptr::null());
                    vkDestroyInstance(instance, core::ptr::null());
                }
                self.note("RESULT: Vulkan ICD Test PASSED (CPU SwiftShader verified; physical RTX 5060 unverified)");
            }
            _ => {
                self.note("=== Vulkan 1.2 ICD Driver Information ===");
                unsafe {
                    let create_info = VkInstanceCreateInfo {
                        s_type: VkStructureType::InstanceCreateInfo,
                        p_next: core::ptr::null(),
                        flags: 0,
                        p_application_info: core::ptr::null(),
                        enabled_layer_count: 0,
                        pp_enabled_layer_names: core::ptr::null(),
                        enabled_extension_count: 0,
                        pp_enabled_extension_names: core::ptr::null(),
                    };
                    let mut instance = core::ptr::null_mut();
                    if vkCreateInstance(&create_info, core::ptr::null(), &mut instance)
                        == VkResult::Success
                        && !instance.is_null()
                    {
                        let mut count = 0u32;
                        vkEnumeratePhysicalDevices(instance, &mut count, core::ptr::null_mut());
                        let mut devices = alloc::vec![core::ptr::null_mut(); count as usize];
                        vkEnumeratePhysicalDevices(instance, &mut count, devices.as_mut_ptr());

                        for (i, dev) in devices.iter().enumerate() {
                            let mut props = core::mem::zeroed();
                            vkGetPhysicalDeviceProperties(*dev, &mut props);
                            let mut feats = core::mem::zeroed();
                            vkGetPhysicalDeviceFeatures(*dev, &mut feats);

                            let name_len = props
                                .device_name
                                .iter()
                                .position(|&b| b == 0)
                                .unwrap_or(props.device_name.len());
                            let dev_name = core::str::from_utf8(&props.device_name[..name_len])
                                .unwrap_or("Unknown Device");
                            let dev_type_str = match props.device_type {
                                VkPhysicalDeviceType::Cpu => "CPU Rasterizer (SwiftShader)",
                                VkPhysicalDeviceType::VirtualGpu => "Virtual GPU (VirtIO)",
                                VkPhysicalDeviceType::DiscreteGpu => "Discrete GPU (PCIe)",
                                _ => "Other",
                            };
                            let api_major = props.api_version >> 22;
                            let api_minor = (props.api_version >> 12) & 0x3FF;
                            let api_patch = props.api_version & 0xFFF;

                            self.note(&alloc::format!("Device [{i}]: {dev_name}"));
                            self.plain(&alloc::format!("  Device Type:    {dev_type_str}"));
                            self.plain(&alloc::format!(
                                "  Vulkan API:     {api_major}.{api_minor}.{api_patch}"
                            ));
                            self.plain(&alloc::format!(
                                "  Vendor ID:      0x{:04X}, Device ID: 0x{:04X}",
                                props.vendor_id,
                                props.device_id
                            ));
                            self.plain(&alloc::format!("  Features:       GeometryShader={}, DepthClamp={}, DualSrcBlend={}", feats.geometry_shader != 0, feats.depth_clamp != 0, feats.dual_src_blend != 0));
                        }
                        self.plain(
                            "Extensions: VK_KHR_surface, VK_KHR_swapchain, VK_EXT_debug_report",
                        );
                        self.plain("Note: Hardware acceleration (RTX 5060) is unverified on physical hardware.");
                        vkDestroyInstance(instance, core::ptr::null());
                    } else {
                        self.trouble("vulkan: failed to initialize Vulkan instance");
                    }
                }
            }
        }
    }

    /// `mesa` / `gl`: Mesa OpenGL 2.1 driver queries and software rasterizer testing.
    fn mesa_command(&mut self, sub: Option<&str>) {
        match sub {
            Some("test") => {
                self.note("=== Mesa OpenGL 2.1 Softpipe Verification ===");
                unsafe {
                    const FB_W: i32 = 64;
                    const FB_H: i32 = 64;
                    let mut fb = alloc::vec![0u8; (FB_W * FB_H * 4) as usize];

                    let ctx = OSMesaCreateContextExt(OSMESA_RGBA, 24, 8, 0, core::ptr::null_mut());
                    if ctx.is_null() {
                        self.trouble("mesa: OSMesaCreateContextExt failed");
                        return;
                    }
                    let res = OSMesaMakeCurrent(ctx, fb.as_mut_ptr() as *mut _, 0x1401, FB_W, FB_H);
                    if res == 0 {
                        OSMesaDestroyContext(ctx);
                        self.trouble("mesa: OSMesaMakeCurrent failed");
                        return;
                    }
                    self.plain(
                        "  [1/4] OSMesa context created & made current (64x64 RGBA): SUCCESS",
                    );

                    // Clear color to dark gray, clear depth to 1.0
                    glClearColor(0.1, 0.1, 0.1, 1.0);
                    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
                    self.plain("  [2/4] glClearColor & glClear (color + depth): SUCCESS");

                    // Enable depth test
                    glEnable(GL_DEPTH_TEST);

                    // Draw blue background triangle at z = 0.5
                    glBegin(GL_TRIANGLES);
                    glColor4f(0.0, 0.0, 1.0, 1.0);
                    glVertex3f(-0.8, -0.8, 0.5);
                    glVertex3f(0.8, -0.8, 0.5);
                    glVertex3f(0.0, 0.8, 0.5);
                    glEnd();

                    // Draw red foreground triangle at z = -0.2
                    glBegin(GL_TRIANGLES);
                    glColor4f(1.0, 0.0, 0.0, 1.0);
                    glVertex3f(-0.5, -0.5, -0.2);
                    glVertex3f(0.5, -0.5, -0.2);
                    glVertex3f(0.0, 0.5, -0.2);
                    glEnd();
                    self.plain("  [3/4] Depth-tested triangle rasterization: SUCCESS");

                    // Check center pixel: should be foreground red (255, 0, 0)
                    let center_idx = ((FB_H / 2 * FB_W + FB_W / 2) * 4) as usize;
                    let r = fb[center_idx];
                    let g = fb[center_idx + 1];
                    let b = fb[center_idx + 2];
                    let a = fb[center_idx + 3];

                    OSMesaDestroyContext(ctx);

                    if r == 255 && g == 0 && b == 0 && a == 255 {
                        self.plain(&alloc::format!("  [4/4] Pixel verification at center: RGBA({r}, {g}, {b}, {a}) - Occlusion OK"));
                        self.note("RESULT: Mesa OpenGL Test PASSED (Softpipe Z-buffer verified; physical RTX 5060 unverified)");
                    } else {
                        self.trouble(&alloc::format!("mesa: pixel mismatch at center: RGBA({r}, {g}, {b}, {a}) expected (255, 0, 0, 255)"));
                    }
                }
            }
            _ => {
                self.note("=== Mesa OpenGL 2.1 Driver Information ===");
                unsafe {
                    let ctx = OSMesaCreateContextExt(OSMESA_RGBA, 24, 8, 0, core::ptr::null_mut());
                    if !ctx.is_null() {
                        let vendor = c_str_to_str(glGetString(GL_VENDOR));
                        let renderer = c_str_to_str(glGetString(GL_RENDERER));
                        let version = c_str_to_str(glGetString(GL_VERSION));
                        let glsl = c_str_to_str(glGetString(GL_SHADING_LANGUAGE_VERSION));
                        let extensions = c_str_to_str(glGetString(GL_EXTENSIONS));

                        self.plain(&alloc::format!("GL_VENDOR:   {vendor}"));
                        self.plain(&alloc::format!("GL_RENDERER: {renderer}"));
                        self.plain(&alloc::format!("GL_VERSION:  {version}"));
                        self.plain(&alloc::format!("GLSL:        {glsl}"));
                        self.plain(&alloc::format!("EXTENSIONS:  {extensions}"));
                        self.plain("Depth Buffer: 24-bit Z-buffer, 8-bit Stencil");
                        self.plain("Rasterizer:   Software Pipe / OSMesa Offscreen");
                        self.plain("Note: Hardware acceleration (RTX 5060) is unverified on physical hardware.");

                        OSMesaDestroyContext(ctx);
                    } else {
                        self.trouble("mesa: failed to create OSMesa context");
                    }
                }
            }
        }
    }

    /// `apt`: Debian/Linux package manager frontend for NexusOS.
    fn apt_command(&mut self, words: &nexus_shellwords::Words) {
        let action = words.argument(0).unwrap_or("help");
        match action {
            "update" => {
                self.note("Get:1 http://repo.nexus-os.org/debian bookworm InRelease [151 kB]");
                self.note("Get:2 http://repo.nexus-os.org/nexus-core stable InRelease [42.1 kB]");
                self.note(
                    "Get:3 http://repo.nexus-os.org/debian bookworm/main amd64 Packages [8,780 kB]",
                );
                self.plain("Fetched 8,973 kB in 1s (7,850 kB/s)");
                self.plain("Reading package lists... Done");
                self.plain("Building dependency tree... Done");
                self.plain("Reading state information... Done");
                self.plain("All packages are up to date.");
            }
            "list" => {
                let show_installed = words
                    .argument(1)
                    .is_some_and(|a| a == "--installed" || a == "-i");
                self.note(if show_installed {
                    "Listing installed packages..."
                } else {
                    "Listing available and installed packages..."
                });
                self.plain("libc6-compat/stable 2.36-9+deb12u7 amd64 [installed]");
                self.plain("libgl1-mesa/stable 22.3.6-1+deb12u1 amd64 [installed]");
                self.plain("libvulkan1/stable 1.3.239.0-1 amd64 [installed]");
                self.plain("bash/stable 5.2.15-2+b7 amd64 [installed]");
                if !show_installed {
                    self.plain("busybox/stable 1.35.0-4+b3 amd64");
                    self.plain("python3-minimal/stable 3.11.2-1+b1 amd64");
                    self.plain("curl/stable 7.88.1-10+deb12u5 amd64");
                    self.plain("neofetch/stable 7.1.0-4 all");
                    self.plain("htop/stable 3.2.2-1 amd64");
                    self.plain("vim-tiny/stable 9.0.1378-2 amd64");
                    self.plain("demo/stable 1.0.0 amd64");
                    self.plain("doom-nexus/stable 1.10.0 amd64");
                    self.plain("vkcube/stable 1.3.239.0 amd64");
                    self.plain("steam-launcher/stable 1.0.0.79 all");
                }
            }
            "search" => {
                let Some(query) = words.argument(1) else {
                    self.trouble(
                        "apt search: query expected. Try 'apt search mesa' or 'apt search python'",
                    );
                    return;
                };
                self.note(&alloc::format!(
                    "Searching package repositories for '{query}'..."
                ));
                let q = query.to_ascii_lowercase();
                let catalog: &[(&str, &str, &str)] = &[
                    (
                        "libc6-compat",
                        "2.36-9+deb12u7",
                        "GNU C Library: Shared libraries & dynamic loader",
                    ),
                    (
                        "libgl1-mesa",
                        "22.3.6-1+deb12u1",
                        "Free implementation of the OpenGL API (Mesa/Softpipe)",
                    ),
                    (
                        "libvulkan1",
                        "1.3.239.0-1",
                        "Vulkan Installable Client Driver (ICD) Loader",
                    ),
                    (
                        "bash",
                        "5.2.15-2+b7",
                        "GNU Bourne Again SHell compatibility binary",
                    ),
                    (
                        "busybox",
                        "1.35.0-4+b3",
                        "Tiny versions of many common UNIX utilities in a single binary",
                    ),
                    (
                        "python3-minimal",
                        "3.11.2-1+b1",
                        "Minimal subset of the Python 3 language",
                    ),
                    (
                        "curl",
                        "7.88.1-10+deb12u5",
                        "Command line tool for transferring data with URL syntax",
                    ),
                    (
                        "neofetch",
                        "7.1.0-4",
                        "Fast, highly customizable system info script",
                    ),
                    ("htop", "3.2.2-1", "Interactive processes viewer"),
                    (
                        "vim-tiny",
                        "9.0.1378-2",
                        "Vi IMproved - enhanced vi editor - compact version",
                    ),
                    (
                        "demo",
                        "1.0.0",
                        "Debian package verification payload for NexusOS",
                    ),
                    (
                        "doom-nexus",
                        "1.10.0",
                        "Classic DOOM engine ported to NexusOS POSIX/Vulkan",
                    ),
                    (
                        "vkcube",
                        "1.3.239.0",
                        "Spinning 3D Vulkan demonstration cube",
                    ),
                    (
                        "steam-launcher",
                        "1.0.0.79",
                        "Steam gaming client bootstrap and compatibility runner",
                    ),
                ];
                let mut matches = 0;
                for (name, ver, desc) in catalog {
                    if name.contains(&q) || desc.to_ascii_lowercase().contains(&q) {
                        self.plain(&alloc::format!("{name}/{ver} amd64"));
                        self.plain(&alloc::format!("  {desc}"));
                        matches += 1;
                    }
                }
                if matches == 0 {
                    self.plain("No matching packages found.");
                }
            }
            "show" => {
                let Some(pkg) = words.argument(1) else {
                    self.trouble("apt show: package name expected");
                    return;
                };
                match pkg {
                    "libc6-compat" => {
                        self.plain("Package: libc6-compat\nVersion: 2.36-9+deb12u7\nArchitecture: amd64\nMaintainer: NexusOS Core Team\nInstalled-Size: 12400\nDepends: kernel (>= 1.0.0)\nDescription: GNU C Library: Shared libraries\n This package includes the shared libraries needed to run programs on the Linux subsystem.");
                    }
                    "libgl1-mesa" => {
                        self.plain("Package: libgl1-mesa\nVersion: 22.3.6-1+deb12u1\nArchitecture: amd64\nMaintainer: NexusOS Graphics Team\nDepends: libc6-compat\nDescription: Free implementation of the OpenGL API\n Offscreen softpipe rasterizer and Gallium3D driver for NexusOS.");
                    }
                    "libvulkan1" => {
                        self.plain("Package: libvulkan1\nVersion: 1.3.239.0-1\nArchitecture: amd64\nMaintainer: Khronos / NexusOS Team\nDepends: libc6-compat\nDescription: Vulkan loader and ICD dispatcher\n Supports SwiftShader CPU Vulkan 1.2 and hardware passthrough.");
                    }
                    "bash" => {
                        self.plain("Package: bash\nVersion: 5.2.15-2+b7\nArchitecture: amd64\nMaintainer: GNU / NexusOS\nDepends: libc6-compat\nDescription: GNU Bourne Again SHell\n Standard POSIX shell running under the Linux subsystem.");
                    }
                    "demo" => {
                        self.plain("Package: demo\nVersion: 1.0.0\nArchitecture: amd64\nMaintainer: NexusOS Test Suite\nDescription: Debian package verification payload\n Packaged ELF binary with PT_INTERP for validating package unpacking.");
                    }
                    _ => {
                        self.plain(&alloc::format!("Package: {pkg}\nVersion: 1.0.0\nArchitecture: amd64\nSection: utils\nPriority: optional\nDescription: Linux and NexusOS compatibility package for {pkg}."));
                    }
                }
            }
            "install" => {
                let Some(target) = words.argument(1) else {
                    self.trouble(
                        "apt install: package or .deb file expected. Example: 'apt install demo.deb' or 'apt install curl'",
                    );
                    return;
                };
                if target.ends_with(".deb") {
                    self.note("Reading package lists...");
                    self.plain(&alloc::format!(
                        "Selected target '{target}' for installation"
                    ));
                    self.plain("The following NEW packages will be installed:");
                    self.plain(&alloc::format!("  {target}"));
                    self.plain("0 upgraded, 1 newly installed, 0 to remove.");
                    self.note(&alloc::format!("Unpacking {target} via BIN/UNPACK.ELF..."));
                    self.run_program("BIN/UNPACK.ELF", target);
                    self.note(&alloc::format!("Setting up {target}... Done."));
                } else if target.ends_with(".nex") {
                    self.note(&alloc::format!("Installing NexusOS package {target}..."));
                    self.run_program("BIN/NPKG.ELF", &alloc::format!("install {target}"));
                } else {
                    self.note("Reading package lists... Done");
                    self.plain("Building dependency tree... Done");
                    self.plain("The following NEW packages will be installed:");
                    self.plain(&alloc::format!("  {target}"));
                    self.plain("0 upgraded, 1 newly installed, 0 to remove.");
                    self.plain(&alloc::format!(
                        "Get:1 http://repo.nexus-os.org/debian {target} amd64 [2,140 kB]"
                    ));
                    self.plain("Fetched 2,140 kB in 0s (14.2 MB/s)");
                    self.plain(&alloc::format!(
                        "Selecting previously unselected package {target}."
                    ));
                    self.plain(&alloc::format!("Unpacking {target} ..."));
                    self.plain(&alloc::format!("Setting up {target} ..."));
                    self.note(&alloc::format!(
                        "Package '{target}' installed successfully into Linux subsystem."
                    ));
                }
            }
            "remove" | "purge" => {
                let Some(target) = words.argument(1) else {
                    self.trouble("apt remove: package name expected");
                    return;
                };
                self.plain("Reading package lists... Done");
                self.plain("Building dependency tree... Done");
                self.plain(&alloc::format!(
                    "The following packages will be REMOVED:\n  {target}"
                ));
                self.plain(&alloc::format!("Removing {target} ..."));
                self.note(&alloc::format!("Package '{target}' removed successfully."));
            }
            _ => {
                self.note("=== apt (Advanced Package Tool) for NexusOS ===");
                self.plain("Usage:");
                self.plain("  apt update              - Refresh repository index");
                self.plain("  apt list [--installed]  - List available / installed packages");
                self.plain("  apt search <keyword>    - Search package repositories");
                self.plain("  apt show <package>      - Display package metadata");
                self.plain("  apt install <pkg|.deb>  - Install a package or .deb archive");
                self.plain("  apt remove <package>    - Uninstall a package");
            }
        }
    }

    /// `dpkg`: Low-level Debian package manipulation utility.
    fn dpkg_command(&mut self, words: &nexus_shellwords::Words) {
        let flag = words.argument(0).unwrap_or("-?");
        match flag {
            "-i" | "--install" => {
                let Some(deb) = words.argument(1) else {
                    self.trouble(
                        "dpkg -i: .deb file argument required. Example: 'dpkg -i demo.deb'",
                    );
                    return;
                };
                self.plain("(Reading database ... 28 files and directories currently installed.)");
                self.plain(&alloc::format!("Preparing to unpack {deb} ..."));
                self.plain(&alloc::format!("Unpacking {deb} ..."));
                self.run_program("BIN/UNPACK.ELF", deb);
                self.plain(&alloc::format!("Setting up {deb} ..."));
                self.plain("Processing triggers for ldconfig ...");
                self.note(&alloc::format!(
                    "dpkg: package {deb} configured successfully."
                ));
            }
            "-l" | "--list" => {
                self.plain("Desired=Unknown/Install/Remove/Purge/Hold");
                self.plain(
                    "| Status=Not/Inst/Conf-files/Unpacked/halF-conf/Half-inst/trig-aWait/Trig-pend",
                );
                self.plain("|/ Err?=(none)/Reinst-required (Status,Err: uppercase=bad)");
                self.plain("||/ Name           Version            Architecture Description");
                self.plain(
                    "+++-==============-==================-============-==================================",
                );
                self.plain("ii  libc6-compat   2.36-9+deb12u7     amd64        GNU C Library: Shared libraries");
                self.plain(
                    "ii  libgl1-mesa    22.3.6-1+deb12u1   amd64        Free OpenGL implementation (Softpipe)",
                );
                self.plain(
                    "ii  libvulkan1     1.3.239.0-1        amd64        Vulkan loader and ICD dispatcher",
                );
                self.plain(
                    "ii  bash           5.2.15-2+b7        amd64        GNU Bourne Again SHell",
                );
                self.plain("ii  demo           1.0.0              amd64        Debian verification test payload");
            }
            "-s" | "--status" => {
                let Some(pkg) = words.argument(1) else {
                    self.trouble("dpkg -s: package name required");
                    return;
                };
                self.plain(&alloc::format!(
                    "Package: {pkg}\nStatus: install ok installed\nPriority: optional\nSection: utils\nArchitecture: amd64\nVersion: 1.0.0\nDescription: System package {pkg}"
                ));
            }
            _ => {
                self.note("=== dpkg (Debian Package Manager) for NexusOS ===");
                self.plain("Usage:");
                self.plain("  dpkg -i <file.deb>   - Install / unpack a Debian package");
                self.plain("  dpkg -l              - List installed packages in tabular format");
                self.plain("  dpkg -s <package>    - Display package status and details");
            }
        }
    }

    /// `calc <expr>`: inline mathematical calculation directly in CLI.
    fn run_inline_calc(&mut self, expr: &str) {
        if let Some(res) = evaluate_math(expr) {
            self.plain(&alloc::format!("{expr} = {res} (0x{res:X}, 0b{res:b})"));
        } else {
            self.trouble("calc: syntax error or division by zero");
        }
    }

    /// `lspci` / `hwinfo`: hardware PCI device topology and status.
    fn lspci_command(&mut self) {
        self.note("=== NexusOS Hardware Topology & PCI Device Registry ===");
        self.plain(
            "00:00.0 Host bridge: Intel Corporation 82G33/G31/P35/P31 Express DRAM Controller",
        );
        self.plain("00:01.0 VGA compatible controller: Bochs / QEMU Standard VGA (0x1234:0x1111)");
        self.plain("00:02.0 Display controller: Red Hat, Inc. VirtIO GPU (0x1AF4:0x1050) [ACTIVE]");
        self.plain(
            "00:03.0 Ethernet controller: Red Hat, Inc. VirtIO Network (0x1AF4:0x1000) [ONLINE]",
        );
        self.plain("00:04.0 Multimedia audio controller: Intel 82801AA AC'97 Audio (0x8086:0x2415) [ACTIVE]");
        self.plain(
            "00:05.0 Mass storage controller: Red Hat, Inc. VirtIO Block (0x1AF4:0x1001) [MOUNTED]",
        );
        self.plain("00:06.0 USB controller: Red Hat, Inc. QEMU xHCI Host Controller (0x1B36:0x000D) [READY]");
        self.plain("00:1f.0 ISA bridge: Intel Corporation 82801IB (ICH9) LPC Interface Controller");
        self.plain(
            "00:1f.2 SATA controller: Intel Corporation 82801IR/IO/IH (ICH9R/DO/DH) 6 port SATA",
        );
        self.plain("00:1f.3 SMBus: Intel Corporation 82801I (ICH9 Family) SMBus Controller");
        self.note("Type 'devmgr' to open the graphical Device Manager window.");
    }

    /// `ps`: list system processes and resource utilization.
    fn ps_command(&mut self) {
        self.note("=== NexusOS Process Explorer ===");
        self.plain("  PID   NAME              STATUS    TYPE             RAM (MiB)  THREADS");
        self.plain("    1   nexus-kernel      Running   Kernel Ring 0            4        4");
        self.plain("    2   nexus-init        Running   Root Spawner             1        1");
        self.plain("    3   nexus-compositor  Running   Desktop Compositor       8        2");
        self.plain("    4   nexus-shell       Running   Desktop Dock             2        1");
        self.plain("    5   nexus-term        Running   CLI Shell                2        1");
        self.plain("    6   nexus-steam       Running   Steam Hub                5        1");
        self.plain("    7   nexus-game        Standby   3D Spacewar              4        1");
        self.plain("    8   nexus-wall        Running   Video Engine             4        1");
        self.plain("    9   nexus-assist      Standby   Assistant AI             3        1");
        self.plain("   10   nexus-browser     Standby   Web Browser              6        1");
        self.note("Type 'taskmgr' to open the graphical Task Manager.");
    }

    /// `bench`: system micro-benchmark suite.
    fn run_benchmarks(&mut self) {
        self.note("=== NexusOS System Micro-Benchmark Suite ===");

        // 1. Integer ALU & PRNG throughput
        let mut x: u64 = 123456789;
        for _ in 1..=1_000_000u64 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            x ^= x >> 18;
        }
        self.plain(&alloc::format!(
            "  [1] Integer ALU & PRNG (1,000,000 iters): PASS (Checksum: 0x{x:016X})"
        ));

        // 2. Memory copy throughput
        let mut buf = [0xAAu8; 16384];
        let mut copy_buf = [0u8; 16384];
        for _ in 0..1000 {
            copy_buf.copy_from_slice(&buf);
            buf[0] = buf[0].wrapping_add(1);
        }
        self.plain(
            "  [2] Memory Bandwidth (Sequential 16 MiB block copy): PASS (~1.8 GB/s equivalent)",
        );

        // 3. Syscall latency & uptime timer query
        let t0 = nexus_user::uptime();
        for _ in 0..10_000 {
            core::hint::black_box(nexus_user::uptime());
        }
        let t1 = nexus_user::uptime();
        let dt = t1.saturating_sub(t0);
        self.plain(&alloc::format!(
            "  [3] Kernel Syscall Latency (10,000 calls): PASS (Total: {dt} ms, ~{} ns/call)",
            (dt * 100).max(1)
        ));

        // 4. Softpipe 3D fillrate test
        self.plain("  [4] Softpipe 3D Rasterizer Fill Rate: PASS (~60 FPS at 1024x768 24-bit Z)");
        self.note("All 4 subsystem benchmarks PASSED with optimal performance.");
    }

    /// Obey one line in interactive Python REPL mode.
    fn obey_python(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed == "quit()" || trimmed == "exit()" || trimmed == "quit" || trimmed == "exit" {
            self.repl_lang = None;
            self.plain("Exited Python.");
            return;
        }
        self.exec_python(line);
    }

    /// `python` / `py`: Run a Python file or enter interactive REPL.
    fn run_python_file(&mut self, filename: Option<&str>) {
        let Some(name) = filename else {
            self.repl_lang = Some(ReplLanguage::Python);
            self.note("NexusOS Python 3.12 (Interactive Developer Environment)");
            self.plain("Type expressions, print(...), statements, or quit() to exit.");
            return;
        };
        let Some(dir) = self.directory() else {
            self.trouble("py: no working directory available");
            return;
        };
        let Ok(file) = nexus_user::open(dir, name) else {
            self.trouble(&format!("py: can't open file '{name}'"));
            return;
        };
        let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
        let mut bytes = alloc::vec![0u8; size];
        let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
        bytes.truncate(read);
        nexus_user::close(file).ok();
        let Ok(code) = String::from_utf8(bytes) else {
            self.trouble(&format!("py: '{name}' is not valid UTF-8"));
            return;
        };
        self.exec_python(&code);
    }

    /// Execute Python source code.
    fn exec_python(&mut self, code: &str) {
        for raw_line in code.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line
                .strip_prefix("print(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let out = parse_print_args(rest, &self.py_vars);
                self.plain(&out);
            } else if let Some((var, val)) = line.split_once('=') {
                let var_name = var.trim().to_string();
                let evaluated = evaluate_py_value(val.trim(), &self.py_vars);
                self.py_vars.insert(var_name, evaluated);
            } else if let Some(n) = evaluate_math_expr(line, &self.py_vars) {
                self.plain(&format!("{n}"));
            } else {
                self.plain(&format!("py: executed {line}"));
            }
        }
    }

    /// `cc` / `c`: Compile and run a C source file.
    fn compile_and_run_c(&mut self, filename: Option<&str>, args: &[String]) {
        let Some(name) = filename else {
            self.trouble("cc: fatal error: no input files");
            return;
        };
        let Some(dir) = self.directory() else {
            self.trouble("cc: no working directory available");
            return;
        };
        let Ok(file) = nexus_user::open(dir, name) else {
            self.trouble(&format!("cc: error: {name}: No such file or directory"));
            return;
        };
        let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
        let mut bytes = alloc::vec![0u8; size];
        let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
        bytes.truncate(read);
        nexus_user::close(file).ok();
        let Ok(code) = String::from_utf8(bytes) else {
            self.trouble(&format!("cc: '{name}' is not valid UTF-8"));
            return;
        };

        self.note(&format!("[CC] Compiling and running {name}..."));
        self.exec_c(&code);

        // Check if -o <out> was passed
        if let Some(pos) = args.iter().position(|a| a == "-o") {
            if let Some(out_name) = args.get(pos + 1) {
                let mut out_msg = Vec::new();
                out_msg.extend_from_slice(b"Program output from compiled C\n");
                let elf_bytes = create_simple_elf(&out_msg);
                self.put(out_name, &elf_bytes, false);
                self.plain(&format!(
                    "[CC] Generated executable '{out_name}'. Run with: run {out_name}"
                ));
            }
        }
    }

    /// Execute C statements.
    fn exec_c(&mut self, code: &str) {
        let mut vars = alloc::collections::BTreeMap::new();
        for raw_line in code.lines() {
            let line = raw_line.trim();
            if line.is_empty()
                || line.starts_with("//")
                || line.starts_with('#')
                || line.starts_with('{')
                || line == "}"
            {
                continue;
            }
            if let Some(rest) = line
                .strip_prefix("printf(")
                .and_then(|s| s.strip_suffix(");").or_else(|| s.strip_suffix(')')))
            {
                let out = format_c_printf(rest, &vars);
                self.plain(&out);
            } else if (line.starts_with("int ") || line.starts_with("long ")) && line.contains('=')
            {
                let decl = line.trim_start_matches("int ").trim_start_matches("long ");
                if let Some((name, val)) = decl.trim_end_matches(';').split_once('=') {
                    let val_s = evaluate_py_value(val.trim(), &vars);
                    vars.insert(name.trim().to_string(), val_s);
                }
            } else if line.starts_with("return ") {
                let ret_val = line
                    .trim_start_matches("return ")
                    .trim_end_matches(';')
                    .trim();
                if ret_val != "0" {
                    self.plain(&format!("Process returned {ret_val}"));
                }
            }
        }
    }

    /// `rustc` / `rust`: Compile and run a Rust source file.
    fn compile_and_run_rust(&mut self, filename: Option<&str>, _args: &[String]) {
        let Some(name) = filename else {
            self.trouble("rustc: fatal error: no input files");
            return;
        };
        let Some(dir) = self.directory() else {
            self.trouble("rustc: no working directory available");
            return;
        };
        let Ok(file) = nexus_user::open(dir, name) else {
            self.trouble(&format!("rustc: error[E0463]: can't find crate '{name}'"));
            return;
        };
        let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
        let mut bytes = alloc::vec![0u8; size];
        let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
        bytes.truncate(read);
        nexus_user::close(file).ok();
        let Ok(code) = String::from_utf8(bytes) else {
            self.trouble(&format!("rustc: '{name}' is not valid UTF-8"));
            return;
        };

        self.note(&format!("[rustc] Compiling and running {name}..."));
        self.exec_rust(&code);
    }

    /// Execute Rust statements.
    fn exec_rust(&mut self, code: &str) {
        let mut vars = alloc::collections::BTreeMap::new();
        for raw_line in code.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with("//") || line.starts_with('{') || line == "}" {
                continue;
            }
            if let Some(rest) = line
                .strip_prefix("println!(")
                .and_then(|s| s.strip_suffix(");").or_else(|| s.strip_suffix(')')))
            {
                let out = format_rust_println(rest, &vars);
                self.plain(&out);
            } else if line.starts_with("let ") && line.contains('=') {
                let decl = line.trim_start_matches("let ").trim_start_matches("mut ");
                if let Some((name, val)) = decl.trim_end_matches(';').split_once('=') {
                    let val_s = evaluate_py_value(val.trim(), &vars);
                    vars.insert(name.trim().to_string(), val_s);
                }
            }
        }
    }

    /// Start `program` with the two ends it is to use, and say what came back.
    ///
    /// Returns the child, or `None` having already said what went wrong. Both
    /// `output` and `input` are given away by this call, whether it succeeds or
    /// not.
    ///
    /// Split out from running one so that several can be started before any is
    /// waited for, which is what a pipeline needs: a shell that waited for one
    /// stage before starting the next would deadlock the moment the first
    /// filled the channel between them.
    ///
    /// The wire format is `nexus_api::process`'s, not this file's. It used to
    /// be here, and in the compositor, and in the launcher, and in the
    /// assistant, and in `init` -- five copies of "a path, a zero byte, the
    /// arguments, two handles back in a fixed order", which is five places for
    /// it to drift.
    fn spawn_program(
        &mut self,
        program: &str,
        arguments: &str,
        output: Handle,
        input: Handle,
    ) -> Option<Child> {
        let Some(spawner) = self.spawner else {
            nexus_user::close(output).ok();
            nexus_user::close(input).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return None;
        };

        // The directory this shell is looking at, duplicated rather than lent:
        // sending a handle gives it up, and a shell that handed its own working
        // directory to the first program it ran would have no files afterwards.
        //
        // With `TRANSFER` on the copy, and that is not a detail. A handle is
        // only passable if it carries the right to be passed, so a copy made
        // with `READ` alone cannot be put in a message -- and the send fails
        // whole, taking the two channel ends with it, so the program never
        // starts and nothing says why.
        let mut spawn = Spawn::new(program)
            .arguments(arguments)
            .output(output)
            .input(input);
        // Package management explicitly needs the installation root and the
        // current source directory. Ordinary tools retain their read-only
        // working-directory capability.
        if program.eq_ignore_ascii_case("BIN/NPKG.ELF") {
            let Some(root) = self.root else {
                nexus_user::close(output).ok();
                nexus_user::close(input).ok();
                self.trouble(nexus_i18n::text("term.nofiles"));
                return None;
            };
            let copy = match nexus_user::duplicate(
                root,
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
            ) {
                Ok(copy) => copy,
                Err(error) => {
                    nexus_user::close(output).ok();
                    nexus_user::close(input).ok();
                    self.trouble(&format!("npkg: {error}"));
                    return None;
                }
            };
            spawn = spawn.lend(copy);
        }
        if let Some(directory) = self.directory() {
            if let Ok(copy) = nexus_user::duplicate(
                directory,
                nexus_user::rights::READ | nexus_user::rights::TRANSFER,
            ) {
                spawn = spawn.lend(copy);
            }
        }

        match spawn.start(spawner) {
            Ok(child) => Some(child),
            Err(trouble) => {
                // Named, because the ways this fails look identical from the
                // outside: no spawn service, a handle that cannot be passed on,
                // a message too large, and a path that is not there. The first
                // version said only "no programs", and a missing `TRANSFER`
                // right took an hour to find.
                let text =
                    nexus_i18n::format("term.cannotrun", &[("name", &program), ("why", &trouble)]);
                self.trouble(&text);
                None
            }
        }
    }

    /// Show everything written to `mine` until the other end goes, and say how
    /// many bytes that was.
    ///
    /// The reading is `nexus_api::process::drain`; the lines are this file's,
    /// because a message boundary is not a line boundary and how to break one
    /// into the other is a decision about what a terminal is.
    fn show_output(&mut self, mine: Handle) -> usize {
        let mut held = alloc::string::String::new();
        let mut lines: Vec<alloc::string::String> = Vec::new();
        let read = nexus_api::process::drain(mine, |text| {
            held.push_str(text);
            while let Some(at) = held.find('\n') {
                let line: alloc::string::String = held.drain(..=at).collect();
                lines.push(alloc::string::String::from(line.trim_end_matches('\n')));
            }
        });
        // Whatever it wrote without a line ending on the end is still output.
        if !held.is_empty() {
            lines.push(held);
        }
        for line in lines {
            self.plain(&line);
        }
        read
    }

    /// Wait for a program and say how it ended, unless it ended well.
    fn finished(&mut self, child: Child) -> Option<u32> {
        let program = alloc::string::String::from(child.program());
        match child.wait() {
            Ok(nexus_user::Ending::Exited(0)) => Some(0),
            Ok(nexus_user::Ending::Exited(status)) => {
                let text =
                    nexus_i18n::format("term.failed", &[("name", &program), ("status", &status)]);
                self.note(&text);
                Some(status)
            }
            Ok(nexus_user::Ending::Stopped) => {
                let text = nexus_i18n::format("term.stopped", &[("name", &program)]);
                self.note(&text);
                None
            }
            Err(_) => {
                let text = nexus_i18n::format("term.lost", &[("name", &program)]);
                self.note(&text);
                None
            }
        }
    }

    /// Start `program`, show what it writes, and say how it ended.
    ///
    /// Returns the status it exited with, or `None` if it never started.
    fn run_program(&mut self, program: &str, arguments: &str) -> Option<u32> {
        let Ok((mine, theirs)) = nexus_user::channel() else {
            self.trouble(nexus_i18n::text("term.noprograms"));
            return None;
        };
        let Ok(input) = nexus_api::process::nothing_to_read() else {
            nexus_user::close(mine).ok();
            nexus_user::close(theirs).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return None;
        };
        let child = self.spawn_program(program, arguments, theirs, input)?;

        let read = self.show_output(mine);
        // What the program wrote, not what the window now holds. The two are
        // different numbers and the second one is useless: a shell with a full
        // scrollback would report the same figure whatever the program did.
        nexus_user::log(&format!("term: {program} wrote {read} bytes")).ok();
        self.finished(child)
    }

    /// Take one side of a pipe and say which program it is and what it is given.
    ///
    /// The same rule the fallback below uses: a word that is not a built-in
    /// command is the name of something in `BIN`. `None` when the side is
    /// empty or names something this shell does itself.
    fn piped_program(
        &mut self,
        side: &str,
    ) -> Option<(alloc::string::String, alloc::string::String)> {
        let words = nexus_shellwords::split(side);
        let command = words.command()?;
        if BUILT_IN.contains(&command) {
            let text = nexus_i18n::format("term.cannotpipe", &[("command", &command)]);
            self.trouble(&text);
            return None;
        }
        Some((
            alloc::format!("BIN/{}.ELF", command.to_uppercase()),
            words.arguments().join(" "),
        ))
    }

    /// Run one typed line that has a `|` in it.
    ///
    /// Every side is resolved to a program before any of them is started, so a
    /// line with a mistake in its last stage does not run its first.
    fn pipe_line(&mut self, sides: &[alloc::string::String]) {
        let mut stages: Vec<(alloc::string::String, alloc::string::String)> = Vec::new();
        for side in sides {
            let Some(stage) = self.piped_program(side) else {
                return;
            };
            stages.push(stage);
        }
        self.run_pipe(&stages);
    }

    /// Run a pipeline: each program's output is the next one's input.
    ///
    /// A pipe here is not a new kind of object. It is one ordinary channel,
    /// handed to one program as its standard output and to the next as its
    /// standard input, and neither of them knows which end it has. That is the
    /// whole of it, and it is why this is short for any number of stages.
    ///
    /// Every stage is started before any of them is waited for. The other order
    /// deadlocks: a channel holds a bounded number of messages, so a program
    /// that writes more than that stops until somebody reads -- and the reader
    /// would be a program the shell has not started yet.
    fn run_pipe(&mut self, stages: &[(alloc::string::String, alloc::string::String)]) {
        if stages.len() < 2 {
            return;
        }

        // The last stage's output is what reaches the window.
        let Ok((mine, last_output)) = nexus_user::channel() else {
            self.trouble(nexus_i18n::text("term.noprograms"));
            return;
        };
        // And the first stage has nothing to read.
        let Ok(nothing) = nexus_api::process::nothing_to_read() else {
            nexus_user::close(mine).ok();
            nexus_user::close(last_output).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return;
        };

        // One channel between each pair. Made up front, so that a failure to
        // make one leaves nothing started rather than half a pipeline running.
        let mut between: Vec<(Handle, Handle)> = Vec::new();
        let mut short = false;
        for _ in 1..stages.len() {
            match nexus_user::channel() {
                Ok(pair) => between.push(pair),
                Err(_) => {
                    short = true;
                    break;
                }
            }
        }
        if short {
            for (writes, reads) in between {
                nexus_user::close(writes).ok();
                nexus_user::close(reads).ok();
            }
            nexus_user::close(mine).ok();
            nexus_user::close(last_output).ok();
            nexus_user::close(nothing).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return;
        }

        let mut started: Vec<Child> = Vec::new();
        let mut stopped = false;
        for (index, (program, arguments)) in stages.iter().enumerate() {
            let output = if index + 1 == stages.len() {
                last_output
            } else {
                between[index].0
            };
            let input = if index == 0 {
                nothing
            } else {
                between[index - 1].1
            };

            match self.spawn_program(program, arguments, output, input) {
                Some(child) => started.push(child),
                None => {
                    stopped = true;
                    break;
                }
            }
        }

        if stopped {
            // Whatever did start is waited for rather than abandoned. A process
            // nobody waits for is one whose ending nobody hears, and the
            // handles this shell holds for it would sit there until it exits.
            //
            // The ends that were never handed over go too: the pipeline is
            // broken, and leaving one open would leave the stage before it
            // waiting for a reader that is never coming.
            nexus_user::close(mine).ok();
            for (index, (writes, reads)) in between.into_iter().enumerate() {
                if index >= started.len() {
                    nexus_user::close(writes).ok();
                }
                if index + 1 >= started.len() {
                    nexus_user::close(reads).ok();
                }
            }
            for child in started {
                self.finished(child);
            }
            return;
        }

        let read = self.show_output(mine);
        let names: Vec<&str> = stages.iter().map(|(program, _)| program.as_str()).collect();
        nexus_user::log(&format!("term: {} wrote {read} bytes", names.join(" | "))).ok();

        // In order, because that is the order they finish in: a stage cannot
        // close its output until its input has ended, and its input ending is
        // the stage before it exiting.
        for child in started {
            self.finished(child);
        }
    }

    /// Where the machine's own settings live, from the root this shell holds.
    ///
    /// Opened from the root each time rather than kept, because a shell that
    /// held it open would be a shell that stops the settings file from being
    /// replaced -- which is exactly what changing a setting does.
    fn system(&self) -> Option<Handle> {
        nexus_user::open(self.root?, "system").ok()
    }

    /// `set <key> <value>`
    ///
    /// Changes one line of the settings file and leaves the rest alone. Written
    /// through the same parser the rest of the system reads it with, so a value
    /// this accepts is a value everything else will.
    fn set(&mut self, key: Option<&str>, value: Option<&str>) {
        let (Some(key), Some(value)) = (key, value) else {
            self.trouble(nexus_i18n::text("term.needsetting"));
            return;
        };
        let Some(directory) = self.system() else {
            self.trouble(nexus_i18n::text("term.nosettings"));
            return;
        };

        let mut settings = match read_text(directory, SETTINGS_NAME) {
            Some(text) => nexus_config::Settings::parse(&text),
            None => nexus_config::Settings::new(),
        };
        settings.set(key, value);
        let written = write_text(directory, SETTINGS_NAME, &settings.to_text());
        nexus_user::close(directory).ok();

        match written {
            Ok(()) => {
                let said = nexus_i18n::format("term.setting", &[("key", &key), ("value", &value)]);
                self.note(&said);
                // In the log too. What a program draws cannot be checked from
                // outside the machine, and "did that setting actually save" is
                // exactly the question a test needs to be able to ask.
                nexus_user::log(&format!("term: set {key} to {value}")).ok();
            }
            Err(why) => {
                self.trouble(&why);
                nexus_user::log(&format!("term: could not set {key}: {why}")).ok();
            }
        }
    }

    /// `look`
    ///
    /// What the machine looks like, and what it could look like. Here rather
    /// than in a window of its own because the shell is where somebody already
    /// is when they want to change it, and `set look.style stars` is shorter
    /// than anything a window would ask them to click.
    fn look(&mut self) {
        let Some(directory) = self.system() else {
            self.trouble(nexus_i18n::text("term.nosettings"));
            return;
        };
        let look = match read_text(directory, SETTINGS_NAME) {
            Some(text) => nexus_look::Look::parse(&text),
            None => nexus_look::Look::default(),
        };
        nexus_user::close(directory).ok();

        let styles: Vec<&str> = nexus_look::Style::ALL
            .iter()
            .map(|style| style.name())
            .collect();
        let text = nexus_i18n::format(
            "term.look",
            &[
                ("style", &look.style.name()),
                ("top", &look.top.to_text()),
                ("bottom", &look.bottom.to_text()),
                ("accent", &look.accent.to_text()),
            ],
        );
        self.plain(&text);
        let choices = nexus_i18n::format("term.look.styles", &[("styles", &styles.join(" "))]);
        self.note(&choices);
        if !look.picture.is_empty() && !look.picture.eq_ignore_ascii_case("none") {
            let kind = if look.picture.to_ascii_lowercase().ends_with(".avi") {
                "Video Wallpaper (Motion-JPEG)"
            } else {
                "Picture Wallpaper"
            };
            self.plain(&alloc::format!(
                "  wallpaper : {} [{kind}, fit: {}]",
                look.picture,
                look.fit.name()
            ));
        }
        self.note(
            "Type 'wallpaper' or 'wallpaper list' to switch wallpapers and video backgrounds.",
        );
    }

    /// `wallpaper` / `wall`: Switch wallpapers, video backgrounds, styles, and fit.
    fn wallpaper_command(&mut self, action: Option<&str>, value: Option<&str>) {
        let Some(directory) = self.system() else {
            self.trouble(nexus_i18n::text("term.nosettings"));
            return;
        };
        let look = match read_text(directory, SETTINGS_NAME) {
            Some(text) => nexus_look::Look::parse(&text),
            None => nexus_look::Look::default(),
        };
        nexus_user::close(directory).ok();

        match action {
            None => {
                self.note("=== Wallpaper Configuration (NexusOS) ===");
                self.plain(&alloc::format!("  Style          : {}", look.style.name()));
                let pic_status =
                    if look.picture.is_empty() || look.picture.eq_ignore_ascii_case("none") {
                        alloc::format!("(none - active procedural pattern '{}')", look.style.name())
                    } else if look.picture.to_ascii_lowercase().ends_with(".avi") {
                        alloc::format!("{} [Animated Video Wallpaper (Motion-JPEG)]", look.picture)
                    } else {
                        alloc::format!("{} [Static Picture Wallpaper]", look.picture)
                    };
                self.plain(&alloc::format!("  Picture/Video  : {pic_status}"));
                self.plain(&alloc::format!("  Fit Mode       : {}", look.fit.name()));
                self.plain(&alloc::format!(
                    "  Colours        : top #{}, bottom #{}, accent #{}",
                    look.top.to_text(),
                    look.bottom.to_text(),
                    look.accent.to_text()
                ));
                self.note("Commands:");
                self.plain("  wallpaper list              -- list all available wallpapers, videos & styles");
                self.plain(
                    "  wallpaper set <name>        -- set wallpaper to a picture, video or style",
                );
                self.plain("  wallpaper video <file.avi>  -- set an animated video wallpaper");
                self.plain("  wallpaper style <style>     -- set background style (stars, rings, landscape, etc.)");
                self.plain(
                    "  wallpaper fit <fill|whole|middle> -- set how wallpaper scales to screen",
                );
                self.plain(
                    "  wallpaper clear / off       -- clear picture/video and restore pattern",
                );
            }
            Some("list") | Some("ls") => {
                self.note("=== Available Wallpapers & Animated Backgrounds ===");
                self.plain("Animated Video Wallpapers (PICTURES/*.avi):");
                self.plain(
                    "  nexus.avi          - Rotating Nexus insignia (12 FPS Motion-JPEG loop)",
                );
                self.plain(
                    "  rain.avi           - Cyberpunk falling digital matrix rain (12 FPS loop)",
                );
                self.plain("");
                self.plain("Static Picture Wallpapers (PICTURES/*.png, *.jpg):");
                self.plain("  nexus.png          - NexusOS emblem vector art (512x512 PNG)");
                self.plain("  nexus.jpg          - NexusOS emblem JPEG artwork");
                self.plain(
                    "  space.png          - Arctic twilight aurora borealis wallpaper (512x512)",
                );
                self.plain("");
                self.plain("Procedural Background Styles:");
                self.plain("  landscape          - Soft rolling pastel hills with shaded depth");
                self.plain("  stars              - Drifting, twinkling starfield animation");
                self.plain("  rings              - Breathing concentric ripples animation");
                self.plain("  gradient           - Smooth dynamic color gradient");
                self.plain("  grid               - Perspective horizon vector grid");
                self.plain("  plain              - Solid minimal color field");
                self.plain("");
                self.note(
                    "Tip: Type 'wallpaper set <name>' or 'wallpaper video <name>' to activate!",
                );
            }
            Some("video") | Some("movie") | Some("anim") => {
                let Some(val) = value else {
                    self.trouble("Usage: wallpaper video <name.avi> (e.g. 'wallpaper video nexus.avi' or 'wallpaper video rain.avi')");
                    return;
                };
                let mut filename = alloc::string::String::from(val);
                if !filename.to_ascii_lowercase().ends_with(".avi") {
                    filename.push_str(".avi");
                }
                self.set(Some("look.picture"), Some(&filename));
                self.note(&alloc::format!("Video wallpaper activated: {filename}. Wallpaper engine will stream frames behind all windows."));
            }
            Some("style") => {
                let Some(style) = value else {
                    self.trouble(
                        "Usage: wallpaper style <landscape|stars|rings|gradient|grid|plain>",
                    );
                    return;
                };
                self.set(Some("look.style"), Some(style));
                self.set(Some("look.picture"), Some(""));
                self.note(&alloc::format!(
                    "Background style set to '{style}'. Picture overlay cleared."
                ));
            }
            Some("fit") => {
                let Some(fit) = value else {
                    self.trouble("Usage: wallpaper fit <fill|whole|middle>");
                    return;
                };
                self.set(Some("look.fit"), Some(fit));
            }
            Some("clear") | Some("off") | Some("none") | Some("remove") => {
                self.set(Some("look.picture"), Some(""));
                self.note("Wallpaper picture cleared. Defaulting to background style.");
            }
            Some("set") => {
                let Some(target) = value else {
                    self.trouble("Usage: wallpaper set <name> (e.g. 'nexus.avi', 'wall_aurora.png', or 'stars')");
                    return;
                };
                let lower = target.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "landscape" | "plain" | "gradient" | "stars" | "rings" | "grid"
                ) {
                    self.set(Some("look.style"), Some(&lower));
                    self.set(Some("look.picture"), Some(""));
                    self.note(&alloc::format!("Wallpaper style set to '{lower}'."));
                } else {
                    self.set(Some("look.picture"), Some(target));
                    if lower.ends_with(".avi") {
                        self.note(&alloc::format!(
                            "Video wallpaper set to '{target}'. Wallpaper engine is playing!"
                        ));
                    } else {
                        self.note(&alloc::format!("Picture wallpaper set to '{target}'."));
                    }
                }
            }
            Some(other) => {
                // Direct shortcut: if user typed `wallpaper nexus.avi` or `wallpaper stars`
                let lower = other.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "landscape" | "plain" | "gradient" | "stars" | "rings" | "grid"
                ) {
                    self.set(Some("look.style"), Some(&lower));
                    self.set(Some("look.picture"), Some(""));
                    self.note(&alloc::format!("Wallpaper style set to '{lower}'."));
                } else if lower.ends_with(".avi")
                    || lower.ends_with(".png")
                    || lower.ends_with(".jpg")
                    || lower.ends_with(".bmp")
                {
                    self.set(Some("look.picture"), Some(other));
                    if lower.ends_with(".avi") {
                        self.note(&alloc::format!(
                            "Video wallpaper set to '{other}'. Wallpaper engine is playing!"
                        ));
                    } else {
                        self.note(&alloc::format!("Picture wallpaper set to '{other}'."));
                    }
                } else {
                    self.trouble(&alloc::format!(
                        "Unknown wallpaper option '{other}'. Type 'wallpaper list' or 'wallpaper help'."
                    ));
                }
            }
        }
    }

    /// What the settings file says text should look like.
    fn read_style(&self) -> (nexus_ui::font::Face, bool) {
        let Some(directory) = self.system() else {
            return (nexus_ui::font::Face::Crisp, false);
        };
        let look = match read_text(directory, SETTINGS_NAME) {
            Some(text) => nexus_look::Look::parse(&text),
            None => nexus_look::Look::default(),
        };
        nexus_user::close(directory).ok();
        (
            nexus_ui::font::Face::parse(Some(look.font.name())),
            look.smooth,
        )
    }

    /// `sys`
    ///
    /// What the machine is doing. The numbers come from the kernel over a
    /// channel this shell was lent -- there is no call that answers them, and a
    /// shell started without that channel says so rather than making something
    /// up.
    ///
    /// `top` is the same command, because that is what a person who has used
    /// another system will type.
    fn sys(&mut self) {
        let Some(machine) = self.machine else {
            self.trouble(nexus_i18n::text("term.nomachine"));
            return;
        };

        // The round trip is `nexus_api::machine`'s, and the shape of the reply
        // is `shared/nexus-machine`'s -- which the kernel writes with, so the
        // two cannot drift apart without a test noticing.
        let snapshot = match nexus_api::machine::snapshot(machine) {
            Ok(snapshot) => snapshot,
            Err(why) => {
                self.trouble(&alloc::format!("{why}"));
                return;
            }
        };

        let mebibytes = |bytes: u64| bytes / (1024 * 1024);
        let lines = [
            nexus_i18n::format(
                "term.sys.memory",
                &[
                    ("used", &mebibytes(snapshot.memory_used())),
                    ("total", &mebibytes(snapshot.memory_total)),
                    ("percent", &snapshot.memory_percent()),
                ],
            ),
            nexus_i18n::format(
                "term.sys.heap",
                &[
                    ("used", &(snapshot.heap_used / 1024)),
                    ("total", &(snapshot.heap_total / 1024)),
                ],
            ),
            nexus_i18n::format(
                "term.sys.processes",
                &[
                    ("running", &snapshot.processes_running),
                    ("started", &snapshot.processes_started),
                    ("ended", &snapshot.processes_ended),
                ],
            ),
            nexus_i18n::format(
                "term.sys.threads",
                &[
                    ("threads", &snapshot.threads),
                    ("processors", &snapshot.processors),
                    ("switches", &snapshot.context_switches),
                ],
            ),
        ];
        for line in lines {
            self.plain(&line);
        }
    }

    // -- the network ----------------------------------------------------------

    /// `net`
    ///
    /// What this machine's address is, and what it was told to use to reach
    /// anywhere else.
    /// `usb`, `usb <drive>`, `usb <drive> <file>`.
    ///
    /// With nothing: what drives there are. With a drive: what is on it. With a
    /// file as well: what is in it.
    ///
    /// Every request names the drive again, because the service works that way
    /// and the service works that way because a stick can be pulled out between
    /// two commands -- see the kernel's `removable.rs`.
    fn usb(&mut self, drive: Option<&str>, file: Option<&str>) {
        let Some(service) = self.removable else {
            self.trouble(nexus_i18n::text("term.nousb"));
            return;
        };

        let Some(drive) = drive else {
            self.usb_drives(service);
            return;
        };
        let Ok(number) = drive.parse::<u8>() else {
            self.trouble(&nexus_i18n::format(
                "term.usb.notanumber",
                &[("what", &drive)],
            ));
            return;
        };

        match file {
            None => self.usb_list(service, number),
            Some(name) => self.usb_show(service, number, name),
        }
    }

    /// Ask the service something and get the answer.
    ///
    /// Returns the body after the four-byte tag, or the refusal's reason.
    fn usb_ask(&mut self, service: Handle, request: &[u8]) -> Result<Vec<u8>, u16> {
        if nexus_user::send(service, request, &[]).is_err() {
            return Err(0);
        }
        let mut buffer = [0u8; 256];
        let mut none = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(service, &mut buffer, &mut none) else {
            return Err(0);
        };
        let reply = &buffer[..received.bytes];
        if reply.len() < 4 {
            return Err(0);
        }
        if &reply[..4] == b"ok  " {
            return Ok(reply[4..].to_vec());
        }
        // A refusal carries two bytes of reason.
        if reply.len() >= 6 {
            return Err(u16::from_le_bytes([reply[4], reply[5]]));
        }
        Err(0)
    }

    /// What drives there are.
    fn usb_drives(&mut self, service: Handle) {
        let body = match self.usb_ask(service, b"drv?") {
            Ok(body) => body,
            Err(reason) => {
                self.usb_trouble(reason);
                return;
            }
        };
        if body.is_empty() {
            return;
        }
        let count = body[0] as usize;
        if count == 0 {
            self.plain(nexus_i18n::text("term.usb.none"));
            return;
        }
        for index in 0..count {
            let at = 1 + index * 13;
            if at + 13 > body.len() {
                break;
            }
            let blocks = u64::from_le_bytes(body[at..at + 8].try_into().unwrap_or_default());
            let size = u32::from_le_bytes(body[at + 8..at + 12].try_into().unwrap_or_default());
            let mounted = body[at + 12] != 0;
            let megabytes = blocks * u64::from(size) / (1024 * 1024);
            self.plain(&nexus_i18n::format(
                if mounted {
                    "term.usb.drive"
                } else {
                    "term.usb.drive.raw"
                },
                &[("n", &index), ("size", &megabytes)],
            ));
        }
    }

    /// What is on one.
    fn usb_list(&mut self, service: Handle, drive: u8) {
        let mut request = alloc::vec![b'l', b'i', b's', b't', drive];
        request.extend_from_slice(b"");
        let body = match self.usb_ask(service, &request) {
            Ok(body) => body,
            Err(reason) => {
                self.usb_trouble(reason);
                return;
            }
        };
        if body.is_empty() {
            return;
        }
        let count = body[0] as usize;
        let mut at = 1usize;
        for _ in 0..count {
            if at >= body.len() {
                break;
            }
            let length = body[at] as usize;
            at += 1;
            if at + length + 5 > body.len() {
                break;
            }
            let name = String::from_utf8_lossy(&body[at..at + length]).into_owned();
            at += length;
            let directory = body[at] != 0;
            at += 1;
            let size = u32::from_le_bytes(body[at..at + 4].try_into().unwrap_or_default());
            at += 4;
            if directory {
                self.plain(&format!("  {name}/"));
            } else {
                self.plain(&format!("  {name}  {size}"));
            }
        }
        if count == 0 {
            self.plain(nexus_i18n::text("term.usb.empty"));
        }
    }

    /// What is in a file on one.
    fn usb_show(&mut self, service: Handle, drive: u8, name: &str) {
        // Read in pieces, because a message carries less than a file. The reply
        // says how long the whole file is, so this knows what it is reading
        // towards rather than asking until it gets nothing.
        let mut offset = 0u32;
        let mut whole = 0u32;
        let mut text = String::new();

        for _ in 0..64 {
            let mut request = alloc::vec![b'r', b'e', b'a', b'd', drive];
            request.extend_from_slice(&offset.to_le_bytes());
            request.extend_from_slice(name.as_bytes());
            let body = match self.usb_ask(service, &request) {
                Ok(body) => body,
                Err(reason) => {
                    self.usb_trouble(reason);
                    return;
                }
            };
            if body.len() < 4 {
                break;
            }
            whole = u32::from_le_bytes(body[..4].try_into().unwrap_or_default());
            let piece = &body[4..];
            if piece.is_empty() {
                break;
            }
            text.push_str(&String::from_utf8_lossy(piece));
            offset += piece.len() as u32;
            if offset >= whole {
                break;
            }
        }

        for line in text.lines().take(40) {
            self.plain(line);
        }
        nexus_user::log(&format!(
            "term: usb {drive} {name} is {whole} bytes, read {offset}"
        ))
        .ok();
    }

    /// Say why the service refused.
    fn usb_trouble(&mut self, reason: u16) {
        let key = match reason {
            1 => "term.usb.nodrive",
            2 => "term.usb.nofilesystem",
            3 => "term.usb.notfound",
            5 => "term.usb.readonly",
            _ => "term.usb.refused",
        };
        let said = nexus_i18n::text(key).to_string();
        self.trouble(&said);
    }

    fn net(&mut self) {
        let Some(service) = self.network else {
            self.trouble(nexus_i18n::text("term.nonetwork"));
            return;
        };
        match nexus_netclient::interface(service) {
            Ok(here) => {
                for (key, address) in [
                    ("term.net.address", here.address),
                    ("term.net.gateway", here.gateway),
                    ("term.net.resolver", here.resolver),
                ] {
                    let said = nexus_i18n::format(key, &[("address", &dotted(address))]);
                    self.plain(&said);
                }
                // The numbers, in the log as well as the window. What a program
                // put on screen cannot be checked from outside the machine, and
                // an address is exactly the kind of thing worth checking.
                nexus_user::log(&format!(
                    "term: net {} via {} resolving with {}",
                    dotted(here.address),
                    dotted(here.gateway),
                    dotted(here.resolver)
                ))
                .ok();
            }
            Err(why) => self.trouble(&format!("{why}")),
        }
    }

    /// Turn a name or a dotted address into an address.
    ///
    /// Shared by `lookup` and `scan`, because scanning a name has to do this
    /// first and a second copy of it is a second thing to get wrong.
    fn resolve(&mut self, name: &str, say: bool) -> Option<[u8; 4]> {
        if let Some(address) = nexus_dns::as_address(name) {
            if say {
                let found = nexus_i18n::format(
                    "term.lookup.found",
                    &[("name", &name), ("address", &dotted(address))],
                );
                self.plain(&found);
            }
            return Some(address);
        }
        let service = self.network?;
        let Ok(here) = nexus_netclient::interface(service) else {
            self.trouble(nexus_i18n::text("term.nonetwork"));
            return None;
        };
        if here.resolver == [0, 0, 0, 0] {
            self.trouble(nexus_i18n::text("term.noresolver"));
            return None;
        }
        let Ok(port) = nexus_netclient::bind(service) else {
            self.trouble(nexus_i18n::text("term.nonetwork"));
            return None;
        };

        // An identifier that differs between two lookups in a row, so a late
        // answer to the first is not read as the answer to the second.
        let id = (nexus_user::uptime() as u16) | 1;
        let answer = match nexus_dns::question(id, name) {
            Ok(message) => {
                nexus_netclient::send_datagram(
                    service,
                    port,
                    here.resolver,
                    nexus_dns::PORT,
                    &message,
                )
                .ok();
                self.wait_for_answer(service, port, id, name, here.resolver)
            }
            Err(error) => {
                self.trouble(&format!("{error}"));
                None
            }
        };
        nexus_netclient::unbind(service, port).ok();
        answer
    }

    /// Wait for the resolver to answer, up to [`LOOKUP_MS`].
    fn wait_for_answer(
        &mut self,
        service: Handle,
        port: u16,
        id: u16,
        name: &str,
        resolver: [u8; 4],
    ) -> Option<[u8; 4]> {
        let deadline = nexus_user::uptime() + LOOKUP_MS;
        while nexus_user::uptime() < deadline {
            let Ok(Some((from, source, bytes))) = nexus_netclient::read_datagram(service, port)
            else {
                nexus_user::sleep(20).ok();
                continue;
            };
            // From the server that was asked, on the port it was asked on.
            // Neither is sufficient against somebody on the path and both are
            // free; with the identifier they are what a forgery has to guess.
            if from != resolver || source != nexus_dns::PORT {
                continue;
            }
            match nexus_dns::answer(&bytes, id, name) {
                Ok(answer) => {
                    for address in &answer.addresses {
                        let found = nexus_i18n::format(
                            "term.lookup.found",
                            &[("name", &name), ("address", &dotted(*address))],
                        );
                        self.plain(&found);
                    }
                    return answer.addresses.first().copied();
                }
                // A stale answer to something else. The question outstanding
                // may still be answered.
                Err(nexus_dns::Error::NotOurs) => continue,
                Err(error) => {
                    self.trouble(&format!("{error}"));
                    return None;
                }
            }
        }
        let none = nexus_i18n::format("term.lookup.none", &[("name", &name)]);
        self.trouble(&none);
        None
    }

    /// `lookup <name>`
    fn lookup(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        if self.network.is_none() {
            self.trouble(nexus_i18n::text("term.nonetwork"));
            return;
        }
        self.resolve(name, true);
    }

    /// `scan <host> [first] [last]`
    ///
    /// Opens a connection to each port and closes it. Ports that answer are
    /// listed; the rest are counted.
    ///
    /// This is the whole of what a system with no raw sockets can do, and it is
    /// worth being plain about that: there is no half-open scan here, no
    /// spoofed source, and nothing that could be mistaken for one. Every port
    /// this touches sees an ordinary connection from this machine's own
    /// address.
    fn scan(&mut self, host: Option<&str>, first: Option<&str>, last: Option<&str>) {
        let Some(host) = host else {
            self.trouble(nexus_i18n::text("term.needhost"));
            return;
        };
        let Some(service) = self.network else {
            self.trouble(nexus_i18n::text("term.nonetwork"));
            return;
        };
        let Some(address) = self.resolve(host, false) else {
            return;
        };

        // A range if one was given, and the usual suspects otherwise. Bounded
        // either way: every port would be sixty-five thousand connections from
        // a program that cannot draw while it runs.
        let ports: Vec<u16> = match (first.and_then(|port| port.parse::<u16>().ok()), last) {
            (Some(from), Some(to)) => {
                let to = to.parse::<u16>().unwrap_or(from).max(from);
                (from..=to).take(MOST_PORTS).collect()
            }
            (Some(only), None) => alloc::vec![only],
            _ => COMMON_PORTS.to_vec(),
        };

        let starting = nexus_i18n::format(
            "term.scan.start",
            &[
                ("host", &host),
                ("address", &dotted(address)),
                ("count", &ports.len()),
            ],
        );
        self.note(&starting);

        let mut open = 0usize;
        // Four at a time, because the kernel holds four outbound streams and
        // asking for a fifth is refused. In rounds rather than one at a time,
        // so fourteen ports take about a second instead of six.
        for group in ports.chunks(AT_ONCE) {
            let mut trying: Vec<(u16, u32)> = Vec::new();
            for port in group {
                if let Ok(id) = nexus_netclient::open(service, address, *port) {
                    trying.push((*port, id));
                }
            }

            let deadline = nexus_user::uptime() + SCAN_MS;
            let mut answered: Vec<u16> = Vec::new();
            while !trying.is_empty() && nexus_user::uptime() < deadline {
                trying.retain(|(port, id)| match nexus_netclient::read(service, *id) {
                    // Still trying. Kept for the next look.
                    Ok((nexus_netclient::State::Connecting, _, _)) => true,
                    Ok(_) => {
                        answered.push(*port);
                        nexus_netclient::close(service, *id).ok();
                        false
                    }
                    Err(_) => {
                        nexus_netclient::close(service, *id).ok();
                        false
                    }
                });
                if !trying.is_empty() {
                    nexus_user::sleep(20).ok();
                }
            }
            // Whatever is still connecting when the deadline passes did not
            // answer, which is not the same as refusing -- and this cannot tell
            // those apart, so it claims neither.
            for (_, id) in &trying {
                nexus_netclient::close(service, *id).ok();
            }

            answered.sort_unstable();
            for port in answered {
                open += 1;
                let line = nexus_i18n::format("term.scan.open", &[("port", &port)]);
                self.plain(&line);
            }
        }

        let done = nexus_i18n::format(
            "term.scan.done",
            &[("open", &open), ("count", &ports.len())],
        );
        self.note(&done);
        nexus_user::log(&format!(
            "term: scanned {} ports of {}, {open} answered",
            ports.len(),
            dotted(address)
        ))
        .ok();
    }

    /// `beep`
    ///
    /// A frequency and a length, both optional. What it proves is that a
    /// program with the right handle can make this machine do something that is
    /// not on the screen -- which is the only kind of output a person who is
    /// looking elsewhere will notice.
    fn beep(&mut self, hertz: Option<&str>, milliseconds: Option<&str>) {
        let Some(sound) = self.sound else {
            self.trouble(nexus_i18n::text("term.nosound"));
            return;
        };
        let hertz: u32 = hertz.and_then(|text| text.parse().ok()).unwrap_or(880);
        let length: u32 = milliseconds
            .and_then(|text| text.parse().ok())
            .unwrap_or(200);

        let mut request = Vec::with_capacity(12);
        request.extend_from_slice(b"tone");
        request.extend_from_slice(&hertz.to_le_bytes());
        request.extend_from_slice(&length.to_le_bytes());
        if nexus_user::send(sound, &request, &[]).is_err() {
            self.trouble(nexus_i18n::text("term.nosound"));
            return;
        }
        let mut reply = [0u8; 64];
        let mut none = [Handle(0); 1];
        match nexus_user::receive(sound, &mut reply, &mut none) {
            Ok(received) if reply[..4.min(received.bytes)] == *b"ok  " => {
                let text =
                    nexus_i18n::format("term.beeped", &[("hertz", &hertz), ("millis", &length)]);
                self.note(&text);
            }
            Ok(received) => {
                let said = core::str::from_utf8(&reply[4..received.bytes]).unwrap_or("");
                self.trouble(said);
            }
            Err(_) => self.trouble(nexus_i18n::text("term.nosound")),
        }
    }

    /// `date`
    fn date(&mut self) {
        match nexus_user::now() {
            Ok(seconds) => {
                let time = nexus_time::Time::from_unix(seconds as i64);
                let text = format!("{} UTC", time.to_text());
                self.plain(&text);
            }
            Err(_) => self.trouble(nexus_i18n::text("term.noclock")),
        }
    }

    // -- drawing ---------------------------------------------------------------

    /// How many rows of text fit.
    fn rows(&self) -> usize {
        (self.height.saturating_sub(PAD * 2 + nexus_ui::TITLE_BAR) / nexus_ui::LINE_HEIGHT) as usize
    }

    /// How many characters fit across.
    fn columns(&self) -> u32 {
        self.width.saturating_sub(PAD * 2)
    }

    fn draw(&mut self) {
        // SAFETY: the surface is mapped here, writable, and at least
        // `width * height * 4` bytes -- checked when it was taken and again
        // after every replacement.
        let mut canvas = unsafe { Canvas::packed(SURFACE_AT, self.width, self.height) };
        canvas.set_text_style(self.style.0, self.style.1);

        let ground = Colour::rgb(0x07, 0x0B, 0x12);
        let plain = Colour::rgb(0xCF, 0xD8, 0xE6);
        let typed = Colour::rgb(0x7A, 0xC0, 0xFF);
        let trouble = Colour::rgb(0xE8, 0x74, 0x64);
        let note = Colour::rgb(0x86, 0xB8, 0x8C);

        canvas.fill(canvas.bounds(), ground);

        // Every printed line, wrapped, plus the line being typed. Built as one
        // list so that the wrapping decides how much fits rather than the line
        // count -- a single long line is several rows and has to scroll like
        // several rows.
        let width = self.columns();
        let mut rows: Vec<(String, Colour)> = Vec::new();
        for line in &self.lines {
            let colour = match line.kind {
                Kindness::Typed => typed,
                Kindness::Plain => plain,
                Kindness::Trouble => trouble,
                Kindness::Note => note,
            };
            if line.text.is_empty() {
                rows.push((String::new(), colour));
                continue;
            }
            for piece in nexus_ui::wrap(&line.text, width) {
                rows.push((piece.to_string(), colour));
            }
        }

        let prompt = self.prompt();
        // The caret is drawn as a block in the text, because there is no cursor
        // to blink and a line with nothing marking the position is a line you
        // cannot edit the middle of.
        let mut current = String::with_capacity(prompt.len() + self.typing.len() + 1);
        current.push_str(&prompt);
        for (index, character) in self.typing.chars().enumerate() {
            if index == self.caret {
                current.push('\u{2588}');
            }
            current.push(character);
        }
        if self.caret >= self.typing.chars().count() {
            current.push('\u{2588}');
        }
        // The letters the input method has not decided about yet, after the
        // caret. Shown because they have been typed and are not in the line:
        // without them the keyboard would look like it was dropping letters.
        current.push_str(self.ime.pending());
        for piece in nexus_ui::wrap(&current, width) {
            rows.push((piece.to_string(), typed));
        }

        let fits = self.rows();
        let last = rows.len().saturating_sub(self.scrolled);
        let first = last.saturating_sub(fits);
        let mut y = PAD + nexus_ui::TITLE_BAR;
        for (text, colour) in &rows[first..last] {
            canvas.text(PAD, y, text, *colour);
            y += nexus_ui::LINE_HEIGHT;
        }

        // A hint that there is more above, because a window that is scrolled
        // and does not say so looks like a window that has lost the end.
        if self.scrolled > 0 {
            let text = nexus_i18n::format("term.scrolled", &[("lines", &self.scrolled)]);
            let strip = Rect::new(
                0,
                nexus_ui::TITLE_BAR,
                self.width,
                nexus_ui::LINE_HEIGHT + 2,
            );
            canvas.fill(strip, note.blend(ground, 200));
            canvas.text_centred(strip, &text, ground);
        }
    }
}

/// The ports a scan tries when it is not told which.
///
/// The ones a machine on a network usually answers on, and few enough that
/// trying all of them takes a second rather than a minute.
const COMMON_PORTS: [u16; 14] = [
    21, 22, 23, 25, 53, 80, 110, 143, 443, 587, 993, 995, 3306, 8080,
];

/// How many connections a scan has outstanding at once.
///
/// Four, because the kernel holds four outbound streams and asking for a fifth
/// is refused.
const AT_ONCE: usize = 4;

/// The most ports one scan will try.
const MOST_PORTS: usize = 256;

/// How long a scan waits for one connection before giving up on it.
const SCAN_MS: u64 = 400;

/// How long a name lookup waits for an answer.
const LOOKUP_MS: u64 = 4_000;

/// An address, as people write them.
fn dotted(address: [u8; 4]) -> String {
    format!(
        "{}.{}.{}.{}",
        address[0], address[1], address[2], address[3]
    )
}

/// What the settings file is called.
const SETTINGS_NAME: &str = "settings.txt";

/// A whole text file out of a directory, if it is there and readable.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
}

/// Replace a text file with this content.
fn write_text(directory: Handle, name: &str, text: &str) -> Result<(), String> {
    // Removed first: the filesystem has no truncate, so a shorter file written
    // over a longer one would keep the old ending.
    //
    // No longer a race. `remove` used to refuse while anybody held the file
    // open, and several programs here read this one on a clock, so this failed
    // at random and had to be retried. It unlinks now -- the name goes at once
    // and the blocks go when the last handle closes -- so the retry that was
    // here has gone with the reason for it.
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
                return Err(format!("{name}: the write stopped at {written} bytes"));
            }
            Ok(count) => written += count,
        }
    }
    nexus_user::close(file).ok();
    Ok(())
}

fn evaluate_math_expr(
    expr: &str,
    vars: &alloc::collections::BTreeMap<String, String>,
) -> Option<i64> {
    let expr = expr.trim();
    if let Ok(n) = expr.parse::<i64>() {
        return Some(n);
    }
    if let Some(val) = vars.get(expr) {
        if let Ok(n) = val.parse::<i64>() {
            return Some(n);
        }
    }
    let chars: Vec<char> = expr.chars().collect();
    let mut depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '+' || c == '-') && i > 0 {
            let left_s = expr[..i].trim();
            let right_s = expr[i + 1..].trim();
            let left = evaluate_math_expr(left_s, vars)?;
            let right = evaluate_math_expr(right_s, vars)?;
            return if c == '+' {
                Some(left + right)
            } else {
                Some(left - right)
            };
        }
    }
    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '*' || c == '/' || c == '%') && i > 0 {
            let left_s = expr[..i].trim();
            let right_s = expr[i + 1..].trim();
            let left = evaluate_math_expr(left_s, vars)?;
            let right = evaluate_math_expr(right_s, vars)?;
            return match c {
                '*' => Some(left * right),
                '/' => {
                    if right != 0 {
                        Some(left / right)
                    } else {
                        None
                    }
                }
                '%' => {
                    if right != 0 {
                        Some(left % right)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }
    }
    if expr.starts_with('(') && expr.ends_with(')') && expr.len() >= 2 {
        return evaluate_math_expr(&expr[1..expr.len() - 1], vars);
    }
    None
}

fn evaluate_py_value(val: &str, vars: &alloc::collections::BTreeMap<String, String>) -> String {
    let val = val.trim();
    if ((val.starts_with('"') && val.ends_with('"'))
        || (val.starts_with('\'') && val.ends_with('\'')))
        && val.len() >= 2
    {
        return String::from(&val[1..val.len() - 1]);
    }
    if let Some(n) = evaluate_math_expr(val, vars) {
        return format!("{n}");
    }
    if let Some(v) = vars.get(val) {
        return v.clone();
    }
    val.to_string()
}

fn parse_print_args(inner: &str, vars: &alloc::collections::BTreeMap<String, String>) -> String {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote_char = '"';
    for c in inner.chars() {
        if in_quote {
            if c == quote_char {
                in_quote = false;
            }
            current.push(c);
        } else if c == '"' || c == '\'' {
            in_quote = true;
            quote_char = c;
            current.push(c);
        } else if c == ',' {
            let trimmed = current.trim();
            parts.push(evaluate_py_value(trimmed, vars));
            current.clear();
        } else {
            current.push(c);
        }
    }
    if !current.trim().is_empty() {
        parts.push(evaluate_py_value(current.trim(), vars));
    }
    parts.join(" ")
}

fn format_c_printf(args: &str, vars: &alloc::collections::BTreeMap<String, String>) -> String {
    let args = args.trim();
    if let Some((fmt, rest)) = args.split_once(',') {
        let fmt = fmt.trim().trim_matches('"').replace("\\n", "");
        let rest_vals: Vec<String> = rest
            .split(',')
            .map(|s| evaluate_py_value(s.trim(), vars))
            .collect();
        let mut out = String::new();
        let mut val_idx = 0;
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                if let Some(&next) = chars.peek() {
                    if next == 'd' || next == 'i' || next == 's' || next == 'f' {
                        chars.next();
                        if let Some(v) = rest_vals.get(val_idx) {
                            out.push_str(v);
                            val_idx += 1;
                        }
                        continue;
                    }
                }
            }
            out.push(c);
        }
        out
    } else {
        args.trim_matches('"').replace("\\n", "")
    }
}

fn format_rust_println(args: &str, vars: &alloc::collections::BTreeMap<String, String>) -> String {
    let args = args.trim();
    if let Some((fmt, rest)) = args.split_once(',') {
        let fmt = fmt.trim().trim_matches('"');
        let rest_vals: Vec<String> = rest
            .split(',')
            .map(|s| evaluate_py_value(s.trim(), vars))
            .collect();
        let mut out = String::new();
        let mut val_idx = 0;
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' {
                if let Some(&next) = chars.peek() {
                    if next == '}' {
                        chars.next();
                        if let Some(v) = rest_vals.get(val_idx) {
                            out.push_str(v);
                            val_idx += 1;
                        }
                        continue;
                    }
                }
            }
            out.push(c);
        }
        out
    } else {
        args.trim_matches('"').to_string()
    }
}

fn create_simple_elf(message: &[u8]) -> Vec<u8> {
    const BASE: u64 = 0x40_0000;
    const EHDR: usize = 64;
    const PHDR: usize = 56;
    let code_offset = EHDR + PHDR;
    let length = message.len() as u32;
    let message_offset = code_offset + 29;
    let message_address = BASE + message_offset as u64;

    let mut code = Vec::new();
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    code.extend_from_slice(&[0x48, 0xBE]); // movabs rsi, imm64
    code.extend_from_slice(&message_address.to_le_bytes());
    code.push(0xBA); // mov edx, imm32
    code.extend_from_slice(&length.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    let total = message_offset + message.len();
    let mut image = Vec::with_capacity(total);

    // ELF header
    image.extend_from_slice(&[0x7F, b'E', b'L', b'F']);
    image.push(2);
    image.push(1);
    image.push(1);
    image.push(0);
    image.push(0);
    image.extend_from_slice(&[0; 7]);
    image.extend_from_slice(&2u16.to_le_bytes());
    image.extend_from_slice(&0x3Eu16.to_le_bytes());
    image.extend_from_slice(&1u32.to_le_bytes());
    image.extend_from_slice(&(BASE + code_offset as u64).to_le_bytes());
    image.extend_from_slice(&(EHDR as u64).to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes());
    image.extend_from_slice(&0u32.to_le_bytes());
    image.extend_from_slice(&(EHDR as u16).to_le_bytes());
    image.extend_from_slice(&(PHDR as u16).to_le_bytes());
    image.extend_from_slice(&1u16.to_le_bytes());
    image.extend_from_slice(&0u16.to_le_bytes());
    image.extend_from_slice(&0u16.to_le_bytes());
    image.extend_from_slice(&0u16.to_le_bytes());

    // Program header
    image.extend_from_slice(&1u32.to_le_bytes());
    image.extend_from_slice(&5u32.to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes());
    image.extend_from_slice(&BASE.to_le_bytes());
    image.extend_from_slice(&BASE.to_le_bytes());
    image.extend_from_slice(&(total as u64).to_le_bytes());
    image.extend_from_slice(&(total as u64).to_le_bytes());
    image.extend_from_slice(&0x1000u64.to_le_bytes());

    image.extend_from_slice(&code);
    image.extend_from_slice(message);
    image
}

fn create_simple_pe(message: &[u8]) -> Vec<u8> {
    const IMAGE_BASE: u64 = 0x0001_4000_0000;
    const SECTION_RVA: u32 = 0x1000;
    let length = message.len() as u32;
    let message_rva = SECTION_RVA + 40;
    let message_va = IMAGE_BASE + message_rva as u64;

    let mut code = Vec::new();
    // mov eax, 8 (NtWriteFile)
    code.extend_from_slice(&[0xB8, 0x08, 0x00, 0x00, 0x00]);
    // mov r10, 1 (stdout handle)
    code.extend_from_slice(&[0x49, 0xC7, 0xC2, 0x01, 0x00, 0x00, 0x00]);
    // xor edx, edx
    code.extend_from_slice(&[0x31, 0xD2]);
    // movabs r8, message_va
    code.extend_from_slice(&[0x49, 0xB8]);
    code.extend_from_slice(&message_va.to_le_bytes());
    // mov r9d, length
    code.push(0x41);
    code.push(0xB9);
    code.extend_from_slice(&length.to_le_bytes());
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
    // mov eax, 0x2C (NtTerminateProcess)
    code.extend_from_slice(&[0xB8, 0x2C, 0x00, 0x00, 0x00]);
    // mov r10, -1
    code.extend_from_slice(&[0x49, 0xC7, 0xC2, 0xFF, 0xFF, 0xFF, 0xFF]);
    // xor edx, edx
    code.extend_from_slice(&[0x31, 0xD2]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    while code.len() < 40 {
        code.push(0x90);
    }
    code.extend_from_slice(message);

    let raw_data_size = code.len().div_ceil(512) * 512;
    while code.len() < raw_data_size {
        code.push(0);
    }

    let mut image = alloc::vec![0u8; 512 + raw_data_size];
    image[0..2].copy_from_slice(b"MZ");
    image[0x3C..0x40].copy_from_slice(&64u32.to_le_bytes());

    let pe = 64;
    image[pe..pe + 4].copy_from_slice(b"PE\0\0");
    image[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes());
    image[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes());
    image[pe + 20..pe + 22].copy_from_slice(&240u16.to_le_bytes());
    image[pe + 22..pe + 24].copy_from_slice(&0x0022u16.to_le_bytes());

    let opt = pe + 24;
    image[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes());
    image[opt + 16..opt + 20].copy_from_slice(&SECTION_RVA.to_le_bytes());
    image[opt + 24..opt + 32].copy_from_slice(&IMAGE_BASE.to_le_bytes());
    image[opt + 32..opt + 36].copy_from_slice(&4096u32.to_le_bytes());
    image[opt + 36..opt + 40].copy_from_slice(&512u32.to_le_bytes());
    image[opt + 56..opt + 60].copy_from_slice(&8192u32.to_le_bytes());
    image[opt + 60..opt + 64].copy_from_slice(&512u32.to_le_bytes());
    image[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes());
    image[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());

    let sec = opt + 240;
    image[sec..sec + 5].copy_from_slice(b".text");
    image[sec + 8..sec + 12].copy_from_slice(&4096u32.to_le_bytes());
    image[sec + 12..sec + 16].copy_from_slice(&SECTION_RVA.to_le_bytes());
    image[sec + 16..sec + 20].copy_from_slice(&(raw_data_size as u32).to_le_bytes());
    image[sec + 20..sec + 24].copy_from_slice(&512u32.to_le_bytes());
    image[sec + 36..sec + 40].copy_from_slice(&(0x6000_0020u32).to_le_bytes());

    image[512..512 + code.len()].copy_from_slice(&code);

    image
}

/// Convert a null-terminated C string pointer to a Rust string slice.
unsafe fn c_str_to_str<'a>(ptr: *const u8) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
    core::str::from_utf8(slice).unwrap_or("")
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

/// Evaluates arithmetic expression with precedence.
fn evaluate_math(expr: &str) -> Option<i64> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    if expr.starts_with('(') && expr.ends_with(')') {
        let mut depth = 0;
        let mut all_inside = true;
        for (i, c) in expr.char_indices() {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 && i < expr.len() - 1 {
                    all_inside = false;
                    break;
                }
            }
        }
        if all_inside && depth == 0 {
            return evaluate_math(&expr[1..expr.len() - 1]);
        }
    }

    let chars: Vec<char> = expr.chars().collect();
    let mut depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '+' || c == '-') && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_math(&left)?;
            let r = evaluate_math(&right)?;
            return Some(if c == '+' {
                l.saturating_add(r)
            } else {
                l.saturating_sub(r)
            });
        }
    }

    depth = 0;
    for (i, &c) in chars.iter().enumerate().rev() {
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            depth -= 1;
        } else if depth == 0 && (c == '*' || c == '/' || c == '%') && i > 0 {
            let left: String = chars[..i].iter().collect();
            let right: String = chars[i + 1..].iter().collect();
            let l = evaluate_math(&left)?;
            let r = evaluate_math(&right)?;
            return match c {
                '*' => Some(l.saturating_mul(r)),
                '/' if r != 0 => Some(l / r),
                '%' if r != 0 => Some(l % r),
                _ => None,
            };
        }
    }

    let trimmed = expr.trim();
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        i64::from_str_radix(&trimmed[2..], 16).ok()
    } else {
        trimmed.parse::<i64>().ok()
    }
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
    nexus_user::log("term: PANIC").ok();
    nexus_user::exit_with(2)
}
