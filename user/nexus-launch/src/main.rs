//! `launch`: type a few letters, press Enter, and the thing starts.
//!
//! The same idea as `rofi` or `dmenu`: one window, a line to type in, a list
//! that narrows as you type, and Enter on whatever is at the top. The point is
//! that reaching a program should cost the letters in its name rather than a
//! journey across the screen with a pointer.
//!
//! # It does not start anything itself
//!
//! Worth being clear about, because a launcher sounds like the one program that
//! *would* have that power. It does not. It holds no spawner, no filesystem and
//! no directory; it cannot read `BIN/` and would not know what is in it if it
//! could.
//!
//! Existing window names/slots are supplied by the compositor. A selection
//! sends a checked activation request; it never receives window handles.
//!
//! What it does is send four bytes to the compositor saying which of a fixed
//! set of things somebody chose. The compositor decides whether to do it, which
//! program that means, and what to lend it — exactly as it already does when
//! the desktop's own buttons are pressed. This window is a *keyboard* for a
//! menu that already existed.
//!
//! That is why the list is fixed rather than read from the disk. A launcher
//! that listed every file in `BIN/` would be a launcher asking the compositor
//! to run arbitrary names, and the compositor would then be deciding what to
//! lend a program it had never heard of.
//!
//! # Matching
//!
//! Two ways, both case-insensitive, and an entry matches if either does:
//!
//! * the letters appear **in order** anywhere in the name — `tl` finds
//!   "Terminal", `stg` finds "Settings";
//! * or the typed text is a plain substring.
//!
//! Subsequence matching is what makes this feel like rofi rather than like a
//! filter box. Entries that match a *prefix* sort first, because when somebody
//! types `s` they almost always mean the thing beginning with s.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
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
const SURFACE_AT: usize = 0x0000_0000_3800_0000;

/// How much heap. A list of eleven short strings and the text somebody typed;
/// this is generous by a wide margin and is the smallest round number that is.
const HEAP: usize = 1024 * 1024;

/// Space around things.
const PAD: u32 = 14;

/// What one entry in the list is.
struct Entry {
    /// The key its name is under, in `locales/`.
    ///
    /// Not the name itself: the interface language can change while this window
    /// is open, and a list that had copied the words at startup would go on
    /// showing the old ones.
    key: &'static str,
    /// The four bytes sent to the compositor when this is chosen.
    tag: &'static [u8],
    /// Whether choosing it stops the machine or the session.
    ///
    /// Drawn differently, and placed at the end. These are the entries somebody
    /// least wants to hit by accident, and a list where "Off" can end up under
    /// the cursor because it happened to match two letters is a list that will
    /// eventually turn a machine off by accident.
    grave: bool,
}

/// Everything this window can ask for.
///
/// Fixed, and the same set the desktop's strip offers -- see the note at the
/// top about why it is not read from the disk.
const ENTRIES: &[Entry] = &[
    Entry {
        key: "shell.term",
        tag: b"term",
        grave: false,
    },
    Entry {
        key: "shell.web",
        tag: b"web ",
        grave: false,
    },
    Entry {
        key: "shell.launch",
        tag: b"open",
        grave: false,
    },
    Entry {
        key: "shell.settings",
        tag: b"sett",
        grave: false,
    },
    Entry {
        key: "shell.packages",
        tag: b"pkgs",
        grave: false,
    },
    Entry {
        key: "shell.pictures",
        tag: b"pics",
        grave: false,
    },
    Entry {
        key: "shell.editor",
        tag: b"edit",
        grave: false,
    },
    Entry {
        key: "shell.files",
        tag: b"file",
        grave: false,
    },
    Entry {
        key: "shell.assist",
        tag: b"asst",
        grave: false,
    },
    Entry {
        key: "shell.unpack",
        tag: b"unpk",
        grave: false,
    },
    Entry {
        key: "shell.solid",
        tag: b"sold",
        grave: false,
    },
    Entry {
        key: "shell.netool",
        tag: b"netw",
        grave: false,
    },
    Entry {
        key: "shell.probe",
        tag: b"prob",
        grave: false,
    },
    Entry {
        key: "shell.fileprobe",
        tag: b"fchk",
        grave: false,
    },
    Entry {
        key: "shell.sleep",
        tag: b"slep",
        grave: true,
    },
    Entry {
        key: "shell.restart",
        tag: b"rest",
        grave: true,
    },
    Entry {
        key: "shell.shutdown",
        tag: b"halt",
        grave: true,
    },
    Entry {
        key: "shell.leave",
        tag: b"quit",
        grave: true,
    },
];

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
        failed("launch: FAILED: could not get a heap");
        finish();
    }

    let mut lent = [Handle(0); 1];
    let (window, _carried) = match Window::open(COMPOSITOR, SURFACE_AT, &mut lent) {
        Ok(opened) => opened,
        Err(trouble) => {
            failed(&format!("launch: FAILED: {trouble}"));
            finish();
        }
    };
    // Nothing is lent to this program and nothing is expected. Anything that
    // arrives anyway is closed rather than held: a window that quietly kept a
    // handle it had no use for would be a window whose authority nobody could
    // account for by reading it.
    for handle in lent.iter().take(_carried) {
        nexus_user::close(*handle).ok();
    }

    let mut launcher = Launcher::new();
    nexus_user::log("launch: a window for starting things by name").ok();

    let outcome = window.run(&mut launcher);
    if outcome.ended != nexus_window::Ended::Finished
        && outcome.ended != nexus_window::Ended::Disconnected
    {
        failed(&format!("launch: FAILED: {}", outcome.ended));
    }
    finish()
}

