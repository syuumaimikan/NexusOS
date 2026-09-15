//! `assist`: an agent you can type a question at.
//!
//! # What it is, and what it is not
//!
//! It is not a language model. There is no model on this machine, nothing has
//! been trained, and there is no weight anywhere in this program. Saying so
//! first matters, because "AI" invites the reader to assume otherwise and every
//! sentence after that assumption is misread.
//!
//! What it is: an agent that reads a question, decides which of a small set of
//! **tools** would answer it, asks permission for that tool, runs it if it may,
//! and says what it did. That is the shape of every tool-using agent, and the
//! interesting half of one — the half that decides what a program is allowed to
//! do to a machine — is here in full. The half that is absent is the language
//! model that would choose the tool more cleverly than a table of keywords.
//!
//! # Permission is not this program's decision
//!
//! Every action goes through [`nexus_ai_core::permitted`], which is GPT-6
//! Astra's: it says which tools are safe, which need somebody to confirm, which
//! are privileged and which are refused outright. This window asks and obeys.
//!
//! The result is that a question this program cannot answer is refused with the
//! *reason* — "that needs somebody to confirm it", "that is not something this
//! machine will do" — rather than silently not happening. An agent that quietly
//! declines is an agent nobody can reason about.
//!
//! # What it was lent
//!
//! Two handles: the filesystem, **read-only**, and the channel that says what
//! the machine is doing. Not the spawner, not the network, and nothing that can
//! write. So the honest answer to "what can this agent do to my machine" is:
//! read files in the one directory it was handed, and ask how busy the machine
//! is. That is a fact about the handles and not a promise about the program.
//!
//! # Searching
//!
//! [`nexus_index`] hashes character三-grams into 256 buckets and compares
//! documents by the cosine of the angle between their count vectors. It is not
//! a model either: nothing is trained and there is no floating point in it, the
//! comparison being done by cross-multiplying two ratios. Characters rather
//! than words, because Japanese has no spaces.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_ai_core::{Status, Tool};
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::{Handle, Kind};
use nexus_window::{App, Key, Movement, Window};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the surface is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_3800_0000;

/// How much heap: an index over the files it can read, and the answers.
const HEAP: usize = 4 * 1024 * 1024;

/// The most files it will read while looking for something.
const MOST_FILES: usize = 128;

/// The most of any one file it will read.
const MOST_BYTES: usize = 64 * 1024;

/// How many lines of answer are kept.
const HISTORY: usize = 400;

/// Space around the text.
const PAD: u32 = 10;

/// What the settings directory is called, inside the filesystem.
const SYSTEM: &str = "system";
/// And the record of what is installed, inside that.
const INSTALLED: &str = "installed.txt";

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
        failed("assist: FAILED: could not get a heap");
        finish();
    }

    let mut lent = [Handle(0); 2];
    let (window, carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("assist: FAILED: {trouble}"));
            finish();
        }
    };

    let mut assistant = Assistant::new(
        (carried >= 1).then_some(lent[0]),
        (carried >= 2).then_some(lent[1]),
    );
    nexus_user::log("assist: an agent that can only do what it was lent").ok();

    let outcome = window.run(&mut assistant);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        failed(&format!("assist: FAILED: {}", outcome.ended));
    }
    if outcome.frames > 0 {
        nexus_user::log("assist: answered for as long as it was asked to").ok();
    }
    finish()
}

/// What a line in the transcript is, which decides its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kindness {
    /// What somebody typed, echoed back.
    Asked,
    /// An answer.
    Said,
    /// What the agent did, and under what permission.
    Did,
    /// A refusal, with its reason.
    Refused,
}

/// One line of the transcript.
struct Line {
    text: String,
    kind: Kindness,
}

/// The window.
struct Assistant {
    /// The filesystem, read-only, if this program was lent one.
    root: Option<Handle>,
    /// The channel that says what the machine is doing.
    machine: Option<Handle>,
    /// What has been said, oldest first.
    lines: Vec<Line>,
    /// What is being typed.
    typing: String,
    /// How far the view is scrolled back, in lines from the bottom.
    scrolled: usize,
    /// What the machine looks like, so this window matches it.
    look: nexus_look::Look,
}

