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
    let mut settings = Settings::new(directory, window.width(), window.height());
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
    /// A displayed value that comes from detected hardware and cannot be edited.
    fixed: Option<String>,
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
            fixed: None,
        }
    }

    fn typed(key: &'static str, label: &'static str, fallback: &'static str) -> Self {
        Self {
            key,
            label,
            choices: Vec::new(),
            fallback,
            colour: false,
            fixed: None,
        }
    }

    fn info(label: &'static str, value: String) -> Self {
        Self {
            key: "",
            label,
            choices: Vec::new(),
            fallback: "",
            colour: false,
            fixed: Some(value),
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
    /// Current surface size, retained so pointer hit targets use the same layout as drawing.
    viewport: (u32, u32),
}

/// Geometry shared by the renderer and pointer hit testing.
struct UiLayout {
    outer: Rect,
    sidebar: u32,
    area: Rect,
    body: Rect,
    scale: u32,
    text_height: u32,
    row_height: u32,
    row_gap: u32,
    footer_height: u32,
    list_y: u32,
}

impl UiLayout {
    fn new(bounds: Rect) -> Self {
        let outer = Rect::new(
            PAD,
            nexus_ui::TITLE_BAR + PAD,
            bounds.width.saturating_sub(PAD * 2),
            bounds.height.saturating_sub(nexus_ui::TITLE_BAR + PAD * 2),
        );
        let scale = if bounds.width >= 700 && bounds.height >= 500 {
            2
        } else {
            1
        };
        let sidebar = if bounds.width >= 600 {
            196
        } else if bounds.width >= 420 {
            128
        } else {
            0
        }
        .min(outer.width / 3);
        let indent = if sidebar == 0 { 0 } else { sidebar + PAD };
        let area = Rect::new(
            outer.x + indent,
            outer.y,
            outer.width.saturating_sub(indent),
            outer.height,
        );
        let text_height = nexus_ui::LINE_HEIGHT * scale;
        let row_gap = 6;
        let header_height = text_height + 12;
        let footer_height = nexus_ui::LINE_HEIGHT * 2 + 14;
        let body = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(footer_height),
        );
        let list_y = body.y + header_height + row_gap;
        Self {
            outer,
            sidebar,
            area,
            body,
            scale,
            text_height,
            row_height: text_height + 8,
            row_gap,
            footer_height,
            list_y,
        }
    }

    fn visible_rows(&self) -> usize {
        (self.body.y + self.body.height)
            .saturating_sub(self.list_y)
            .checked_div(self.row_height + self.row_gap)
            .unwrap_or(0)
            .max(1) as usize
    }

    fn value_x(&self, row_x: u32, row_width: u32) -> u32 {
        row_x + row_width * 48 / 100
    }

    fn apply_button(&self) -> Rect {
        let width = 156.min(self.area.width.saturating_sub(16));
        Rect::new(
            self.area.x + self.area.width.saturating_sub(width + 8),
            self.area.y + 4,
            width,
            40,
        )
    }
}

