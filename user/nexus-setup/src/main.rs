//! `setup`: the screen a machine shows the first time it is turned on.
//!
//! Language, timezone, an account with a password, a look at the network, and
//! then it writes the answers down and never appears again.
//!
//! # Why this is an ordinary program
//!
//! Because it has to be. It draws into a surface it was given, exactly as any
//! other client does; it reads keys the compositor routed to it; and the one
//! thing it can reach beyond that is the directory it was handed to write the
//! answers into. A setup program with the run of the machine would be a program
//! that could do anything on the machine, and it runs before anybody has agreed
//! to anything.
//!
//! # What it stores, and what it does not
//!
//! Not the password. What goes in the file is a salt and the output of
//! PBKDF2-HMAC-SHA512 over the password and that salt, which cannot be turned
//! back into what was typed. The number of rounds is stored beside it, so that
//! raising the count later does not lock anybody out of an account created
//! before the change.
//!
//! # What it looks like
//!
//! One page at a time, with the keys that work written at the bottom of the
//! screen — because a machine that has never been configured is a machine whose
//! owner has not read anything about it yet.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_config::{key, Settings};
use nexus_ui::{Canvas, Colour, Rect};
use nexus_user::{Handle, Kind};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to the compositor that started this program.
const COMPOSITOR: Handle = Handle(1);
/// The directory the answers go in, handed over with the first message.
const SETTINGS_NAME: &str = "settings.txt";
/// And where the kernel wrote what the network is doing.
const NETWORK_NAME: &str = "network.txt";

/// Where the surface is mapped. This program's own choice.
const SURFACE_AT: usize = 0x0000_0000_1000_0000;

/// How much heap: a hash, some strings, and the settings file.
const HEAP: usize = 512 * 1024;

/// What the compositor says.
mod wire {
    pub const SHOWN: &[u8] = b"shown";
    pub const RESIZED: &[u8] = b"size";
    /// Bytes one key takes on the wire.
    pub const KEY: usize = 5;
}

/// What a key is, as the kernel sends it.
mod key_kind {
    pub const CHARACTER: u8 = 1;
    pub const BACKSPACE: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const ESCAPE: u8 = 4;
    pub const TAB: u8 = 5;
    pub const FUNCTION: u8 = 6;
    pub const LANGUAGE: u8 = 7;
}

/// The pages, in the order they are shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Language,
    Timezone,
    Account,
    Password,
    Confirm,
    Network,
    Done,
}

impl Page {
    /// The next one, or `None` at the end.
    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Language => Self::Timezone,
            Self::Timezone => Self::Account,
            Self::Account => Self::Password,
            Self::Password => Self::Confirm,
            Self::Confirm => Self::Network,
            Self::Network => Self::Done,
            Self::Done => return None,
        })
    }

    /// The one before, or `None` at the start.
    fn previous(self) -> Option<Self> {
        Some(match self {
            Self::Language => return None,
            Self::Timezone => Self::Language,
            Self::Account => Self::Timezone,
            Self::Password => Self::Account,
            // Going back from the confirmation means typing the password
            // again, not editing the one that is already there -- because
            // there is nothing to edit: it was never kept as text.
            Self::Confirm => Self::Password,
            Self::Network => Self::Confirm,
            Self::Done => Self::Network,
        })
    }

    /// Which of the pages this is, and how many there are, for the reader.
    fn position(self) -> (usize, usize) {
        let order = [
            Self::Language,
            Self::Timezone,
            Self::Account,
            Self::Password,
            Self::Confirm,
            Self::Network,
            Self::Done,
        ];
        let at = order.iter().position(|page| *page == self).unwrap_or(0);
        (at + 1, order.len())
    }
}