impl Assistant {
    fn new(root: Option<Handle>, machine: Option<Handle>) -> Self {
        let look = root
            .and_then(|handle| nexus_user::open(handle, SYSTEM).ok())
            .and_then(|directory| {
                let text = read_text(directory, "settings.txt");
                nexus_user::close(directory).ok();
                text
            })
            .map(|text| nexus_look::Look::parse(&text))
            .unwrap_or_default();

        let mut assistant = Self {
            root,
            machine,
            lines: Vec::new(),
            typing: String::new(),
            scrolled: 0,
            look,
        };
        assistant.say(nexus_i18n::text("assist.welcome"), Kindness::Said);
        assistant.say(nexus_i18n::text("assist.notamodel"), Kindness::Said);
        assistant.say(nexus_i18n::text("assist.hint"), Kindness::Did);
        assistant
    }

    fn say(&mut self, text: &str, kind: Kindness) {
        for piece in text.split('\n') {
            if self.lines.len() >= HISTORY {
                self.lines.remove(0);
            }
            self.lines.push(Line {
                text: piece.to_string(),
                kind,
            });
        }
        self.scrolled = 0;
    }

    /// Ask whether a tool may be used, and say what the answer was.
    ///
    /// The policy is not this program's: `nexus_ai_core::permitted` decides,
    /// and this window asks and obeys.
    ///
    /// It asks `permitted` and not `authorize`, and the difference matters.
    /// `authorize` answers "may Astra's service execute this", which folds two
    /// questions into one: whether an agent is allowed to, and whether that
    /// service has implemented it. This window does its own reading, with its
    /// own read-only handle, so the second question is not about it -- and
    /// asking `authorize` made it report that looking through files was "not
    /// built yet" while doing exactly that. `permitted` is the first question
    /// on its own, which is the one an agent holding its own capabilities has
    /// any business asking.
    ///
    /// It asks `permitted` and not `requirement(tool).1` either, though that
    /// was what it did first and the two agree today. `requirement` is the
    /// policy *data*; `permitted` is the policy *decision*. A caller that
    /// re-derives the decision from the data has quietly made itself a second
    /// place where policy lives, and the day those two disagree is the day an
    /// agent does something the policy said it must not.
    ///
    /// `requirement` is still asked, for the name of the permission alone --
    /// that is description, not decision, and a refusal that cannot say what
    /// was being asked for is a worse refusal.
    ///
    /// A refusal names the reason, because "no" without one is a bug report
    /// waiting to be filed against the wrong thing.
    fn may(&mut self, tool: Tool) -> bool {
        let (permission, _) = nexus_ai_core::requirement(tool);
        let named = format!("{permission:?}");
        let tool_name = format!("{tool:?}");

        let status = match nexus_ai_core::permitted(tool) {
            Ok(()) => {
                let did = nexus_i18n::format(
                    "assist.using",
                    &[("tool", &tool_name), ("permission", &named)],
                );
                self.say(&did, Kindness::Did);
                return true;
            }
            Err(status) => status,
        };

        // `permitted` returns one of three today, and `Status` has ten
        // variants. The last arm is not padding and is not unreachable in the
        // sense that matters: it is what happens when Astra adds a reason to
        // refuse and this window has not been taught to phrase it. Refusing
        // and naming the raw status is right; the alternative is a match that
        // has to be edited in lockstep with somebody else's crate, and the
        // failure mode of forgetting is an agent that does the thing.
        let status_name = format!("{status:?}");
        // Typed, because binding the list to a name loses the unsizing that
        // happens for free when it is written out at the call.
        let (key, fields): (&str, [(&str, &dyn core::fmt::Display); 2]) = match status {
            Status::ConfirmRequired => (
                "assist.needsconfirming",
                [("tool", &tool_name), ("permission", &named)],
            ),
            Status::Privileged => (
                "assist.privileged",
                [("tool", &tool_name), ("permission", &named)],
            ),
            Status::Blocked => (
                "assist.blocked",
                [("tool", &tool_name), ("permission", &named)],
            ),
            _ => (
                "assist.refusedunknown",
                [("tool", &tool_name), ("reason", &status_name)],
            ),
        };
        let why = nexus_i18n::format(key, &fields);
        self.say(&why, Kindness::Refused);
        false
    }