/// The window.
struct Launcher {
    windows: Vec<OpenWindow>,
    /// What has been typed.
    typed: String,
    /// Which of the matches is under the cursor.
    at: usize,
    /// Matching app indices followed by window indices, best first.
    matching: Vec<usize>,
    /// Still open.
    open: bool,
    /// A request has been sent and the compositor has not confirmed it yet.
    awaiting_acceptance: bool,
    /// What the machine looks like. Read once: this window is open for a few
    /// seconds and a person does not change the accent while using it.
    look: nexus_look::Look,
}

impl Launcher {
    fn new() -> Self {
        let mut launcher = Self {
            windows: Vec::new(),
            typed: String::new(),
            at: 0,
            matching: Vec::new(),
            open: true,
            awaiting_acceptance: false,
            look: nexus_look::Look::default(),
        };
        launcher.narrow();
        launcher
    }

    /// Work out which entries match what has been typed, best first.
    fn narrow(&mut self) {
        let wanted = self.typed.to_lowercase();
        let mut scored: Vec<(u8, usize)> = Vec::new();

        for (index, entry) in ENTRIES.iter().enumerate() {
            let name = nexus_i18n::text(entry.key).to_lowercase();
            if wanted.is_empty() {
                scored.push((1, index));
                continue;
            }
            // Ranked rather than merely filtered: 0 beats 1 beats 2, and the
            // order within a rank is the order the entries are written in --
            // which puts the things somebody reaches for most at the top.
            let rank = if name.starts_with(&wanted) {
                0
            } else if name.contains(&wanted) {
                1
            } else if is_subsequence(&wanted, &name) {
                2
            } else {
                continue;
            };
            scored.push((rank, index));
        }

        for (index, window) in self.windows.iter().enumerate() {
            let name = window.name.to_lowercase();
            let rank = if wanted.is_empty() {
                1
            } else if name.starts_with(&wanted) {
                0
            } else if name.contains(&wanted) {
                1
            } else if is_subsequence(&wanted, &name) {
                2
            } else {
                continue;
            };
            scored.push((rank, ENTRIES.len() + index));
        }
        scored.sort_by_key(|(rank, index)| (*rank, *index));
        self.matching = scored.into_iter().map(|(_, index)| index).collect();
        // The cursor cannot point past the end of a list that has just got
        // shorter, and a cursor left where it was would select something the
        // person can no longer see.
        self.at = self.at.min(self.matching.len().saturating_sub(1));
    }

