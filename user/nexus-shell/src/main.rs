//! `shell`: the desktop.
//!
//! The strip along the bottom of the screen, and the one place on the machine
//! where a person can start a program or bring one back. Until this existed the
//! compositor drew that strip itself and decided what a click in it meant, and
//! the only programs that ever ran were the two it started at boot.
//!
//! # Why it is a program and not part of the compositor
//!
//! The compositor owns the display. Everything it does is therefore something
//! nothing else can check: a bug in it is pixels anywhere on screen, and a
//! decision in it is a decision no other program can replace. A dock is
//! *policy* -- what a strip looks like, what a click in it means, which
//! programs are worth launching -- and policy in the one program that owns the
//! framebuffer is the same mistake as policy in the kernel, one layer up.
//!
//! So the desktop is a client. It is handed a surface exactly as any other
//! client is, it draws into memory, and it has no idea where that memory ends
//! up. What it is given that other clients are not is a *list*: which windows
//! exist and what state each is in, and a channel to say what should happen to
//! them. It cannot draw a window, move one, read one, or reach the display.
//!
//! # What it draws
//!
//! A button that starts a program, a tab per window, and a line saying what is
//! running -- in the interface language, which arrives from the kernel through
//! the compositor. Pressing F1 changes the kernel's panel and this strip at the
//! same moment, because both read the same table and neither counts keys.
//!
//! And a clock, and whose machine it is. Both come from the settings file that
//! the first-run wizard wrote, which this program is handed a directory handle
//! to read -- not a path, and not the filesystem: the one directory, read-only.
//! A desktop that could open anything in order to find out what time it is
//! would be a desktop with the run of the disk for the sake of four digits.
//!
//! # Why this one program waits on a clock
//!
//! Everything else here redraws when something changed. A clock has nothing to
//! change *except* time, so this waits on its channel with a deadline: it wakes
//! when the compositor says something, or when the minute turns, whichever
//! comes first. That is one wake a minute on an idle machine, against fifty a
//! second for a program that polled.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString as _};
use core::panic::PanicInfo;

use nexus_config::{key, Settings};
use nexus_look::Look;
use nexus_time::Zone;
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::Handle;

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);

/// Where the strip is mapped. This program's own choice, as every mapping is.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// What the settings file is called inside the directory this is handed.
const SETTINGS_NAME: &str = "settings.txt";

/// And what the updater leaves there, saying what it found and what it did.
///
/// Read and never written. What this program knows about updating is one
/// number; the deciding, the checking and the installing happen in a program
/// that holds the filesystem, and a dock that could install software would be a
/// dock with the run of the disk.
const UPDATES_NAME: &str = "updates.txt";

/// Longest settings file this will read.
///
/// A bound rather than a trust, on a file that is a few hundred bytes when it
/// is right: a desktop that read whatever it found there would be a desktop a
/// large file could stop from starting.
const SETTINGS_MAX: usize = 8 * 1024;

/// How long the strip waits before redrawing with nothing to redraw for.
///
/// A second and not a minute, even though the clock shows minutes. What this
/// bounds is how late the display is when the minute turns, and a strip whose
/// clock changed up to sixty seconds after the minute did would be a clock
/// nobody could set a watch by. The redraw itself is skipped unless the text
/// actually differs, so the cost of being prompt is a compare.
const TICK_MS: u64 = 1_000;

/// What the compositor says, and what this program says back.
///
/// Distinguished by a word rather than by a length: a length is a thing two
/// messages can share by accident.
mod wire {
    /// A frame is on screen.
    pub const SHOWN: &[u8] = b"shown";
    /// Here is every window and what state it is in.
    pub const WINDOWS: &[u8] = b"win";
    /// Somebody clicked in the strip, at these coordinates within it.
    pub const CLICK: &[u8] = b"clk";
    /// The interface language is now this.
    pub const LANGUAGE: &[u8] = b"lng";

    /// This program has drawn.
    pub const DAMAGED: &[u8] = b"damaged";
    /// Bring this window back and give it focus.
    pub const SHOW: &[u8] = b"show";
    /// Put this window away.
    pub const HIDE: &[u8] = b"hide";
    /// Start another program.
    pub const OPEN: &[u8] = b"open";
    /// Start a window that can fetch a page.
    pub const BROWSE: &[u8] = b"web ";
    /// Start a window with a prompt in it.
    pub const TERMINAL: &[u8] = b"term";
    /// Start a window that changes what this machine is.
    pub const SETTINGS: &[u8] = b"sett";
    /// Start a window that shows what there is to install.
    pub const PACKAGES: &[u8] = b"pkgs";
    /// Start a window that shows pictures.
    pub const PICTURES: &[u8] = b"pics";
    /// Start the agent.
    pub const ASSIST: &[u8] = b"asst";
    /// End the session.
    pub const QUIT: &[u8] = b"quit";
    /// Turn the machine off.
    pub const HALT: &[u8] = b"halt";
    /// Restart it.
    pub const RESTART: &[u8] = b"rest";
    /// Put the screen out until somebody touches the machine.
    pub const SLEEP: &[u8] = b"slep";
}