    /// Read the question and do something about it.
    fn answer(&mut self, question: &str) {
        let asked = question.trim();
        if asked.is_empty() {
            return;
        }
        self.say(&format!("> {asked}"), Kindness::Asked);
        nexus_user::log("assist: answered a question").ok();

        let lowered = asked.to_lowercase();
        let wants = |words: &[&str]| words.iter().any(|word| lowered.contains(word));

        // A table of words and not a model, and the code says so where somebody
        // reading it would otherwise wonder. What this decides is only *which
        // tool*; whether the tool may run is decided elsewhere and cannot be
        // talked round by the phrasing of a question.
        if wants(&[
            "memory",
            "cpu",
            "processor",
            "busy",
            "machine",
            "state",
            "メモリ",
            "状態",
            "負荷",
        ]) {
            self.about_the_machine();
        } else if wants(&["package", "installed", "software", "パッケージ", "導入"]) {
            self.about_packages();
        } else if let Some(what) = after_any(
            &lowered,
            asked,
            &["find ", "search ", "look for ", "探して", "検索"],
        ) {
            self.search(&what);
        } else if wants(&[
            "delete", "remove", "write", "install", "run ", "消し", "削除", "書き",
        ]) {
            // Deliberately reached: somebody asking for one of these should see
            // the permission model refuse it by name, not a shrug.
            let tool = if wants(&["delete", "remove", "消し", "削除"]) {
                Tool::FileRemove
            } else if wants(&["run ", "install"]) {
                Tool::TerminalExecute
            } else {
                Tool::FileWrite
            };
            self.may(tool);
        } else {
            self.say(nexus_i18n::text("assist.cannot"), Kindness::Said);
            self.say(nexus_i18n::text("assist.hint"), Kindness::Did);
        }
    }

    /// How busy the machine is, through the kernel's snapshot service.
    fn about_the_machine(&mut self) {
        if !self.may(Tool::SystemInfo) {
            return;
        }
        let Some(machine) = self.machine else {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        };

        let asked = nexus_machine::request();
        if nexus_user::send(machine, &asked, &[]).is_err() {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        }
        let mut reply = [0u8; 128];
        let mut none = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(machine, &mut reply, &mut none) else {
            self.say(nexus_i18n::text("assist.nomachine"), Kindness::Refused);
            return;
        };
        match nexus_machine::Snapshot::of(&reply[..received.bytes]) {
            Ok(snapshot) => {
                let said = nexus_i18n::format(
                    "assist.machine",
                    &[
                        ("used", &(snapshot.memory_used() / (1024 * 1024))),
                        ("total", &(snapshot.memory_total / (1024 * 1024))),
                        ("percent", &snapshot.memory_percent()),
                        ("processes", &snapshot.processes_running),
                        ("threads", &snapshot.threads),
                        ("processors", &snapshot.processors),
                    ],
                );
                self.say(&said, Kindness::Said);
            }
            Err(why) => self.say(&format!("{why}"), Kindness::Refused),
        }
    }

    /// What is on record as installed.
    fn about_packages(&mut self) {
        if !self.may(Tool::FileRead) {
            return;
        }
        let Some(root) = self.root else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };
        let Ok(directory) = nexus_user::open(root, SYSTEM) else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };
        let text = read_text(directory, INSTALLED);
        nexus_user::close(directory).ok();

        match text {
            Some(text) if !text.trim().is_empty() => {
                let record = nexus_update::Installed::parse(&text);
                let count = record.len();
                let said = nexus_i18n::format("assist.packages", &[("count", &count)]);
                self.say(&said, Kindness::Said);
                for line in text.lines().take(20) {
                    if !line.trim().is_empty() && !line.starts_with('#') {
                        self.say(&format!("  {}", line.trim()), Kindness::Said);
                    }
                }
            }
            _ => self.say(nexus_i18n::text("assist.nopackages"), Kindness::Said),
        }
    }

    /// Look through the files it can read for the one most like a phrase.
    fn search(&mut self, what: &str) {
        if !self.may(Tool::FileRead) {
            return;
        }
        let Some(root) = self.root else {
            self.say(nexus_i18n::text("assist.nofiles"), Kindness::Refused);
            return;
        };

        let mut index: nexus_index::Index<String> = nexus_index::Index::new();
        let mut read = 0usize;
        gather(root, "", &mut index, &mut read, 0);

        let searched = nexus_i18n::format("assist.searched", &[("count", &read)]);
        self.say(&searched, Kindness::Did);

        match index.best(what) {
            Some((name, overlap, size)) => {
                let found = nexus_i18n::format(
                    "assist.found",
                    &[("name", name), ("overlap", &overlap), ("size", &size)],
                );
                self.say(&found, Kindness::Said);
            }
            None => self.say(nexus_i18n::text("assist.notfound"), Kindness::Said),
        }
    }
}