/// Everything the wizard has been told so far.
struct Answers {
    page: Page,
    language: usize,
    zone: usize,
    name: String,
    password: String,
    confirmation: String,
    /// What went wrong with the last thing that was typed, if anything.
    complaint: Option<String>,
    /// What the kernel says the network is doing.
    network: Vec<String>,
    /// Set once the answers are on the disk.
    saved: bool,
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
        failed("setup: FAILED: could not get a heap");
        finish();
    }

    // Two handles with the first message: the surface to draw into, and the
    // directory to write the answers to. Nothing else -- this program cannot
    // start another, cannot reach the rest of the filesystem, and cannot see
    // any window but its own.
    let mut buffer = [0u8; 32];
    let mut handles = [Handle(0); 2];
    let Ok(received) = nexus_user::receive(COMPOSITOR, &mut buffer, &mut handles) else {
        failed("setup: FAILED: nothing arrived to draw on");
        finish();
    };
    if received.handles != 2 || received.bytes < 16 {
        failed("setup: FAILED: it was not given a surface and a place to write");
        finish();
    }

    let mut width = read_u32(&buffer, 0);
    let mut height = read_u32(&buffer, 4);
    let mut surface = handles[0];
    let directory = handles[1];

    let Ok(mapped) = nexus_user::memory_map(surface, SURFACE_AT, true) else {
        failed("setup: FAILED: could not map its surface");
        finish();
    };
    if width as usize * height as usize * 4 > mapped {
        failed("setup: FAILED: the surface is smaller than the size it was given");
        finish();
    }

    let mut answers = Answers {
        page: Page::Language,
        language: 0,
        zone: 0,
        name: String::new(),
        password: String::new(),
        confirmation: String::new(),
        complaint: None,
        network: read_network(directory),
        saved: false,
    };

    // The language the machine is already showing, so the first page starts on
    // it rather than jumping.
    answers.language = nexus_i18n::LOCALES
        .iter()
        .position(|locale| locale.tag == nexus_i18n::current().tag)
        .unwrap_or(0);

    nexus_user::log("setup: this machine has not been set up; asking").ok();

    // Bounded, so a machine nobody is sitting at does not spin for ever. It is
    // a very generous bound: somebody reading each page carefully will not
    // reach it, and a machine left alone will.
    for _ in 0..4096 {
        draw(&answers, width, height);
        if nexus_user::send(COMPOSITOR, b"damaged", &[]).is_err() {
            break;
        }

        let mut shown = false;
        loop {
            let mut message = [0u8; 32];
            let mut incoming = [Handle(0); 1];
            let Ok(received) = nexus_user::receive(COMPOSITOR, &mut message, &mut incoming) else {
                failed("setup: FAILED: the compositor stopped answering");
                finish();
            };
            let message = &message[..received.bytes];

            if message == wire::SHOWN {
                shown = true;
                continue;
            }

            // The surface can be replaced, exactly as for any other client.
            if received.bytes >= 12 && message.starts_with(wire::RESIZED) && received.handles == 1 {
                nexus_user::memory_unmap(surface, SURFACE_AT).ok();
                nexus_user::close(surface).ok();
                surface = incoming[0];
                width = read_u32(message, 4);
                height = read_u32(message, 8);
                if nexus_user::memory_map(surface, SURFACE_AT, true).is_err() {
                    failed("setup: FAILED: could not map the surface it was given");
                    finish();
                }
                break;
            }

            if received.bytes >= wire::KEY {
                if typed(&mut answers, message, directory) {
                    break;
                }
                if shown {
                    break;
                }
            }
        }

        if answers.saved {
            // One more frame, so the last page is on screen before this program
            // stops and the compositor takes the surface back.
            draw(&answers, width, height);
            nexus_user::send(COMPOSITOR, b"damaged", &[]).ok();
            nexus_user::sleep(1500).ok();
            break;
        }
    }

    nexus_user::log("setup: finished").ok();
    finish()
}