/// What state a window can be in, as the compositor reports it.
mod state {
    /// No such window, or it has ended.
    pub const GONE: u8 = 0;
    /// On screen.
    pub const SHOWN: u8 = 1;
    /// Put away, reachable only by its tab.
    pub const AWAY: u8 = 2;
    /// On screen and holding focus.
    pub const FOCUSED: u8 = 3;
}

/// The most windows this program will ever be told about.
///
/// A fixed ceiling because the strip has a fixed width: a dock that accepted
/// any number of windows would draw tabs one pixel wide and then none at all.
/// The compositor has its own limit and it is the smaller of the two that
/// decides; neither trusts the other's.
const MAX_WINDOWS: usize = 8;

/// How wide the button that starts a program is.
const LAUNCH_WIDTH: u32 = 72;

/// Pixels between things in the strip.
const GAP: u32 = 4;

/// How round the corners of a button or a tab are.
///
/// Five pixels on a thirty-two pixel button: enough to read as rounded at a
/// glance and not so much that a short label sits in a lozenge.
const RADIUS: u32 = 5;

/// Everything this program knows about the desktop.
struct Desktop {
    /// How many window slots the compositor has.
    slots: usize,
    /// What state each is in.
    windows: [u8; MAX_WINDOWS],
    /// Where the strip is, and how large.
    width: u32,
    height: u32,
    /// Whose machine this is, if the settings said.
    ///
    /// `None` on a machine whose settings could not be read, which is drawn as
    /// a strip with no name rather than as an error: a desktop that refused to
    /// appear because it could not find out who owned it would be a machine
    /// nobody could use to repair the file.
    owner: Option<String>,
    /// The timezone the clock is shown in.
    zone: &'static Zone,
    /// What this machine picks things out in.
    ///
    /// Read once at startup and again whenever the settings change, on the same
    /// minute tick as the clock -- because somebody changing the accent expects
    /// the strip to follow the windows, and the windows follow it already.
    look: Look,
    /// The directory the settings and the update record live in, if this
    /// program was given one.
    settings: Option<Handle>,
    /// What the machine is doing, when it is stopping.
    ///
    /// Drawn across the whole strip in place of the buttons, because the
    /// buttons are about to stop meaning anything. This is the last text on the
    /// screen before the machine goes: the compositor darkens everything above
    /// the strip and leaves this alone.
    stopping: Option<String>,
    /// How many updates are waiting, and how many were installed this boot.
    ///
    /// Re-read when the minute turns rather than watched, because the updater
    /// runs once at boot and there is nothing to watch: a file that changes
    /// twice in a machine's life does not need a channel.
    pending: u32,
    installed: u32,
    /// What the clock said when the strip was last drawn.
    ///
    /// Kept so that a wake-up with nothing to show for it costs a string
    /// compare rather than a frame. Empty before the first draw, which no
    /// clock ever reads as, so the first tick always draws.
    clock: String,
}

impl Desktop {
    /// Where the button that starts a program is.
    fn launcher(&self) -> Rect {
        Rect::new(GAP, GAP / 2, LAUNCH_WIDTH, self.height.saturating_sub(GAP))
    }