/// Read every file under a directory into an index, up to a bound.
///
/// Two levels deep and a hundred and twenty-eight files, because an agent that
/// walked an unbounded tree would be an agent a deep directory could hang.
fn gather(
    directory: Handle,
    prefix: &str,
    index: &mut nexus_index::Index<String>,
    read: &mut usize,
    depth: usize,
) {
    if depth > 1 || *read >= MOST_FILES {
        return;
    }
    let mut packed = alloc::vec![0u8; 8 * 1024];
    let listed = nexus_user::list(directory, &mut packed).unwrap_or(0);
    packed.truncate(listed);

    for entry in nexus_user::entries(&packed) {
        if *read >= MOST_FILES {
            return;
        }
        let name = if prefix.is_empty() {
            entry.name.to_string()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.kind {
            Kind::Directory => {
                if let Ok(child) = nexus_user::open(directory, entry.name) {
                    gather(child, &name, index, read, depth + 1);
                    nexus_user::close(child).ok();
                }
            }
            Kind::File => {
                if let Some(text) = read_text(directory, entry.name) {
                    *read += 1;
                    index.add(name, &text);
                }
            }
        }
    }
}

impl App for Assistant {
    fn draw(&mut self, canvas: &mut Canvas) {
        canvas.set_text_style(
            nexus_ui::font::Face::parse(Some(self.look.font.name())),
            self.look.smooth,
        );

        let top = Colour(self.look.top.packed());
        let bottom = Colour(self.look.bottom.packed());
        let accent = Colour(self.look.accent.packed());
        let ink = Colour::rgb(0xE6, 0xEC, 0xF5);
        let quiet = Colour::rgb(0x8A, 0x9A, 0xB4);
        let bad = Colour::rgb(0xE0, 0x80, 0x70);

        canvas.gradient(canvas.bounds(), top, bottom);

        let area = canvas.bounds().inset(PAD);
        let prompt_height = nexus_ui::LINE_HEIGHT + 8;
        let rows = (area.height.saturating_sub(prompt_height) / nexus_ui::LINE_HEIGHT) as usize;

        // The newest lines, unless somebody has scrolled back.
        let end = self.lines.len().saturating_sub(self.scrolled);
        let start = end.saturating_sub(rows);
        for (row, line) in self.lines[start..end].iter().enumerate() {
            let colour = match line.kind {
                Kindness::Asked => accent,
                Kindness::Said => ink,
                Kindness::Did => quiet,
                Kindness::Refused => bad,
            };
            canvas.text(
                area.x,
                area.y + row as u32 * nexus_ui::LINE_HEIGHT,
                &line.text,
                colour,
            );
        }

        // The question being typed, in a panel of its own so it is obviously
        // where the typing goes.
        let prompt = Rect::new(
            area.x,
            area.y + area.height - prompt_height,
            area.width,
            prompt_height,
        );
        canvas.panel(prompt, 5, blend(bottom, accent, 40), accent);
        let typed = format!("{}\u{2588}", self.typing);
        canvas.text(prompt.x + 8, prompt.y + 4, &typed, ink);
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Character(character) => {
                self.typing.push(character);
                self.scrolled = 0;
                true
            }
            Key::Backspace => self.typing.pop().is_some(),
            Key::Enter => {
                let question = core::mem::take(&mut self.typing);
                self.answer(&question);
                true
            }
            Key::Escape => {
                if self.typing.is_empty() {
                    return false;
                }
                self.typing.clear();
                true
            }
            Key::Move(Movement::PageUp) => {
                self.scrolled = (self.scrolled + 8).min(self.lines.len());
                true
            }
            Key::Move(Movement::PageDown) => {
                let was = self.scrolled;
                self.scrolled = self.scrolled.saturating_sub(8);
                was != self.scrolled
            }
            Key::Language => true,
            _ => false,
        }
    }
}

/// Whatever follows the first of these words, if any of them appear.
fn after_any(lowered: &str, original: &str, words: &[&str]) -> Option<String> {
    for word in words {
        if let Some(at) = lowered.find(word) {
            let rest = original[at + word.len()..].trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// A whole text file out of a directory, if it is there and readable.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(MOST_BYTES);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
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
    nexus_user::log("assist: PANIC").ok();
    nexus_user::exit_with(2)
}