    /// Ask for whatever is under the cursor. The compositor closes this
    /// window with an acknowledgement after it has consumed the request.
    fn choose(&mut self) -> bool {
        if let Some(window) = self
            .matching
            .get(self.at)
            .and_then(|index| index.checked_sub(ENTRIES.len()))
            .and_then(|index| self.windows.get(index))
        {
            let mut request = [0u8; 8];
            request[..4].copy_from_slice(b"pick");
            request[4..].copy_from_slice(&window.slot.to_le_bytes());
            if nexus_user::send(COMPOSITOR, &request, &[]).is_err() {
                failed("launch: FAILED: window switch could not be sent");
            } else {
                nexus_user::log("launch: asked to switch to an open window").ok();
            }
            self.awaiting_acceptance = true;
            return true;
        }
        let Some(entry) = self.matching.get(self.at).and_then(|at| ENTRIES.get(*at)) else {
            return false;
        };
        // Four bytes to the compositor, which decides what they mean. See the
        // note at the top of this file: nothing here starts anything.
        if nexus_user::send(COMPOSITOR, entry.tag, &[]).is_err() {
            failed("launch: FAILED: the compositor would not take the request");
        } else {
            nexus_user::log(&format!(
                "launch: asked for {}",
                core::str::from_utf8(entry.tag).unwrap_or("????").trim()
            ))
            .ok();
        }
        // Closed either way. A launcher that stayed open after choosing would
        // be a launcher sitting on top of the thing it had just started.
        self.awaiting_acceptance = true;
        true
    }

    /// Move the cursor, wrapping.
    fn step(&mut self, by: isize) -> bool {
        if self.matching.len() < 2 {
            return false;
        }
        let count = self.matching.len() as isize;
        self.at = ((self.at as isize + by).rem_euclid(count)) as usize;
        true
    }
}

/// A compositor-supplied window name and slot, without window capabilities.
struct OpenWindow {
    slot: u32,
    name: String,
    away: bool,
}

///
/// Whether every character of `wanted` appears in `name`, in order.
/// What makes `tl` find "Terminal". Compares by `char` rather than by byte, so
/// a Japanese name is matched a character at a time rather than a third of a
/// character at a time.
fn is_subsequence(wanted: &str, name: &str) -> bool {
    let mut characters = name.chars();
    wanted
        .chars()
        .all(|letter| characters.any(|against| against == letter))
}

impl App for Launcher {
    fn message(&mut self, bytes: &[u8]) -> bool {
        if bytes == b"accepted" && self.awaiting_acceptance {
            self.open = false;
            return true;
        }
        if bytes == b"wclr" {
            self.windows.clear();
            self.narrow();
            return true;
        }
        if bytes.len() >= 9 && bytes.starts_with(b"wrow") {
            let slot = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            if slot >= 8 {
                return false;
            }
            let Ok(name) = core::str::from_utf8(&bytes[9..]) else {
                return false;
            };
            self.windows.retain(|window| window.slot != slot);
            self.windows.push(OpenWindow {
                slot,
                name: String::from(name),
                away: bytes[8] != 0,
            });
            self.narrow();
            return true;
        }
        false
    }
    fn draw(&mut self, canvas: &mut Canvas) {
        canvas.set_layer(true);
        canvas.set_text_style(nexus_ui::font::Face::Smooth, true);
        let accent = Colour(self.look.accent.packed());
        let ink = Colour(0x00333c57);
        let quiet = Colour(0x006c7490);
        let grave = Colour(0x00b34e68);

        canvas.fill(canvas.bounds(), Colour(0xb8f6f4ff));
        // The compositor owns the first 32 rows for its title decoration.
        let bounds = canvas.bounds();
        let area = Rect::new(
            PAD,
            32 + PAD,
            bounds.width.saturating_sub(PAD * 2),
            bounds.height.saturating_sub(32 + PAD * 2),
        );
        let field = Rect::new(area.x, area.y, area.width, 42);
        canvas.fill_rounded(field, 12, Colour(0xd8ffffff));
        canvas.fill_rounded(Rect::new(field.x + 12, field.y + 12, 13, 13), 6, quiet);
        canvas.fill_rounded(
            Rect::new(field.x + 14, field.y + 14, 9, 9),
            4,
            Colour(0x00f7f6ff),
        );
        canvas.fill(Rect::new(field.x + 24, field.y + 24, 5, 2), quiet);
        let baseline = field.y + 13;
        // Keep the caret inside the field even after a long query.
        let mut visible = self.typed.as_str();
        while nexus_ui::measure(visible) > field.width.saturating_sub(58) {
            let Some(letter) = visible.chars().next() else {
                break;
            };
            visible = &visible[letter.len_utf8()..];
        }
        let width = canvas.text(field.x + 38, baseline, visible, ink);
        canvas.fill(
            Rect::new(field.x + 38 + width + 1, baseline, 2, nexus_ui::LINE_HEIGHT),
            accent,
        );
        if self.typed.is_empty() {
            canvas.text(
                field.x + 44,
                baseline,
                nexus_i18n::text("launch.hint"),
                quiet,
            );
        }

        // The matches, one per row, with the one under the cursor picked out.
        let row = 38;
        let mut y = field.y + field.height + PAD;
        let rows = area.height.saturating_sub(field.height + PAD + 28) / row;
        let start = self.at.saturating_sub(rows.saturating_sub(1) as usize);
        for (place, index) in self.matching.iter().enumerate().skip(start) {
            if y + row > area.y + area.height.saturating_sub(28) {
                break;
            }
            let (name, dangerous, detail) = if let Some(entry) = ENTRIES.get(*index) {
                (nexus_i18n::text(entry.key), entry.grave, "")
            } else if let Some(window) = self.windows.get(index.saturating_sub(ENTRIES.len())) {
                (
                    window.name.as_str(),
                    false,
                    if window.away {
                        "Restore window"
                    } else {
                        "Open window"
                    },
                )
            } else {
                continue;
            };
            let here = Rect::new(area.x, y, area.width, row);
            if place == self.at {
                canvas.fill_rounded(here, 10, Colour(0x00e2ddfa));
            }
            let colour = if dangerous { grave } else { ink };
            canvas.fill_rounded(Rect::new(here.x + 8, here.y + 7, 24, 24), 7, accent);
            canvas.text(
                here.x + 15,
                here.y + 11,
                if dangerous { "!" } else { ">" },
                Colour(0x00ffffff),
            );
            canvas.text(here.x + 44, here.y + 11, name, colour);
            if !detail.is_empty() && here.width > 300 {
                canvas.text(here.x + here.width - 136, here.y + 11, detail, quiet);
            }
            y += row;
        }

        if self.matching.is_empty() {
            canvas.text(
                area.x + PAD / 2,
                field.y + field.height + PAD + 4,
                nexus_i18n::text("launch.nothing"),
                quiet,
            );
        }

        canvas.text(
            area.x,
            area.y + area.height.saturating_sub(nexus_ui::LINE_HEIGHT),
            nexus_i18n::text("launch.keys"),
            quiet,
        );
    }

    fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Character(letter) => {
                if self.typed.chars().count() >= 128 {
                    return false;
                }
                self.typed.push(letter);
                self.narrow();
                true
            }
            Key::Backspace => {
                if self.typed.pop().is_none() {
                    return false;
                }
                self.narrow();
                true
            }
            Key::Enter => self.choose(),
            Key::Escape => {
                nexus_user::log("launch: closed without starting anything").ok();
                self.open = false;
                true
            }
            Key::Move(Movement::Down) => self.step(1),
            Key::Move(Movement::Up) => self.step(-1),
            Key::Move(Movement::Home) => {
                self.at = 0;
                true
            }
            Key::Move(Movement::End) => {
                self.at = self.matching.len().saturating_sub(1);
                true
            }
            // The language changed, so every name in the list is a different
            // string now and the matching has to be done again.
            Key::Language => {
                self.narrow();
                true
            }
            _ => false,
        }
    }

    fn running(&self) -> bool {
        self.open
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
    nexus_user::log("launch: PANIC").ok();
    nexus_user::exit_with(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_in_order_match() {
        assert!(is_subsequence("tl", "terminal"));
        assert!(is_subsequence("stg", "settings"));
        assert!(is_subsequence("", "anything"));
        assert!(is_subsequence("web", "web"));
    }

    #[test]
    fn letters_out_of_order_do_not() {
        assert!(!is_subsequence("lt", "terminal"));
        assert!(!is_subsequence("xyz", "terminal"));
        assert!(!is_subsequence("terminals", "terminal"));
    }

    #[test]
    fn a_character_is_consumed_once() {
        // "tt" must not match a name with a single t in it. Getting this wrong
        // is the classic subsequence bug: restarting the scan for each letter
        // makes every repeated letter match the same position.
        assert!(!is_subsequence("tt", "terminal"));
        assert!(is_subsequence("tt", "settings"));
    }

    #[test]
    fn matching_is_by_character_and_not_by_byte() {
        // A Japanese name is three bytes a character. A byte-wise version of
        // this would match half a character and find things nobody asked for.
        assert!(is_subsequence("設定", "設定"));
        assert!(!is_subsequence("定設", "設定"));
    }
}
