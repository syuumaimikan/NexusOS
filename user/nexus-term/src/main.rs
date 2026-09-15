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
        style: (nexus_ui::font::Face::Crisp, false),
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
    "help", "?", "clear", "cls", "echo", "pwd", "dir", "cd", "cat", "type", "write", "append",
    "mkdir", "rm", "del", "run", "date", "beep", "set", "look", "sys", "top", "net", "usb",
    "lookup", "dig", "nslookup", "scan", "uptime", "history",
];

/// Split a typed line at its first `|`, if it has one outside quotes.
///
/// Outside quotes, because `echo "a | b"` is one argument containing a bar and
/// not a pipe. The scan is the same one `nexus_shellwords` does and it is done
/// again here rather than shared, because this needs the *position* of the bar
/// in the original text and the splitter returns words.
fn split_pipe(line: &str) -> Option<(alloc::string::String, alloc::string::String)> {
    let mut quote: Option<char> = None;
    for (at, character) in line.char_indices() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character == '|' => {
                let left = line[..at].trim();
                let right = line[at + 1..].trim();
                if left.is_empty() || right.is_empty() {
                    return None;
                }
                return Some((
                    alloc::string::String::from(left),
                    alloc::string::String::from(right),
                ));
            }
            None => {}
        }
    }
    None
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
        if let Some((left, right)) = split_pipe(line) {
            self.pipe_line(&left, &right);
            return;
        }

        match command {
            "help" | "?" => self.help(),
            "clear" | "cls" => {
                self.lines.clear();
                self.scrolled = 0;
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
            "cat" | "type" => self.show(words.argument(0)),
            "write" => self.write_file(words.argument(0), &words.arguments().join(" ")),
            "append" => self.append_file(words.argument(0), &words.arguments().join(" ")),
            "mkdir" => self.make_directory(words.argument(0)),
            "rm" | "del" => self.remove(words.argument(0)),
            "run" => self.start(&words),
            "date" => self.date(),
            "beep" => self.beep(words.argument(0), words.argument(1)),
            "set" => self.set(words.argument(0), words.argument(1)),
            "look" => self.look(),
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
                        let is_directory = nexus_user::list(handle, &mut probe).is_ok();
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

    /// `cat`
    fn show(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            self.trouble(nexus_i18n::text("term.needname"));
            return;
        };
        let Some(directory) = self.directory() else {
            self.trouble(nexus_i18n::text("term.nofiles"));
            return;
        };
        let file = match nexus_user::open(directory, name) {
            Ok(handle) => handle,
            Err(error) => {
                let text =
                    nexus_i18n::format("term.cannotopen", &[("name", &name), ("why", &error)]);
                self.trouble(&text);
                return;
            }
        };
        let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
        let mut bytes = alloc::vec![0u8; size];
        let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
        nexus_user::close(file).ok();
        bytes.truncate(read);

        if bytes.is_empty() {
            self.note(nexus_i18n::text("term.emptyfile"));
            return;
        }
        // Shown as text, replacing what is not. A file of machine code printed
        // as a wall of replacement characters is an honest answer to `cat` on
        // a program, and refusing to show it would be less use.
        let text = String::from_utf8_lossy(&bytes).replace('\t', "    ");
        self.plain(&text);
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

    /// Start `program` with the two ends it is to use, and say what came back.
    ///
    /// Returns the channel to it and its process, or `None` having already said
    /// what went wrong. Both `output` and `input` are given away by this call,
    /// whether it succeeds or not.
    ///
    /// Split out from running one so that two can be started before either is
    /// waited for, which is what a pipe needs: a shell that waited for the left
    /// program before starting the right one would deadlock the moment the left
    /// one filled the channel between them.
    fn spawn_program(
        &mut self,
        program: &str,
        arguments: &str,
        output: Handle,
        input: Handle,
    ) -> Option<(Handle, Handle)> {
        let Some(spawner) = self.spawner else {
            nexus_user::close(output).ok();
            nexus_user::close(input).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return None;
        };

        // Three things to lend it, in the order every program on this machine
        // expects: somewhere to write, somewhere to read, and the directory
        // this shell is looking at.
        //
        // The directory is duplicated, not lent: sending a handle gives it up,
        // and a shell that handed its own working directory to the first
        // program it ran would have no files afterwards.
        //
        // With `TRANSFER` on the copy, and that is not a detail. A handle is
        // only passable if it carries the right to be passed, so a copy made
        // with `READ` alone cannot be put in a message -- and the send fails
        // whole, taking the two channel ends with it, so the program never
        // starts and nothing says why. Read is what the program gets to *do*
        // with the directory; transfer is what this shell needs to hand it
        // over at all.
        let mut lent = alloc::vec![output, input];
        if let Some(directory) = self.directory() {
            if let Ok(copy) = nexus_user::duplicate(
                directory,
                nexus_user::rights::READ | nexus_user::rights::TRANSFER,
            ) {
                lent.push(copy);
            }
        }

        // The path, a zero byte, then the arguments -- which the kernel sends
        // down the new program's parent channel before handing it over, so they
        // are waiting when it makes its first read. The zero byte goes in even
        // when there are no arguments: it is what tells the kernel this caller
        // uses arguments at all, and without it a program that reads them first
        // waits for a message that never comes.
        let mut request = alloc::vec::Vec::new();
        request.extend_from_slice(program.as_bytes());
        request.push(0);
        request.extend_from_slice(arguments.as_bytes());

        if let Err(error) = nexus_user::send(spawner, &request, &lent) {
            // Named, because the three ways this fails look identical from the
            // outside: no spawn service, a handle that cannot be passed on, and
            // a message too large. The first version said only "no programs"
            // and a missing `TRANSFER` right took an hour to find.
            let text = nexus_i18n::format("term.cannotrun", &[("name", &program), ("why", &error)]);
            self.trouble(&text);
            return None;
        }
        let mut reply = [0u8; 128];
        let mut handles = [Handle(0); 2];
        let Ok(received) = nexus_user::receive(spawner, &mut reply, &mut handles) else {
            self.trouble(nexus_i18n::text("term.noprograms"));
            return None;
        };
        if received.handles != 2 {
            let said = core::str::from_utf8(&reply[..received.bytes]).unwrap_or("");
            let text = nexus_i18n::format("term.cannotrun", &[("name", &program), ("why", &said)]);
            self.trouble(&text);
            return None;
        }
        Some((handles[0], handles[1]))
    }

    /// A channel end that is already finished.
    ///
    /// For a program with nothing to read. It still gets a handle, so that the
    /// numbering is the same for every program rather than depending on how it
    /// was started; this end is dropped at once, which is what makes the
    /// program's first read report the end of its input.
    fn nothing_to_read(&mut self) -> Option<Handle> {
        match nexus_user::channel() {
            Ok((ours, theirs)) => {
                nexus_user::close(ours).ok();
                Some(theirs)
            }
            Err(_) => {
                self.trouble(nexus_i18n::text("term.noprograms"));
                None
            }
        }
    }

    /// Show everything written to `mine` until the other end goes, and say how
    /// many bytes that was.
    ///
    /// Read *while* the program runs, not after. A channel holds a bounded
    /// number of messages, so a program that printed more than that into a
    /// queue nobody was draining would block for ever waiting for room -- and
    /// the shell would be blocked waiting for the program. This ends when the
    /// other end closes, which is what the program exiting does to it.
    fn show_output(&mut self, mine: Handle) -> usize {
        let mut written = alloc::string::String::new();
        let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
        let mut none = [Handle(0); 1];
        let mut read = 0usize;
        while let Ok(got) = nexus_user::receive(mine, &mut buffer, &mut none) {
            read += got.bytes;
            written.push_str(&alloc::string::String::from_utf8_lossy(&buffer[..got.bytes]));
            // Shown a line at a time as it arrives, so a slow program is
            // something you watch rather than something that appears all at
            // once when it finishes.
            while let Some(at) = written.find('\n') {
                let line: alloc::string::String = written.drain(..=at).collect();
                self.plain(line.trim_end_matches('\n'));
            }
        }
        // Whatever it wrote without a line ending on the end is still output.
        if !written.is_empty() {
            let rest = core::mem::take(&mut written);
            self.plain(&rest);
        }
        nexus_user::close(mine).ok();
        read
    }

    /// Wait for a program and say how it ended, unless it ended well.
    fn finished(&mut self, program: &str, channel: Handle, process: Handle) -> Option<u32> {
        let ending = nexus_user::wait(process);
        nexus_user::close(channel).ok();
        nexus_user::close(process).ok();
        match ending {
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
        let Some(input) = self.nothing_to_read() else {
            nexus_user::close(mine).ok();
            nexus_user::close(theirs).ok();
            return None;
        };
        let (channel, process) = self.spawn_program(program, arguments, theirs, input)?;

        let read = self.show_output(mine);
        // What the program wrote, not what the window now holds. The two are
        // different numbers and the second one is useless: a shell with a full
        // scrollback would report the same figure whatever the program did.
        nexus_user::log(&format!("term: {program} wrote {read} bytes")).ok();
        self.finished(program, channel, process)
    }

    /// Take one side of a pipe and say which program it is and what it is given.
    ///
    /// The same rule the fallback below uses: a word that is not a built-in
    /// command is the name of something in `BIN`. `None` when the side is
    /// empty or names something this shell does itself.
    fn piped_program(&mut self, side: &str) -> Option<(alloc::string::String, alloc::string::String)> {
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
    fn pipe_line(&mut self, left: &str, right: &str) {
        let Some((left_program, left_arguments)) = self.piped_program(left) else {
            return;
        };
        let Some((right_program, right_arguments)) = self.piped_program(right) else {
            return;
        };
        self.run_pipe(
            &left_program,
            &left_arguments,
            &right_program,
            &right_arguments,
        );
    }

    /// Run `left | right`: the first program's output is the second one's input.
    ///
    /// A pipe here is not a new kind of object. It is one ordinary channel,
    /// handed to one program as its standard output and to the other as its
    /// standard input, and neither of them knows which end it has. That is the
    /// whole of it, and it is why this function is short.
    ///
    /// Both are started before either is waited for. The other order deadlocks:
    /// a channel holds a bounded number of messages, so a left-hand program
    /// that writes more than that stops until somebody reads -- and the reader
    /// is a program the shell has not started yet.
    fn run_pipe(&mut self, left: &str, left_arguments: &str, right: &str, right_arguments: &str) {
        // The pipe itself. `between` is written by the left program; `and` is
        // read by the right one.
        let Ok((between, and)) = nexus_user::channel() else {
            self.trouble(nexus_i18n::text("term.noprograms"));
            return;
        };
        // And the right-hand program's own output, which is what reaches the
        // window.
        let Ok((mine, theirs)) = nexus_user::channel() else {
            nexus_user::close(between).ok();
            nexus_user::close(and).ok();
            self.trouble(nexus_i18n::text("term.noprograms"));
            return;
        };
        let Some(nothing) = self.nothing_to_read() else {
            nexus_user::close(between).ok();
            nexus_user::close(and).ok();
            nexus_user::close(mine).ok();
            nexus_user::close(theirs).ok();
            return;
        };

        let Some((left_channel, left_process)) =
            self.spawn_program(left, left_arguments, between, nothing)
        else {
            nexus_user::close(and).ok();
            nexus_user::close(mine).ok();
            nexus_user::close(theirs).ok();
            return;
        };
        let Some((right_channel, right_process)) =
            self.spawn_program(right, right_arguments, theirs, and)
        else {
            nexus_user::close(mine).ok();
            self.finished(left, left_channel, left_process);
            return;
        };

        let read = self.show_output(mine);
        nexus_user::log(&format!("term: {left} | {right} wrote {read} bytes")).ok();
        // The left one first, because it is the one that has already finished:
        // the right one cannot have closed its output until its input ended,
        // and its input ending is the left one exiting.
        self.finished(left, left_channel, left_process);
        self.finished(right, right_channel, right_process);
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

        let asked = nexus_machine::request();
        if nexus_user::send(machine, &asked, &[]).is_err() {
            self.trouble(nexus_i18n::text("term.nomachine"));
            return;
        }

        let mut reply = [0u8; 128];
        let mut none = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(machine, &mut reply, &mut none) else {
            self.trouble(nexus_i18n::text("term.nomachine"));
            return;
        };

        let snapshot = match nexus_machine::Snapshot::of(&reply[..received.bytes]) {
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
            self.trouble(&nexus_i18n::format("term.usb.notanumber", &[("what", &drive)]));
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
        (self.height.saturating_sub(PAD * 2) / nexus_ui::LINE_HEIGHT) as usize
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
        let mut y = PAD;
        for (text, colour) in &rows[first..last] {
            canvas.text(PAD, y, text, *colour);
            y += nexus_ui::LINE_HEIGHT;
        }

        // A hint that there is more above, because a window that is scrolled
        // and does not say so looks like a window that has lost the end.
        if self.scrolled > 0 {
            let text = nexus_i18n::format("term.scrolled", &[("lines", &self.scrolled)]);
            let strip = Rect::new(0, 0, self.width, nexus_ui::LINE_HEIGHT + 2);
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
    nexus_user::log("term: PANIC").ok();
    nexus_user::exit_with(2)
}
