//! What this machine looks like.
//!
//! Four colours and a pattern, read out of the settings file. It is a library
//! rather than a few lines inside the wallpaper because three programs need the
//! same answer — the thing that paints the background, the thing that paints
//! the strip, and the thing that lets somebody change it — and three readings
//! of one file is how two of them come to disagree.
//!
//! # Why colours are written as hex
//!
//! `look.accent = 58a6ff` is a thing a person can type, look up, and copy from
//! anywhere else that talks about colour. Names would need a table that is
//! always missing the one somebody wants; three numbers would need a separator
//! nobody agrees on.
//!
//! An unreadable value falls back to the default rather than failing. A machine
//! whose desktop would not start because somebody mistyped a colour would be a
//! machine held hostage by a settings file.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

/// The keys this reads.
pub mod key {
    /// The pattern behind the windows.
    pub const STYLE: &str = "look.style";
    /// The colour at the top of the background.
    pub const TOP: &str = "look.top";
    /// And at the bottom.
    pub const BOTTOM: &str = "look.bottom";
    /// What is picked out: focus rings, buttons, the things that can be pressed.
    pub const ACCENT: &str = "look.accent";
}

/// What is drawn behind the windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// One colour.
    Plain,
    /// Top to bottom.
    #[default]
    Gradient,
    /// A gradient with points of light on it, drifting.
    Stars,
    /// Rings spreading from the middle, breathing.
    Rings,
    /// A grid, fading with distance.
    Grid,
}

impl Style {
    /// The style this text names, or the default.
    #[must_use]
    pub fn parse(text: Option<&str>) -> Self {
        match text.map(str::trim) {
            Some("plain") => Self::Plain,
            Some("stars") => Self::Stars,
            Some("rings") => Self::Rings,
            Some("grid") => Self::Grid,
            _ => Self::Gradient,
        }
    }

    /// How it is written down.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Gradient => "gradient",
            Self::Stars => "stars",
            Self::Rings => "rings",
            Self::Grid => "grid",
        }
    }

    /// Every style there is, for something that lists them.
    pub const ALL: [Self; 5] = [
        Self::Plain,
        Self::Gradient,
        Self::Stars,
        Self::Rings,
        Self::Grid,
    ];

    /// Whether it changes on its own.
    ///
    /// What this decides is whether the program drawing it asks to be woken on
    /// a timer. A still background that redrew on a clock would be a machine
    /// spending a frame a second on a picture nobody is watching change.
    #[must_use]
    pub const fn moves(self) -> bool {
        matches!(self, Self::Stars | Self::Rings)
    }
}