    /// Where the button that opens a page is.
    ///
    /// Beside the one that starts a program, because they are the same kind of
    /// thing: both ask the compositor for a window, and which program goes in
    /// it is the compositor's decision and not this one's.
    fn web(&self) -> Rect {
        let width = nexus_ui::measure(nexus_i18n::text("shell.web")) + GAP * 4;
        Rect::new(
            GAP * 2 + LAUNCH_WIDTH,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that opens a terminal is.
    fn terminal(&self) -> Rect {
        let web = self.web();
        let width = nexus_ui::measure(nexus_i18n::text("shell.term")) + GAP * 4;
        Rect::new(
            web.x + web.width + GAP,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that opens the settings is.
    fn settings(&self) -> Rect {
        let terminal = self.terminal();
        let width = nexus_ui::measure(nexus_i18n::text("shell.settings")) + GAP * 4;
        Rect::new(
            terminal.x + terminal.width + GAP,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that opens the packages is.
    fn packages(&self) -> Rect {
        let settings = self.settings();
        let width = nexus_ui::measure(nexus_i18n::text("shell.packages")) + GAP * 4;
        Rect::new(
            settings.x + settings.width + GAP,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that opens the pictures is.
    fn pictures(&self) -> Rect {
        let packages = self.packages();
        let width = nexus_ui::measure(nexus_i18n::text("shell.pictures")) + GAP * 4;
        Rect::new(
            packages.x + packages.width + GAP,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that opens the agent is.
    fn assist(&self) -> Rect {
        let pictures = self.pictures();
        let width = nexus_ui::measure(nexus_i18n::text("shell.assist")) + GAP * 4;
        Rect::new(
            pictures.x + pictures.width + GAP,
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that ends the session is, and how wide.
    ///
    /// At the far right, which is where a machine's own controls go on every
    /// desktop anybody has used, and as wide as the word in it -- "Log out" and
    /// "ログアウト" are not the same number of pixels, and a fixed width would
    /// clip one of them.
    fn leave(&self) -> Rect {
        let width = nexus_ui::measure(nexus_i18n::text("shell.leave")) + GAP * 4;
        Rect::new(
            self.width.saturating_sub(width + GAP),
            GAP / 2,
            width,
            self.height.saturating_sub(GAP),
        )
    }

    /// Where the button that turns the machine off is.
    ///
    /// Inside the session controls at the right, in the order a person reads
    /// back from the edge: log out, then off, then restart, then sleep. Off is
    /// nearest the edge of the three because it is the one most often wanted
    /// and the one a person reaches for without looking.
    fn shutdown(&self) -> Rect {
        Self::before(self.leave(), nexus_i18n::text("shell.shutdown"))
    }

    /// Where the button that restarts the machine is.
    fn restart(&self) -> Rect {
        Self::before(self.shutdown(), nexus_i18n::text("shell.restart"))
    }

    /// Where the button that puts the screen out is.
    fn sleep(&self) -> Rect {
        Self::before(self.restart(), nexus_i18n::text("shell.sleep"))
    }

    /// A button of its own width, immediately left of another.
    ///
    /// The width comes from the text, for the same reason `leave` does: these
    /// are words in two languages and a fixed width clips one of them.
    fn before(right: Rect, label: &str) -> Rect {
        let width = nexus_ui::measure(label) + GAP * 3;
        Rect::new(
            right.x.saturating_sub(width + GAP),
            right.y,
            width,
            right.height,
        )
    }

    /// Where a window's tab is.
    ///
    /// By slot, so a tab does not move when another window is put away or
    /// brought back. A tab that shuffled sideways under the pointer would be a
    /// tab somebody clicked and missed.
    fn tab(&self, slot: usize) -> Rect {
        let last = self.assist();
        let left = last.x + last.width + GAP;
        let available = self
            .width
            .saturating_sub(left + GAP)
            .saturating_sub(self.reserved());
        let each = (available / self.slots.max(1) as u32).saturating_sub(GAP);
        Rect::new(
            left + slot as u32 * (each + GAP),
            GAP / 2,
            each,
            self.height.saturating_sub(GAP),
        )
    }

    /// How many windows are put away.
    fn away(&self) -> usize {
        self.windows[..self.slots.min(MAX_WINDOWS)]
            .iter()
            .filter(|state| **state == state::AWAY)
            .count()
    }

    /// Whether anything is running at all.
    fn empty(&self) -> bool {
        self.windows[..self.slots.min(MAX_WINDOWS)]
            .iter()
            .all(|state| *state == state::GONE)
    }

    /// How much of the strip's width the right-hand side keeps for itself.
    ///
    /// Measured rather than guessed, because what goes there is a clock in one
    /// of two formats and a name somebody typed, and a fixed number would be
    /// either too small for a long name or a gap on every machine without one.
    /// Capped at a third: a strip is for windows, and a name long enough to
    /// take half of it is a name that gets cut instead.
    fn reserved(&self) -> u32 {
        // The three power buttons as well as the way out. Without them the
        // tabs are laid out as though the right-hand end were empty, and the
        // last tab is drawn underneath them.
        let mut width = self.leave().width
            + self.shutdown().width
            + self.restart().width
            + self.sleep().width
            + GAP * 5;
        if let Some(text) = self.update_line() {
            width += nexus_ui::measure(&text) + GAP * 3;
        }
        if !self.clock.is_empty() {
            width += nexus_ui::measure(&self.clock) + GAP * 3;
        }
        if let Some(name) = &self.owner {
            width +=
                nexus_ui::measure(&nexus_i18n::format("shell.owner", &[("name", name)])) + GAP * 3;
        }
        width.min(self.width / 2)
    }

    /// What to say about updates, if there is anything to say.
    ///
    /// What is waiting takes precedence over what was installed, because one is
    /// something somebody still has to do and the other is news.
    fn update_line(&self) -> Option<String> {
        if self.pending > 0 {
            return Some(nexus_i18n::format(
                "shell.updates.pending",
                &[("number", &self.pending)],
            ));
        }
        if self.installed > 0 {
            return Some(nexus_i18n::format(
                "shell.updates.installed",
                &[("number", &self.installed)],
            ));
        }
        None
    }

    /// What the update record says now.
    ///
    /// Zero and zero for a machine with no record, which is every machine until
    /// the first check has finished. That reads as "nothing to say", which is
    /// the truth: not knowing is not the same as being up to date, and the
    /// strip shows nothing either way.
    fn updates(&self) -> (u32, u32) {
        let Some(directory) = self.settings else {
            return (0, 0);
        };
        let Some(text) = read_text(directory, UPDATES_NAME) else {
            return (0, 0);
        };
        let number = |name: &str| -> u32 {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(key, _)| key.trim() == name)
                .and_then(|(_, value)| value.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        (number("pending"), number("installed"))
    }

    /// What the clock says now, in the machine's timezone.
    ///
    /// Empty when the machine has no clock to read -- a board with no battery,
    /// or an emulator that was not given one. Drawn as nothing rather than as
    /// zeroes: a strip showing 00:00 all day is worse than a strip showing no
    /// time at all, because the first one looks like an answer.
    fn now(&self) -> String {
        match nexus_user::now() {
            Ok(seconds) => nexus_time::local(seconds as i64, self.zone).to_clock(),
            Err(_) => String::new(),
        }
    }
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
    // Before anything allocates, and both the toolkit and the translations do.
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        failed("shell: FAILED: could not get a heap");
        finish();
    }

    let mut buffer = [0u8; 32];
    // Two: the surface, and the directory the settings live in. The second is
    // optional here and not in the compositor -- this program is the thing that
    // gets it wrong if it is missing, and a desktop that would not start
    // because it had no clock would be a worse machine than one with no clock.
    let mut handles = [Handle(0); 2];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("shell: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles == 0 || received.bytes < 16 {
        failed("shell: FAILED: no surface came with the message");
        finish();
    }

    let settings = (received.handles >= 2).then(|| handles[1]);
    let (owner, zone, language) = match settings {
        Some(directory) => read_settings(directory),
        None => {
            nexus_user::log("shell: started without a settings directory; no clock, no name").ok();
            (None, &nexus_time::ZONES[0], None)
        }
    };

    // Before the first frame, so that nothing is ever drawn in one language and
    // then redrawn in another. The compositor sends the language too -- what it
    // sends is what the *kernel* is showing, which a person can change with F1;
    // this is what the machine was set up as, and it is the starting point.
    if let Some(tag) = &language {
        nexus_i18n::set_locale(tag);
    }

    let mut desktop = Desktop {
        slots: (read_u32(&buffer, 8) as usize).min(MAX_WINDOWS),
        windows: [state::GONE; MAX_WINDOWS],
        width: read_u32(&buffer, 0),
        height: read_u32(&buffer, 4),
        owner,
        zone,
        // The defaults only when there is genuinely nothing to read, which on
        // a machine nobody has configured is the truth.
        look: read_look(settings).unwrap_or_default(),
        settings,
        stopping: None,
        pending: 0,
        installed: 0,
        clock: String::new(),
    };
    let surface = handles[0];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("shell: FAILED: could not map the strip");
        finish();
    };
    // Checked against the mapping rather than against the message, because
    // memory is handed out in whole pages and the two need not agree.
    if desktop.width as usize * desktop.height as usize * 4 > mapped {
        failed("shell: FAILED: the strip is smaller than the size it was given");
        finish();
    }
    if desktop.slots == 0 {
        failed("shell: FAILED: a desktop with no window slots");
        finish();
    }

    nexus_user::log("shell: took the strip along the bottom of the screen").ok();

    // The line the setup test looks for, and the first thing on this machine
    // that says a person's name back to them. Said once, at the start, because
    // it is a fact about the machine and not an event.
    match &desktop.owner {
        Some(name) => nexus_user::log(&alloc::format!(
            "desktop: welcome, {name} ({}, {})",
            nexus_i18n::LOCALES[nexus_i18n::current_index()].tag,
            desktop.zone.name
        ))
        .ok(),
        None => nexus_user::log("desktop: welcome; this machine has no owner on record").ok(),
    };

    run(&mut desktop);
    finish()
}

/// Read the settings file: who owns this machine, and where it is.
///
/// Every failure is the same answer -- no name, UTC, no language -- because
/// every failure means the same thing to this program. It has a strip to draw
/// and it draws it; what it cannot do is report the problem to anyone, since
/// the thing that would show a message is itself.
fn read_look(directory: Option<Handle>) -> Option<Look> {
    // `None` means "could not read it", not "it says the defaults".
    //
    // Replacing that file means removing the name and making it again, because
    // there is no truncate, so there is a short window in which the name does
    // not exist. A strip that answered that window with the defaults would
    // repaint itself in somebody else's colours for one frame, and -- worse --
    // would then treat the defaults as the current state and not notice the
    // colour that was actually written. The wallpaper did exactly that.
    let directory = directory?;
    let text = read_text(directory, SETTINGS_NAME)?;
    if text.is_empty() {
        return None;
    }
    Some(Look::parse(&text))
}

fn read_settings(directory: Handle) -> (Option<String>, &'static Zone, Option<String>) {
    let utc = &nexus_time::ZONES[0];

    let Some(text) = read_text(directory, SETTINGS_NAME) else {
        nexus_user::log("shell: no settings to read; showing the strip without a name").ok();
        return (None, utc, None);
    };
    let settings = Settings::parse(&text);

    let owner = settings.get(key::USER_NAME).map(String::from);
    // By name and not by index: an index is a position in a table this program
    // did not build, and a table that gained an entry would silently move every
    // machine's clock.
    let zone = settings
        .get(key::TIMEZONE)
        .and_then(nexus_time::zone_by_name)
        .unwrap_or(utc);
    let language = settings
        .get(key::LANGUAGE)
        .filter(|tag| nexus_i18n::LOCALES.iter().any(|locale| locale.tag == *tag))
        .map(String::from);

    (owner, zone, language)
}

/// A whole text file out of a directory, if it is there and readable.
///
/// Every failure is the same answer, because every failure means the same thing
/// to this program: there is nothing to show. It has a strip to draw either
/// way, and what it cannot do is report the problem to anyone -- the thing that
/// would show a message is itself.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(SETTINGS_MAX);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
}

/// Draw, say so, and act on whatever comes back.
///
/// Event-driven with one exception. This program redraws when something it
/// shows has changed, and the clock is something it shows: so the wait has a
/// deadline, and a wake-up with nothing behind it re-reads the clock and
/// redraws only if the text differs. On an idle machine that is one frame a
/// minute -- the minute turning -- and not one a second, because fifty-nine of
/// every sixty wake-ups find the same four digits and go back to sleep.
fn run(desktop: &mut Desktop) {
    let mut drawn = 0u32;
    let mut launched = 0u32;
    let mut acted = 0u32;
    let mut announced = false;
    let mut ticked = false;
    let mut said_updates = false;
    let mut said_missed = false;
    // What was last said about where the tabs are, so it is said again only
    // when it is no longer true.
    let mut said_layout = (0u32, 0usize);

    // The channel, watched rather than read directly, because a blocking read
    // is a read with no deadline and the clock needs one.
    let Ok(set) = nexus_user::wait_set() else {
        failed("shell: FAILED: could not make a wait set");
        return;
    };
    const COMPOSITOR_SAID: u64 = 1;
    if nexus_user::watch(set, COMPOSITOR, COMPOSITOR_SAID).is_err() {
        failed("shell: FAILED: could not watch the compositor");
        return;
    }

    // Whether the strip on screen is out of date, and whether the frame that
    // would replace it has been acknowledged. Both, because they are different
    // questions: a frame sent and not yet shown must not be sent again, and a
    // change that arrives while one is in flight must not be forgotten.
    let mut stale = true;
    let mut in_flight = false;

    // Bounded, so a desktop whose compositor stops answering cannot spin. The
    // bound is a backstop and not a schedule: at one frame a minute this is
    // most of a day, and every frame it does draw is one somebody asked for.
    for _ in 0..65_536 {
        if stale && !in_flight {
            desktop.clock = desktop.now();
            draw(desktop);
            if nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).is_err() {
                break;
            }
            drawn += 1;
            stale = false;
            in_flight = true;
        }

        let mut keys = [0u64; 2];
        let Ok(ready) = nexus_user::wait_any_until(set, &mut keys, TICK_MS) else {
            break;
        };
        if ready == 0 {
            // Nothing was said. The only thing that can have changed is the
            // time, so ask it, and draw only if the answer is different.
            // The minute turning is also when the update record is looked at
            // again. Once a minute rather than once a second, because the
            // updater runs at boot and this is a file, not an event -- and a
            // strip that opened a file fifty times a minute to find the same
            // number would be the polling the rest of this avoids.
            // Left alone when it could not be read.
            let now = read_look(desktop.settings).unwrap_or(desktop.look);
            if now != desktop.look {
                desktop.look = now;
                stale = true;
            }

            let seen = desktop.updates();
            let changed = seen != (desktop.pending, desktop.installed);
            if changed {
                desktop.pending = seen.0;
                desktop.installed = seen.1;
                stale = true;
            }
            // Said on the first look and again whenever it changes, rather than
            // only on the first *change*. The updater runs while this window is
            // already up, so the first look is often at a record it has not
            // written yet -- and a line that only appeared when the number
            // moved would be missing on a machine that had nothing to install.
            if changed || !said_updates {
                said_updates = true;
                nexus_user::log(&alloc::format!(
                    "desktop: {} update(s) waiting, {} installed this boot",
                    desktop.pending,
                    desktop.installed
                ))
                .ok();
            }
            if desktop.now() != desktop.clock {
                stale = true;
                // Said once, the first time the clock moves on its own. It is
                // the only evidence from outside this program that the timed
                // wait works: a deadline that never expired would leave this
                // line missing and the strip showing the minute it started in.
                if !ticked {
                    ticked = true;
                    nexus_user::log("shell: the clock moved on without anything being said").ok();
                }
            }
            continue;
        }

        // One message per wake-up, not a drain. The set is level-triggered:
        // whatever is still queued makes it ready again on the next turn round
        // this loop, and taking them one at a time means the clock is checked
        // between them rather than after a burst.
        let mut message = [0u8; 64];
        let mut none = [Handle(0); 1];
        let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut none) else {
            report(drawn);
            return;
        };
        let message = &message[..received.bytes];

        // Where the tabs are, whenever that changes.
        //
        // Written for the tests, and worth having for that reason alone: a
        // test that clicks a tab has to know where one is, and every time a
        // button was added to the left of them every such test moved. Reading
        // the position out of the machine instead of guessing it means a sixth
        // button costs nothing.
        if (desktop.width, desktop.slots) != said_layout {
            said_layout = (desktop.width, desktop.slots);
            let first = desktop.tab(0);
            nexus_user::log(&alloc::format!(
                "shell: {} tabs start at {} and are {} wide, {} apart",
                desktop.slots,
                first.x,
                first.width,
                first.width + GAP,
            ))
            .ok();
        }

        if message == wire::SHOWN {
            in_flight = false;
            continue;
        }

        if message.starts_with(wire::WINDOWS) && message.len() >= 4 {
            let count = (message.len() - 3).min(MAX_WINDOWS);
            desktop.windows = [state::GONE; MAX_WINDOWS];
            desktop.windows[..count].copy_from_slice(&message[3..3 + count]);
            stale = true;
            continue;
        }

        if message.starts_with(wire::LANGUAGE) && message.len() >= 7 {
            let index = read_u32(message, 3) as usize;
            // Set by tag rather than by index, because an index is a position
            // in a table this program did not build. The tag is what both sides
            // actually agree on.
            if let Some(locale) = nexus_i18n::LOCALES.get(index) {
                nexus_i18n::set_locale(locale.tag);
                if !announced {
                    announced = true;
                    nexus_user::log("shell: drew its strip in the language it was told").ok();
                }
            }
            stale = true;
            continue;
        }

        if message.starts_with(wire::CLICK) && message.len() >= 11 {
            let x = read_u32(message, 3);
            let y = read_u32(message, 7);
            let acted_on = clicked(desktop, x, y);
            if acted_on.is_none() && !said_missed {
                // Once, and with the numbers. A press that lands on nothing is
                // ordinary -- there is space between the tabs -- but a press
                // that lands on nothing *when somebody meant it to land on
                // something* is the hardest thing here to work out afterwards,
                // because the only evidence is a click that did not happen.
                said_missed = true;
                nexus_user::log(&alloc::format!(
                    "shell: a press at ({x}, {y}) in a {}x{} strip landed on nothing;                      the launcher is at {}..{}, tabs start at {} and are {} wide",
                    desktop.width,
                    desktop.height,
                    desktop.launcher().x,
                    desktop.launcher().x + desktop.launcher().width,
                    desktop.tab(0).x,
                    desktop.tab(0).width
                ))
                .ok();
            }
            if let Some(sent) = acted_on {
                // Logged the first time rather than counted up and reported at
                // the end: this program ends when the compositor does, and a
                // claim that only appears at shutdown is a claim nothing can
                // check while the machine is running.
                if acted == 0 {
                    nexus_user::log("shell: turned a click in the strip into a command").ok();
                }
                acted += 1;
                if sent {
                    if launched == 0 {
                        nexus_user::log("shell: asked for a program to be started").ok();
                    }
                    launched += 1;
                }
            }
            // No repaint asked for: the compositor answers with a new list, and
            // drawing a state this program merely expected would be a strip
            // that lies for as long as the compositor takes to disagree.
            continue;
        }
    }

    report(drawn);
}

/// Decide what a click in the strip meant, and say so.
///
/// Returns `None` if it landed on nothing, and otherwise whether it started a
/// program. Nothing is changed here: this program says what should happen and
/// the compositor decides whether it does.
fn clicked(desktop: &mut Desktop, x: u32, y: u32) -> Option<bool> {
    if desktop.terminal().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::TERMINAL, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.assist().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::ASSIST, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.pictures().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::PICTURES, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.packages().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::PACKAGES, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.settings().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::SETTINGS, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.web().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::BROWSE, &[])
            .ok()
            .map(|_| true);
    }

    if desktop.leave().contains(x, y) {
        nexus_user::log("shell: somebody pressed the button that ends the session").ok();
        return nexus_user::send(COMPOSITOR, wire::QUIT, &[])
            .ok()
            .map(|_| false);
    }

    // Off and restart both stop the machine, so both say what is happening in
    // the strip *before* the message goes. The compositor darkens everything
    // above this strip and leaves it alone, so this line is the last thing on
    // the screen -- which is the whole reason it is written here, by the one
    // program in the session that draws text.
    if desktop.shutdown().contains(x, y) {
        nexus_user::log("shell: somebody pressed the button that turns the machine off").ok();
        desktop.stopping = Some(nexus_i18n::text("shell.shuttingdown").to_string());
        draw(desktop);
        nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).ok();
        return nexus_user::send(COMPOSITOR, wire::HALT, &[])
            .ok()
            .map(|_| false);
    }

    if desktop.restart().contains(x, y) {
        nexus_user::log("shell: somebody pressed the button that restarts the machine").ok();
        desktop.stopping = Some(nexus_i18n::text("shell.restarting").to_string());
        draw(desktop);
        nexus_user::send(COMPOSITOR, wire::DAMAGED, &[]).ok();
        return nexus_user::send(COMPOSITOR, wire::RESTART, &[])
            .ok()
            .map(|_| false);
    }

    if desktop.sleep().contains(x, y) {
        nexus_user::log("shell: somebody pressed the button that puts the screen out").ok();
        return nexus_user::send(COMPOSITOR, wire::SLEEP, &[])
            .ok()
            .map(|_| false);
    }

    if desktop.launcher().contains(x, y) {
        return nexus_user::send(COMPOSITOR, wire::OPEN, &[])
            .ok()
            .map(|_| true);
    }

    for slot in 0..desktop.slots {
        if !desktop.tab(slot).contains(x, y) {
            continue;
        }
        let command = match desktop.windows[slot] {
            state::AWAY => wire::SHOW,
            state::SHOWN | state::FOCUSED => wire::HIDE,
            // A tab for a window that has ended is a tab that does nothing.
            // Left drawn, because a gap where a tab was is a strip that
            // reshuffles under the pointer.
            _ => return None,
        };
        let mut message = [0u8; 8];
        message[..command.len()].copy_from_slice(command);
        message[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
        return nexus_user::send(COMPOSITOR, &message, &[])
            .ok()
            .map(|_| false);
    }

    None
}

/// Draw the whole strip.
fn draw(desktop: &Desktop) {
    // SAFETY: the strip is mapped here, writable, and at least
    // `width * height * 4` bytes -- checked against the mapping before the
    // first frame.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, desktop.width, desktop.height) };

    // The face and the blending a person chose, from the same settings file the
    // colours come from.
    canvas.set_text_style(
        nexus_ui::font::Face::parse(Some(desktop.look.font.name())),
        desktop.look.smooth,
    );

    // The strip is a darker version of the background's bottom, so a machine
    // with a warm wallpaper does not have a cold bar under it; everything that
    // can be pressed is the accent.
    let ground = Colour(
        desktop
            .look
            .bottom
            .towards(nexus_look::Colour::new(0, 0, 0), 90)
            .packed(),
    );
    let accent = Colour(desktop.look.accent.packed());
    let ink = Colour::rgb(0xC8, 0xD4, 0xE8);

    canvas.fill(canvas.bounds(), ground);

    // A machine that is stopping has nothing else worth saying. The buttons are
    // about to stop meaning anything, so they are not drawn -- a strip full of
    // things to press, over a screen that has gone dark, invites somebody to
    // press one.
    if let Some(what) = &desktop.stopping {
        canvas.text_centred(canvas.bounds(), what, Colour::rgb(0xE6, 0xEC, 0xF5));
        return;
    }

    // The launcher, which is the only thing on this machine that starts a
    // program by being pressed.
    let launcher = desktop.launcher();
    canvas.panel(
        launcher,
        RADIUS,
        accent.blend(Colour::rgb(0, 0, 0), 120),
        accent,
    );
    canvas.text_centred(
        launcher,
        nexus_i18n::text("shell.launch"),
        Colour::rgb(0xF0, 0xF4, 0xFF),
    );

    // The button that opens a page, next to the one that starts a program.
    let web = desktop.web();
    canvas.panel(
        web,
        RADIUS,
        accent.blend(Colour::rgb(0, 0, 0), 150),
        accent.blend(ground, 90),
    );
    canvas.text_centred(
        web,
        nexus_i18n::text("shell.web"),
        Colour::rgb(0xE8, 0xF0, 0xFF),
    );

    // And the one that opens a prompt.
    let terminal = desktop.terminal();
    canvas.panel(
        terminal,
        RADIUS,
        Colour::rgb(0x10, 0x20, 0x18),
        Colour::rgb(0x4E, 0x8E, 0x5C),
    );
    canvas.text_centred(
        terminal,
        nexus_i18n::text("shell.term"),
        Colour::rgb(0xD8, 0xF0, 0xDC),
    );

    // And the one that opens the machine's own settings.
    let settings = desktop.settings();
    canvas.panel(
        settings,
        RADIUS,
        Colour::rgb(0x22, 0x1C, 0x30),
        Colour::rgb(0x7A, 0x6C, 0xA8),
    );
    canvas.text_centred(
        settings,
        nexus_i18n::text("shell.settings"),
        Colour::rgb(0xE4, 0xDE, 0xF4),
    );

    // And the one that opens what there is to install.
    let packages = desktop.packages();
    canvas.panel(
        packages,
        RADIUS,
        Colour::rgb(0x2A, 0x20, 0x14),
        Colour::rgb(0xA8, 0x8C, 0x54),
    );
    canvas.text_centred(
        packages,
        nexus_i18n::text("shell.packages"),
        Colour::rgb(0xF4, 0xEA, 0xD4),
    );

    // And the one that opens the pictures.
    let pictures = desktop.pictures();
    canvas.panel(
        pictures,
        RADIUS,
        Colour::rgb(0x14, 0x26, 0x26),
        Colour::rgb(0x54, 0x9C, 0x9C),
    );
    canvas.text_centred(
        pictures,
        nexus_i18n::text("shell.pictures"),
        Colour::rgb(0xD8, 0xF0, 0xF0),
    );

    // And the one that opens the agent.
    let assist = desktop.assist();
    canvas.panel(
        assist,
        RADIUS,
        Colour::rgb(0x2A, 0x14, 0x22),
        Colour::rgb(0xB0, 0x68, 0x90),
    );
    canvas.text_centred(
        assist,
        nexus_i18n::text("shell.assist"),
        Colour::rgb(0xF4, 0xDC, 0xE8),
    );

    for slot in 0..desktop.slots {
        let tab = desktop.tab(slot);
        if tab.width == 0 {
            break;
        }
        let state = desktop.windows[slot];
        if state == state::GONE {
            // An empty slot is the ground with an edge around it, which is not
            // the same thing as the strip being shorter.
            canvas.panel(tab, RADIUS, ground, ground.blend(ink, 40));
            continue;
        }

        let colour = match state {
            state::AWAY => accent.blend(Colour::rgb(0, 0, 0), 90),
            state::FOCUSED => Colour::rgb(0x19, 0x33, 0x50),
            _ => Colour::rgb(0x11, 0x1E, 0x30),
        };
        // The focused window's tab is the only one with the accent around it,
        // which is what makes it findable without reading any of them.
        let edge = if state == state::FOCUSED {
            accent
        } else {
            colour
        };
        canvas.panel(tab, RADIUS, colour, edge);
        let label = nexus_i18n::format("shell.window", &[("number", &(slot + 1))]);
        canvas.text_centred(tab, &label, ink);
    }

    // The way out, at the very edge, drawn in a colour nothing else uses: it is
    // the one control here that cannot be undone by pressing it again.
    let leave = desktop.leave();
    canvas.fill(leave, Colour::rgb(0x3A, 0x14, 0x18));
    canvas.outline(leave, 1, Colour::rgb(0xB0, 0x48, 0x50));
    canvas.text_centred(
        leave,
        nexus_i18n::text("shell.leave"),
        Colour::rgb(0xF0, 0xD8, 0xD8),
    );

    // The three that stop the machine, quieter than the way out: they are
    // reversible in a way it is not -- a machine that has been turned off can
    // be turned on again, and a session that has ended has taken the windows
    // with it.
    for (rect, label) in [
        (desktop.shutdown(), "shell.shutdown"),
        (desktop.restart(), "shell.restart"),
        (desktop.sleep(), "shell.sleep"),
    ] {
        canvas.fill(rect, Colour::rgb(0x18, 0x22, 0x36));
        canvas.outline(rect, 1, Colour::rgb(0x38, 0x4A, 0x68));
        canvas.text_centred(rect, nexus_i18n::text(label), Colour::rgb(0xC8, 0xD4, 0xE6));
    }

    // The right-hand end, in the order a person reads back from the edge: the
    // clock, then whose machine this is, then what is running. Each is drawn
    // only if it fits in what is left after the one outside it, and the whole
    // lot is cut at the last tab -- text drawn over a tab is a strip that
    // cannot be read.
    let last = desktop.tab(desktop.slots.saturating_sub(1));
    let floor = last.x + last.width;
    let mut right = desktop.sleep().x.saturating_sub(GAP * 2);
    let baseline = GAP / 2 + 2;

    if !desktop.clock.is_empty() {
        let width = nexus_ui::measure(&desktop.clock);
        if right.saturating_sub(width) > floor {
            right -= width;
            canvas.text(right, baseline, &desktop.clock, ink);
            right = right.saturating_sub(GAP * 3);
        }
    }

    if let Some(text) = desktop.update_line() {
        let width = nexus_ui::measure(&text);
        if right.saturating_sub(width) > floor {
            right -= width;
            canvas.text(right, baseline, &text, accent);
            right = right.saturating_sub(GAP * 3);
        }
    }

    if let Some(name) = &desktop.owner {
        let label = nexus_i18n::format("shell.owner", &[("name", name)]);
        let width = nexus_ui::measure(&label);
        if right.saturating_sub(width) > floor {
            right -= width;
            canvas.text(right, baseline, &label, ink.blend(ground, 60));
            right = right.saturating_sub(GAP * 3);
        }
    }

    let away = desktop.away();
    let status: String = if desktop.empty() {
        nexus_i18n::text("shell.none").to_string()
    } else if away > 0 {
        nexus_i18n::format("shell.hidden", &[("number", &away)])
    } else {
        return;
    };
    let width = nexus_ui::measure(&status);
    if right.saturating_sub(width) > floor {
        canvas.text(right - width, baseline, &status, ink.blend(ground, 90));
    }
}

/// Say what this program did, at the end.
///
/// Only the drawing. What it was asked to do is logged as it happens, because
/// this program ends when the compositor does and a claim that appears only at
/// shutdown is a claim nothing can check while the machine is running.
fn report(drawn: u32) {
    if drawn > 0 {
        nexus_user::log("shell: drew the desktop strip for as long as it was asked to").ok();
    }
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
    nexus_user::log("shell: PANIC").ok();
    nexus_user::exit_with(2)
}