impl Settings {
    fn new(directory: Option<Handle>, width: u32, height: u32) -> Self {
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
        let hardware_report =
            directory.and_then(|handle| read_text(handle, "hardware.txt").ok().flatten());
        let (wifi, bluetooth, graphics) = hardware_report
            .as_deref()
            .map(|report| {
                (
                    format_hardware_status(report_value(report, "wifi")),
                    format_hardware_status(report_value(report, "bluetooth")),
                    format_hardware_status(report_value(report, "graphics")),
                )
            })
            .unwrap_or_else(|| {
                (
                    nexus_i18n::text("settings.device.unavailable").to_string(),
                    nexus_i18n::text("settings.device.unavailable").to_string(),
                    nexus_i18n::text("settings.device.unavailable").to_string(),
                )
            });
        if hardware_report.is_some() {
            nexus_user::log("settings: detected device inventory loaded").ok();
        }

        use nexus_config::key;
        let items = alloc::vec![
            Item::Heading("settings.look"),
            Item::Row(Row::choice(
                nexus_look::key::STYLE,
                "settings.style",
                styles,
                "landscape"
            )),
            Item::Row(Row::typed(nexus_look::key::TOP, "settings.top", "d9e5f8").into_colour()),
            Item::Row(
                Row::typed(nexus_look::key::BOTTOM, "settings.bottom", "dbc9ee").into_colour()
            ),
            Item::Row(
                Row::typed(nexus_look::key::ACCENT, "settings.accent", "7c72d9").into_colour()
            ),
            Item::Row(Row::choice(
                nexus_look::key::FONT,
                "settings.font",
                nexus_look::Font::ALL
                    .iter()
                    .map(|font| font.name().to_string())
                    .collect(),
                "smooth"
            )),
            Item::Row(Row::choice(
                nexus_look::key::SMOOTH,
                "settings.smooth",
                alloc::vec!["no".to_string(), "yes".to_string()],
                "yes"
            )),
            Item::Row(Row::choice(
                nexus_look::key::PICTURE,
                "settings.picture",
                alloc::vec![
                    "none".to_string(),
                    "nexus.png".to_string(),
                    "nexus.avi".to_string(),
                    "space.png".to_string(),
                    "rain.avi".to_string(),
                ],
                "none",
            )),
            Item::Row(Row::choice(
                nexus_look::key::FIT,
                "settings.fit",
                alloc::vec![
                    "fill".to_string(),
                    "whole".to_string(),
                    "middle".to_string()
                ],
                "fill"
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
            Item::Heading("settings.display"),
            Item::Row(Row::choice(
                key::RESOLUTION,
                "settings.resolution",
                alloc::vec![
                    "1920x1080".to_string(),
                    "1920x1200".to_string(),
                    "2560x1440".to_string(),
                    "3840x2160".to_string(),
                    "1280x800".to_string(),
                ],
                "1920x1080",
            )),
            Item::Row(Row::choice(
                key::REFRESH_RATE,
                "settings.refreshrate",
                alloc::vec!["60Hz".to_string(), "120Hz".to_string(), "144Hz".to_string()],
                "60Hz",
            )),
            Item::Row(Row::choice(
                key::SCALE,
                "settings.scale",
                alloc::vec![
                    "100%".to_string(),
                    "125%".to_string(),
                    "150%".to_string(),
                    "200%".to_string(),
                ],
                "100%",
            )),
            Item::Heading("settings.input"),
            Item::Row(Row::choice(
                key::KEYBOARD,
                "settings.keyboard",
                alloc::vec!["ja-JP (106/109)".to_string(), "en-US (101/104)".to_string()],
                "ja-JP (106/109)",
            )),
            Item::Row(Row::choice(
                key::IME_MODE,
                "settings.ime",
                alloc::vec![
                    "Direct (Romaji)".to_string(),
                    "Hiragana".to_string(),
                    "Katakana".to_string(),
                ],
                "Direct (Romaji)",
            )),
            Item::Heading("settings.audio"),
            Item::Row(Row::choice(
                key::VOLUME,
                "settings.volume",
                alloc::vec![
                    "100%".to_string(),
                    "75%".to_string(),
                    "50%".to_string(),
                    "25%".to_string(),
                    "0% (Muted)".to_string(),
                ],
                "100%",
            )),
            Item::Row(Row::choice(
                key::SOUND_CHIME,
                "settings.soundchime",
                alloc::vec!["enabled".to_string(), "disabled".to_string()],
                "enabled",
            )),
            Item::Heading("settings.performance"),
            Item::Row(Row::choice(
                key::POWER_PROFILE,
                "settings.powerprofile",
                alloc::vec![
                    "High Performance".to_string(),
                    "Balanced".to_string(),
                    "Power Saver".to_string(),
                ],
                "High Performance",
            )),
            Item::Row(Row::choice(
                key::GPU_BACKEND,
                "settings.gpubackend",
                alloc::vec![
                    "Auto (NVIDIA / VirtIO)".to_string(),
                    "Vulkan (Hardware/SW)".to_string(),
                    "Mesa (Softpipe)".to_string(),
                ],
                "Auto (NVIDIA / VirtIO)",
            )),
            Item::Heading("settings.system"),
            Item::Row(Row::typed(
                key::HOSTNAME,
                "settings.hostname",
                "nexus-workstation"
            )),
            Item::Row(Row::choice(
                key::DOCK_AUTOHIDE,
                "settings.dockautohide",
                alloc::vec!["no".to_string(), "yes".to_string()],
                "no",
            )),
            Item::Heading("settings.devices"),
            Item::Row(Row::info("settings.wifi", wifi)),
            Item::Row(Row::info("settings.bluetooth", bluetooth)),
            Item::Row(Row::info("settings.graphics", graphics)),
            Item::Heading("settings.you"),
            Item::Row(Row::typed(key::USER_NAME, "settings.name", "")),
            Item::Row(Row::choice(
                "account.autologin",
                "settings.autologin",
                alloc::vec!["no".to_string(), "yes".to_string()],
                "no",
            )),
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
            viewport: (width, height),
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
        row.fixed
            .clone()
            .unwrap_or_else(|| self.values.get(row.key).unwrap_or(row.fallback).to_string())
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

    /// Apply all current settings, commit to disk and flush live changes.
    fn apply_all(&mut self) -> bool {
        if let Some(typed) = self.editing.take() {
            if let Some(row) = self.row() {
                self.save(row.key, &typed);
            }
        }
        let Some(directory) = self.directory else {
            self.complain(nexus_i18n::text("settings.nofile"));
            return false;
        };
        match write_text(directory, SETTINGS_NAME, &self.values.to_text()) {
            Ok(()) => {
                self.look = nexus_look::Look::parse(&self.values.to_text());
                self.say(nexus_i18n::text("settings.applied"));
                nexus_user::log("settings: all settings applied and synced to disk").ok();
                true
            }
            Err(why) => {
                self.complain(&why);
                false
            }
        }
    }
}

impl App for Settings {
    fn draw(&mut self, canvas: &mut Canvas) {
        // The settings surface is a reading surface, not glass: text and values
        // must remain legible even when other windows sit behind it.
        canvas.set_layer(false);
        canvas.set_text_style(
            nexus_ui::font::Face::parse(Some(self.look.font.name())),
            self.look.smooth,
        );
        let bounds = canvas.bounds();
        self.viewport = (bounds.width, bounds.height);
        let layout = UiLayout::new(bounds);
        let accent = Colour(self.look.accent.packed());
        let ink = Colour::rgb(36, 45, 66);
        let quiet = Colour::rgb(91, 103, 128);
        let panel = Colour::rgb(250, 251, 255);

        canvas.fill(bounds, Colour::rgb(239, 243, 250));
        canvas.panel(layout.outer, 16, panel, Colour::rgb(211, 220, 237));

        let section = self.items[..=self.cursor]
            .iter()
            .rposition(|item| matches!(item, Item::Heading(_)))
            .unwrap_or(0);
        let end = self
            .items
            .iter()
            .enumerate()
            .skip(section + 1)
            .find(|(_, item)| matches!(item, Item::Heading(_)))
            .map_or(self.items.len(), |(index, _)| index);
        if layout.sidebar != 0 {
            canvas.fill_rounded(
                Rect::new(
                    layout.outer.x,
                    layout.outer.y,
                    layout.sidebar,
                    layout.outer.height,
                ),
                12,
                Colour::rgb(235, 240, 249),
            );
            let mut y = layout.outer.y + 10;
            for (index, item) in self.items.iter().enumerate() {
                if let Item::Heading(label) = item {
                    if index == section {
                        canvas.fill_rounded(
                            Rect::new(layout.outer.x + 6, y, layout.sidebar - 12, 40),
                            9,
                            accent.blend(Colour::rgb(235, 240, 249), 224),
                        );
                        canvas.fill_rounded(Rect::new(layout.outer.x + 6, y + 7, 3, 26), 2, accent);
                    }
                    let label = nexus_i18n::text(label);
                    let label_scale = 1;
                    let text_height = nexus_ui::LINE_HEIGHT * label_scale;
                    canvas.text_scaled(
                        layout.outer.x + 18,
                        y + (40 - text_height) / 2,
                        label,
                        if index == section { accent } else { ink },
                        label_scale,
                    );
                    y += 48;
                }
            }
        }
        let title = match &self.items[section] {
            Item::Heading(label) => nexus_i18n::text(label),
            _ => nexus_i18n::text("settings.title"),
        };
        canvas.text_scaled(
            layout.area.x,
            layout.area.y + 4,
            title,
            accent,
            layout.scale,
        );

        let btn_rect = layout.apply_button();
        canvas.fill_rounded(btn_rect, 9, accent);
        canvas.text_centred(
            btn_rect,
            nexus_i18n::text("settings.apply"),
            Colour::rgb(255, 255, 255),
        );

        let visible = layout.visible_rows();
        let start = (section + 1).max(self.cursor.saturating_sub(visible - 1));
        for index in start..end {
            let offset = index - start;
            let line = Rect::new(
                layout.area.x,
                layout.list_y + offset as u32 * (layout.row_height + layout.row_gap),
                layout.area.width,
                layout.row_height,
            );
            if line.y + line.height > layout.body.y + layout.body.height {
                break;
            }
            match &self.items[index] {
                Item::Heading(label) => {
                    canvas.text_scaled(
                        line.x,
                        line.y,
                        nexus_i18n::text(label),
                        accent,
                        layout.scale,
                    );
                }
                Item::Row(row) => {
                    let chosen = index == self.cursor;
                    if chosen {
                        canvas.fill_rounded(line, 8, accent.blend(panel, 224));
                        canvas.fill_rounded(
                            Rect::new(line.x + 2, line.y + 7, 3, line.height - 14),
                            2,
                            accent,
                        );
                    } else {
                        canvas.fill(
                            Rect::new(line.x + 8, line.y + line.height - 1, line.width - 16, 1),
                            Colour::rgb(231, 235, 244),
                        );
                    }
                    let text_y = line.y + (line.height - layout.text_height) / 2;
                    canvas.text_scaled(
                        line.x + 14,
                        text_y,
                        nexus_i18n::text(row.label),
                        ink,
                        layout.scale,
                    );

                    let value = match (&self.editing, chosen) {
                        (Some(typed), true) => format!("{typed}\u{2588}"),
                        _ => self.shown(row),
                    };
                    let value_at = layout.value_x(line.x, line.width);
                    let after =
                        value_at + canvas.text_scaled(value_at, text_y, &value, ink, layout.scale);

                    if row.colour {
                        if let Some(swatch) = nexus_look::Colour::parse(&value) {
                            canvas.panel(
                                Rect::new(after + 12, line.y + 6, 34, line.height - 12),
                                6,
                                Colour(swatch.packed()),
                                Colour::rgb(205, 213, 228),
                            );
                        }
                    } else if !row.choices.is_empty() && chosen {
                        canvas.text_scaled(value_at - 22, text_y, "<", quiet, layout.scale);
                        canvas.text_scaled(after + 12, text_y, ">", quiet, layout.scale);
                    }
                }
            }
        }

        let footer = Rect::new(
            layout.area.x,
            layout.area.y + layout.area.height.saturating_sub(layout.footer_height),
            layout.area.width,
            layout.footer_height,
        );
        canvas.fill(
            Rect::new(footer.x, footer.y, footer.width, 1),
            Colour::rgb(220, 227, 239),
        );
        if !self.said.is_empty() {
            let colour = if self.trouble {
                Colour::rgb(176, 54, 61)
            } else {
                accent
            };
            canvas.text(footer.x, footer.y + 4, &self.said, colour);
        }
        canvas.text(
            footer.x,
            footer.y + nexus_ui::LINE_HEIGHT + 8,
            nexus_i18n::text("settings.keys"),
            quiet,
        );
    }

    fn resized(&mut self, width: u32, height: u32) {
        self.viewport = (width, height);
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
                if !row.choices.is_empty() || row.fixed.is_some() {
                    return false;
                }
                let current = self.shown(row);
                self.editing.get_or_insert(current).push(character);
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
                    if !row.choices.is_empty() || row.fixed.is_some() {
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
                    return self.apply_all();
                };
                let Some(row) = self.row() else {
                    return false;
                };
                if row.fixed.is_some() {
                    return false;
                }
                let (key, colour) = (row.key, row.colour);
                if colour && nexus_look::Colour::parse(&typed).is_none() {
                    self.editing = Some(typed);
                    self.complain(nexus_i18n::text("settings.notacolour"));
                    return true;
                }
                self.save(key, &typed);
                true
            }
            Key::Escape => self.editing.take().is_some(),
            Key::Language => true,
            _ => false,
        }
    }

    fn click(&mut self, x: u32, y: u32) -> bool {
        let layout = UiLayout::new(Rect::new(0, 0, self.viewport.0, self.viewport.1));
        if layout.apply_button().contains(x, y) {
            return self.apply_all();
        }

        if x >= layout.outer.x && x < layout.outer.x + layout.sidebar {
            let mut item_y = layout.outer.y + 10;
            for (index, item) in self.items.iter().enumerate() {
                if let Item::Heading(_) = item {
                    if y >= item_y && y < item_y + 40 {
                        if let Some(target) = self.next_row(index + 1, 1) {
                            self.cursor = target;
                            self.editing = None;
                            return true;
                        }
                    }
                    item_y += 48;
                }
            }
            return false;
        }

        let section = self.items[..=self.cursor]
            .iter()
            .rposition(|item| matches!(item, Item::Heading(_)))
            .unwrap_or(0);
        let end = self
            .items
            .iter()
            .enumerate()
            .skip(section + 1)
            .find(|(_, item)| matches!(item, Item::Heading(_)))
            .map_or(self.items.len(), |(index, _)| index);
        if y < layout.list_y {
            return false;
        }
        let stride = layout.row_height + layout.row_gap;
        let offset = ((y - layout.list_y) / stride) as usize;
        let row_y = layout.list_y + offset as u32 * stride;
        if y >= row_y + layout.row_height {
            return false;
        }
        let visible = layout.visible_rows();
        let start = (section + 1).max(self.cursor.saturating_sub(visible - 1));
        let target_idx = start + offset;
        if target_idx >= end {
            return false;
        }
        if let Some(Item::Row(row)) = self.items.get(target_idx) {
            let line_x = layout.area.x;
            let value_at = layout.value_x(line_x, layout.area.width);
            if self.cursor == target_idx {
                if !row.choices.is_empty() {
                    return self.step(if x < value_at { -1 } else { 1 });
                }
            } else {
                self.cursor = target_idx;
                self.editing = None;
                if !row.choices.is_empty() && x >= value_at {
                    return self.step(1);
                }
                return true;
            }
        }
        false
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
fn report_value<'a>(report: &'a str, key: &str) -> Option<&'a str> {
    report.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn format_hardware_status(value: Option<&str>) -> String {
    let Some(value) = value else {
        return nexus_i18n::text("settings.device.unavailable").to_string();
    };
    if value == "none" {
        return nexus_i18n::text("settings.device.none").to_string();
    }
    let Some((device, id)) = value.split_once('|') else {
        return nexus_i18n::text("settings.device.unavailable").to_string();
    };
    nexus_i18n::format(
        "settings.device.present",
        &[("device", &device), ("id", &id)],
    )
}

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