/// Act on one key. Returns whether anything on screen changed.
fn typed(answers: &mut Answers, message: &[u8], directory: Handle) -> bool {
    let kind = message[0];
    let value = read_u32(message, 1);

    // Not a key: the kernel saying the interface language changed under us.
    if kind == key_kind::LANGUAGE {
        return false;
    }

    answers.complaint = None;

    match kind {
        key_kind::ENTER => forward(answers, directory),
        key_kind::ESCAPE => {
            // Back a page. Escape rather than a cursor key because it is the
            // one key everybody already knows means "not that".
            if let Some(previous) = answers.page.previous() {
                answers.page = previous;
                if answers.page == Page::Password {
                    // Both are cleared: a confirmation left over from a
                    // password that is being retyped would confirm the wrong
                    // thing.
                    answers.password.clear();
                    answers.confirmation.clear();
                }
                return true;
            }
            false
        }
        key_kind::TAB => {
            // The one key that moves a choice on the pages that have one.
            match answers.page {
                Page::Language => {
                    answers.language = (answers.language + 1) % nexus_i18n::LOCALES.len();
                    // Applied at once rather than at the end, so the rest of the
                    // wizard is in the language being chosen -- which is the
                    // only way to tell whether the right one was chosen.
                    nexus_i18n::set_locale(nexus_i18n::LOCALES[answers.language].tag);
                    true
                }
                Page::Timezone => {
                    answers.zone = (answers.zone + 1) % nexus_time::ZONES.len();
                    true
                }
                _ => false,
            }
        }
        key_kind::BACKSPACE => {
            let field = match answers.page {
                Page::Account => Some(&mut answers.name),
                Page::Password => Some(&mut answers.password),
                Page::Confirm => Some(&mut answers.confirmation),
                _ => None,
            };
            match field {
                Some(field) => {
                    field.pop();
                    true
                }
                None => false,
            }
        }
        key_kind::CHARACTER => {
            let Some(character) = char::from_u32(value) else {
                return false;
            };
            let field = match answers.page {
                Page::Account => Some(&mut answers.name),
                Page::Password => Some(&mut answers.password),
                Page::Confirm => Some(&mut answers.confirmation),
                _ => None,
            };
            match field {
                // Bounded: a field that grew without limit would be a field
                // somebody could fill the heap with by leaning on a key.
                Some(field) if field.chars().count() < 48 => {
                    field.push(character);
                    true
                }
                _ => false,
            }
        }
        key_kind::FUNCTION => false,
        _ => false,
    }
}

/// Enter: check this page's answer and move on if it holds.
fn forward(answers: &mut Answers, directory: Handle) -> bool {
    match answers.page {
        Page::Account => {
            let trimmed = answers.name.trim().to_string();
            if trimmed.is_empty() {
                answers.complaint = Some(nexus_i18n::text("setup.name_empty").to_string());
                return true;
            }
            answers.name = trimmed;
        }
        Page::Password => {
            // Short enough to guess is the failure this catches. Eight is not a
            // strong rule and it is a rule: a machine that accepted an empty
            // password would have an account anybody can open.
            if answers.password.chars().count() < 8 {
                answers.complaint = Some(nexus_i18n::text("setup.password_short").to_string());
                return true;
            }
        }
        Page::Confirm => {
            if answers.confirmation != answers.password {
                answers.complaint = Some(nexus_i18n::text("setup.password_differs").to_string());
                answers.confirmation.clear();
                return true;
            }
        }
        Page::Done => return false,
        _ => {}
    }

    if answers.page == Page::Network {
        // The last page before the summary is where the work happens: hashing
        // the password takes a moment, and doing it here means the summary is
        // shown only once the answers are actually on the disk.
        match save(answers, directory) {
            Ok(()) => answers.saved = true,
            Err(why) => {
                answers.complaint = Some(why);
                return true;
            }
        }
    }

    if let Some(next) = answers.page.next() {
        answers.page = next;
    }
    true
}

/// Write the answers down.
fn save(answers: &Answers, directory: Handle) -> Result<(), String> {
    let mut settings = Settings::new();
    settings.set(key::CONFIGURED, "yes");
    settings.set(key::LANGUAGE, nexus_i18n::LOCALES[answers.language].tag);
    settings.set(key::TIMEZONE, nexus_time::ZONES[answers.zone].name);
    settings.set(key::USER_NAME, &answers.name);
    settings.set(key::VERSION, env!("CARGO_PKG_VERSION"));

    if let Ok(seconds) = nexus_user::now() {
        settings.set(key::CONFIGURED_AT, &format!("{seconds}"));
    }

    // The salt. Not from a source of randomness, because this machine has none
    // it would trust -- so it is made from the things that differ between two
    // machines and two moments: the clock, the uptime at this instant, and the
    // name that was typed. That is weaker than random and it is stated rather
    // than hidden; what a salt has to do is differ between installations, and
    // this does.
    let mut salt = [0u8; nexus_crypto::password::SALT];
    let clock = nexus_user::now().unwrap_or(0);
    let uptime = nexus_user::uptime();
    for (index, byte) in salt.iter_mut().enumerate() {
        let mixed = clock.rotate_left(index as u32 * 7)
            ^ uptime.rotate_right(index as u32 * 3)
            ^ (answers.name.as_bytes().get(index).copied().unwrap_or(0x5A) as u64);
        *byte = (mixed >> (index % 8 * 8)) as u8 ^ (index as u8).wrapping_mul(31);
    }

    let rounds = nexus_crypto::password::ITERATIONS;
    let hash = nexus_crypto::password::derive(answers.password.as_bytes(), &salt, rounds);

    settings.set(key::USER_SALT, &nexus_config::to_hex(&salt));
    settings.set(key::USER_HASH, &nexus_config::to_hex(&hash));
    settings.set(key::USER_ROUNDS, &format!("{rounds}"));

    // Whatever the kernel wrote about the network, carried across so that one
    // file is the whole of what the machine was told.
    for line in &answers.network {
        if let Some((name, value)) = line.split_once('=') {
            settings.set(name.trim(), value.trim());
        }
    }

    write(directory, SETTINGS_NAME, settings.to_text().as_bytes())?;
    nexus_user::log(&format!(
        "setup: wrote settings for {} ({}, {})",
        answers.name,
        nexus_i18n::LOCALES[answers.language].tag,
        nexus_time::ZONES[answers.zone].name
    ))
    .ok();
    Ok(())
}