/// A colour, as the settings file writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Colour {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// Read `rrggbb`, with or without a leading `#`.
    ///
    /// Three digits are accepted too, because `#08f` is a thing people write
    /// and expanding it is one line.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('#');
        let digits: Option<alloc::vec::Vec<u8>> = text
            .chars()
            .map(|character| character.to_digit(16).map(|value| value as u8))
            .collect();
        let digits = digits?;
        match digits.len() {
            // Each digit doubled: `08f` is `0088ff`, which is what every other
            // tool that accepts three digits means by it.
            3 => Some(Self::new(digits[0] * 17, digits[1] * 17, digits[2] * 17)),
            6 => Some(Self::new(
                digits[0] * 16 + digits[1],
                digits[2] * 16 + digits[3],
                digits[4] * 16 + digits[5],
            )),
            _ => None,
        }
    }

    /// How it is written down.
    #[must_use]
    pub fn to_text(self) -> String {
        format!("{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }

    /// Packed as the framebuffer wants it.
    #[must_use]
    pub const fn packed(self) -> u32 {
        (self.red as u32) << 16 | (self.green as u32) << 8 | self.blue as u32
    }

    /// From the packed form.
    #[must_use]
    pub const fn from_packed(value: u32) -> Self {
        Self::new((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// Part of the way to another colour; `amount` is 0..=255.
    #[must_use]
    pub const fn towards(self, other: Self, amount: u8) -> Self {
        let inverse = 255 - amount as u32;
        let amount = amount as u32;
        Self::new(
            ((self.red as u32 * inverse + other.red as u32 * amount) / 255) as u8,
            ((self.green as u32 * inverse + other.green as u32 * amount) / 255) as u8,
            ((self.blue as u32 * inverse + other.blue as u32 * amount) / 255) as u8,
        )
    }
}

/// Everything about how this machine looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub style: Style,
    pub top: Colour,
    pub bottom: Colour,
    pub accent: Colour,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            style: Style::Gradient,
            // The colours this system has had since its first frame. Kept as
            // the default so that a machine with no `look.*` settings looks
            // exactly as it did before there were any.
            top: Colour::new(0x0B, 0x14, 0x28),
            bottom: Colour::new(0x04, 0x08, 0x14),
            accent: Colour::new(0x38, 0x8B, 0xE8),
        }
    }
}

impl Look {
    /// Read it out of a settings file's contents.
    ///
    /// Every value that will not read falls back to the default, one at a time.
    /// A mistyped accent should cost the accent and not the wallpaper.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut look = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            match name {
                key::STYLE => look.style = Style::parse(Some(value)),
                key::TOP => {
                    if let Some(colour) = Colour::parse(value) {
                        look.top = colour;
                    }
                }
                key::BOTTOM => {
                    if let Some(colour) = Colour::parse(value) {
                        look.bottom = colour;
                    }
                }
                key::ACCENT => {
                    if let Some(colour) = Colour::parse(value) {
                        look.accent = colour;
                    }
                }
                _ => {}
            }
        }
        look
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_reads_six_digits() {
        assert_eq!(Colour::parse("58a6ff"), Some(Colour::new(0x58, 0xA6, 0xFF)));
        assert_eq!(
            Colour::parse("#58A6FF"),
            Some(Colour::new(0x58, 0xA6, 0xFF))
        );
        assert_eq!(
            Colour::parse("  58a6ff  "),
            Some(Colour::new(0x58, 0xA6, 0xFF))
        );
    }

    #[test]
    fn three_digits_are_each_doubled() {
        assert_eq!(Colour::parse("08f"), Some(Colour::new(0x00, 0x88, 0xFF)));
        assert_eq!(Colour::parse("#fff"), Some(Colour::new(255, 255, 255)));
    }

    #[test]
    fn something_that_is_not_a_colour_is_not_guessed_at() {
        assert_eq!(Colour::parse(""), None);
        assert_eq!(Colour::parse("blue"), None);
        assert_eq!(Colour::parse("58a6f"), None);
        assert_eq!(Colour::parse("58a6fff"), None);
        assert_eq!(Colour::parse("zzzzzz"), None);
    }

    #[test]
    fn a_colour_round_trips() {
        let colour = Colour::new(0x12, 0x34, 0x56);
        assert_eq!(colour.to_text(), "123456");
        assert_eq!(Colour::parse(&colour.to_text()), Some(colour));
        assert_eq!(Colour::from_packed(colour.packed()), colour);
    }

    #[test]
    fn blending_ends_where_it_should() {
        let black = Colour::new(0, 0, 0);
        let white = Colour::new(255, 255, 255);
        assert_eq!(black.towards(white, 0), black);
        assert_eq!(black.towards(white, 255), white);
        let half = black.towards(white, 128);
        assert!(half.red > 120 && half.red < 135);
    }

    #[test]
    fn a_style_is_named_and_read_back() {
        for style in Style::ALL {
            assert_eq!(Style::parse(Some(style.name())), style);
        }
        // Anything else is the default rather than a failure.
        assert_eq!(Style::parse(None), Style::Gradient);
        assert_eq!(Style::parse(Some("something else")), Style::Gradient);
    }

    #[test]
    fn only_some_styles_want_waking() {
        assert!(Style::Stars.moves());
        assert!(Style::Rings.moves());
        assert!(!Style::Plain.moves());
        assert!(!Style::Gradient.moves());
        assert!(!Style::Grid.moves());
    }

    #[test]
    fn a_settings_file_gives_a_look() {
        let look = Look::parse(
            "# a machine\n\
             system.configured = yes\n\
             look.style = stars\n\
             look.top = 201040\n\
             look.accent = 08f\n",
        );
        assert_eq!(look.style, Style::Stars);
        assert_eq!(look.top, Colour::new(0x20, 0x10, 0x40));
        assert_eq!(look.accent, Colour::new(0x00, 0x88, 0xFF));
        // Untouched, so it keeps the default.
        assert_eq!(look.bottom, Look::default().bottom);
    }

    #[test]
    fn one_bad_value_costs_only_itself() {
        let look = Look::parse("look.accent = not-a-colour\nlook.style = grid\n");
        assert_eq!(look.accent, Look::default().accent);
        assert_eq!(look.style, Style::Grid);
    }

    #[test]
    fn a_file_with_nothing_in_it_is_the_default() {
        assert_eq!(Look::parse(""), Look::default());
        assert_eq!(Look::parse("# only a comment\n"), Look::default());
    }
}