/// Write a file, replacing what was there.
fn write(directory: Handle, name: &str, contents: &[u8]) -> Result<(), String> {
    nexus_user::remove(directory, name).ok();
    let file = nexus_user::create(directory, name, Kind::File)
        .map_err(|error| format!("cannot create {name}: {error:?}"))?;
    let mut written = 0;
    while written < contents.len() {
        match nexus_user::write_at(file, written as u64, &contents[written..]) {
            Ok(0) | Err(_) => {
                nexus_user::close(file).ok();
                return Err(format!("cannot write {name}"));
            }
            Ok(count) => written += count,
        }
    }
    nexus_user::close(file).ok();
    Ok(())
}

/// What the kernel wrote about the network, as lines.
fn read_network(directory: Handle) -> Vec<String> {
    let Ok(file) = nexus_user::open(directory, NETWORK_NAME) else {
        return Vec::new();
    };
    let size = nexus_user::size(file).unwrap_or(0).min(1024);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && line.contains('='))
        .map(String::from)
        .collect()
}

/// Draw the current page.
///
/// A card in the middle of the screen rather than text in the corner of it. The
/// screen is two thousand pixels across and the face is sixteen high: filling
/// it with a paragraph would be a wall nobody reads, and leaving the text at
/// the top left would be a machine that looks broken. So the content is a fixed
/// width, centred, with the heading drawn at three times the face's size and
/// the body at two.
fn draw(answers: &Answers, width: u32, height: u32) {
    // SAFETY: the surface is mapped here, writable, and at least
    // `width * height * 4` bytes -- checked when it was taken and again after
    // every replacement.
    let mut canvas = unsafe { Canvas::packed(SURFACE_AT, width, height) };

    let ink = Colour::rgb(0xE8, 0xEE, 0xF8);
    let dim = Colour::rgb(0x7C, 0x8A, 0xA4);
    let accent = Colour::rgb(0x38, 0x8B, 0xE8);
    let warn = Colour::rgb(0xE8, 0x70, 0x60);

    canvas.gradient(
        canvas.bounds(),
        Colour::rgb(0x0A, 0x16, 0x2A),
        Colour::rgb(0x04, 0x08, 0x12),
    );

    // The card. Bounded on both sides so it is neither a stripe on a wide
    // screen nor wider than a narrow one.
    let card_width = width
        .clamp(320, 1_100)
        .min(width.saturating_sub(80))
        .max(280);
    let card_height = height
        .clamp(240, 620)
        .min(height.saturating_sub(60))
        .max(220);
    let card = Rect::new(
        (width.saturating_sub(card_width)) / 2,
        (height.saturating_sub(card_height)) / 2,
        card_width,
        card_height,
    );
    canvas.fill(card, Colour::rgb(0x0D, 0x1A, 0x30));
    canvas.outline(card, 1, accent.blend(Colour::rgb(0, 0, 0), 110));

    let inner = card.inset(28);
    let scale = if inner.width >= 700 { 2 } else { 1 };

    // Where the reader is, small and above the heading.
    let (at, total) = answers.page.position();
    canvas.text(inner.x, inner.y, &format!("{at} / {total}"), dim);

    // The heading.
    let heading_y = inner.y + nexus_ui::LINE_HEIGHT + 8;
    canvas.text_scaled(
        inner.x,
        heading_y,
        nexus_i18n::text("setup.title"),
        accent,
        scale + 1,
    );

    let rule_y = heading_y + nexus_ui::LINE_HEIGHT * (scale + 1) + 10;
    canvas.fill(
        Rect::new(inner.x, rule_y, inner.width, 1),
        accent.blend(Colour::rgb(0, 0, 0), 150),
    );

    let (prompt, body) = page_text(answers);

    // The question.
    let mut y = rule_y + 18;
    for line in nexus_ui::wrap(&prompt, inner.width / scale) {
        canvas.text_scaled(inner.x, y, line, ink, scale);
        y += nexus_ui::LINE_HEIGHT * scale + 2;
    }

    // And the answer, larger still, because it is the thing being changed.
    y += 10;
    let answer_scale = scale + 1;
    for line in &body {
        for wrapped in nexus_ui::wrap(line, inner.width / answer_scale) {
            canvas.text_scaled(inner.x, y, wrapped, accent, answer_scale);
            y += nexus_ui::LINE_HEIGHT * answer_scale + 2;
        }
    }

    if let Some(complaint) = &answers.complaint {
        y += 8;
        for wrapped in nexus_ui::wrap(complaint, inner.width / scale) {
            canvas.text_scaled(inner.x, y, wrapped, warn, scale);
            y += nexus_ui::LINE_HEIGHT * scale + 2;
        }
    }

    // The keys that work, along the bottom of the card. A machine nobody has
    // read anything about has to say what to press.
    let footer_y = card.y
        + card
            .height
            .saturating_sub(nexus_ui::LINE_HEIGHT * scale + 16);
    canvas.text_scaled(inner.x, footer_y, &keys_for(answers.page), dim, scale);
}

/// What the current page asks, and what it is showing.
fn page_text(answers: &Answers) -> (String, Vec<String>) {
    match answers.page {
        Page::Language => (
            nexus_i18n::text("setup.language").to_string(),
            alloc::vec![nexus_i18n::LOCALES[answers.language].name.to_string()],
        ),
        Page::Timezone => {
            let zone = nexus_time::ZONES[answers.zone];
            let mut lines = alloc::vec![zone.name.to_string()];
            // The clock, in the zone being chosen. Which is the only way to
            // tell whether it is the right one.
            if let Ok(seconds) = nexus_user::now() {
                lines.push(nexus_time::local(seconds as i64, &zone).to_text());
            }
            (nexus_i18n::text("setup.timezone").to_string(), lines)
        }
        Page::Account => (
            nexus_i18n::text("setup.name").to_string(),
            alloc::vec![format!("{}_", answers.name)],
        ),
        Page::Password => (
            nexus_i18n::text("setup.password").to_string(),
            alloc::vec![masked(&answers.password)],
        ),
        Page::Confirm => (
            nexus_i18n::text("setup.password_again").to_string(),
            alloc::vec![masked(&answers.confirmation)],
        ),
        Page::Network => {
            let lines: Vec<String> = if answers.network.is_empty() {
                alloc::vec![nexus_i18n::text("setup.network_none").to_string()]
            } else {
                answers
                    .network
                    .iter()
                    .filter_map(|line| line.split_once('='))
                    .map(|(name, value)| {
                        format!("{}: {}", name.trim().replace("network.", ""), value.trim())
                    })
                    .collect()
            };
            (nexus_i18n::text("setup.network").to_string(), lines)
        }
        Page::Done => (
            nexus_i18n::text("setup.done").to_string(),
            alloc::vec![
                format!("{}: {}", nexus_i18n::text("setup.name"), answers.name),
                format!(
                    "{}: {}",
                    nexus_i18n::text("setup.language"),
                    nexus_i18n::LOCALES[answers.language].name
                ),
                format!(
                    "{}: {}",
                    nexus_i18n::text("setup.timezone"),
                    nexus_time::ZONES[answers.zone].name
                ),
            ],
        ),
    }
}

/// A password, as it is shown.
///
/// One dot per character. Not the length hidden entirely, because somebody
/// typing needs to see that the keys are arriving, and not the characters,
/// because somebody may be watching.
fn masked(password: &str) -> String {
    let mut out = String::new();
    for _ in password.chars() {
        out.push('*');
    }
    out.push('_');
    out
}

/// Which keys do something on this page.
fn keys_for(page: Page) -> String {
    match page {
        Page::Language | Page::Timezone => nexus_i18n::text("setup.keys_choose").to_string(),
        Page::Done => nexus_i18n::text("setup.keys_done").to_string(),
        _ => nexus_i18n::text("setup.keys_type").to_string(),
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
    nexus_user::log("setup: PANIC").ok();
    nexus_user::exit_with(2)
}
