//! The settings file: what the machine was told about itself.
//!
//! `key = value`, one per line, UTF-8, with `#` for comments — the same shape as
//! `locales/*.txt`, for the same reason. It is a format a person can read and
//! repair with a text editor, which matters more than compactness for a file
//! that decides whether the machine can be logged into.
//!
//! # Why not something structured
//!
//! Because the failure that matters is a settings file that cannot be parsed.
//! A binary format turns a single corrupt byte into a machine that will not
//! start and cannot say why; this turns it into one line that is ignored, and
//! the rest still applies. Every reader here takes the view that a missing or
//! unreadable setting has a default, and the only setting without one is
//! whether setup has been completed — because guessing *that* wrong either
//! locks somebody out of their machine or hands it to a stranger.
//!
//! # What is not in here
//!
//! The password. What is stored is a salt and the output of a key-derivation
//! function over the password and that salt, which is not a password and cannot
//! be turned back into one. See `nexus_crypto::password`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The keys this system reads and writes.
pub mod key {
    /// Whether the first-run setup has been completed.
    pub const CONFIGURED: &str = "system.configured";
    /// The interface language, as a BCP 47 tag.
    pub const LANGUAGE: &str = "system.language";
    /// The timezone's name, as it appears in `nexus_time::ZONES`.
    pub const TIMEZONE: &str = "system.timezone";
    /// When setup was completed, as seconds since 1970.
    pub const CONFIGURED_AT: &str = "system.configured_at";
    /// The version of the system that is installed.
    pub const VERSION: &str = "system.version";
    /// Whether the machine installs updates by itself.
    ///
    /// `automatic`, which is the default and what an unset or unreadable value
    /// is read as, or `ask`. A machine that quietly stopped updating because
    /// somebody mistyped a setting would be the worst of the three outcomes.
    pub const UPDATES: &str = "system.updates";
    /// When the machine last looked for updates, as seconds since 1970.
    pub const UPDATES_CHECKED: &str = "system.updates_checked";

    /// Who uses this machine.
    pub const USER_NAME: &str = "user.name";
    /// The salt their password was hashed with, as hex.
    pub const USER_SALT: &str = "user.salt";
    /// The hash, as hex.
    pub const USER_HASH: &str = "user.hash";
    /// How many rounds it was hashed with, so raising the count later does not
    /// lock anybody out: an old hash is still checkable with the old number.
    pub const USER_ROUNDS: &str = "user.rounds";

    /// How the network is configured -- `dhcp` or `off`.
    pub const NETWORK: &str = "network.mode";
    /// The address the machine last had, for showing.
    pub const NETWORK_ADDRESS: &str = "network.address";
    /// And its gateway and resolver.
    pub const NETWORK_GATEWAY: &str = "network.gateway";
    pub const NETWORK_DNS: &str = "network.dns";
}

/// A settings file, in memory.
#[derive(Default)]
pub struct Settings {
    entries: Vec<(String, String)>,
}

impl Settings {
    /// An empty one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Read a settings file.
    ///
    /// Never fails. A line that is not `key = value` is skipped, because the
    /// alternative — refusing the whole file — turns one bad line into a
    /// machine that cannot start.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut settings = Self::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            settings.set(name, value.trim());
        }
        settings
    }

    /// What a setting says, if it says anything.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// What it says, or a default.
    #[must_use]
    pub fn get_or<'a>(&'a self, name: &str, fallback: &'a str) -> &'a str {
        self.get(name).unwrap_or(fallback)
    }

    /// A setting read as a number.
    #[must_use]
    pub fn get_number(&self, name: &str) -> Option<u64> {
        self.get(name)?.parse().ok()
    }

    /// Whether a setting is present and says yes.
    #[must_use]
    pub fn is_yes(&self, name: &str) -> bool {
        matches!(self.get(name), Some("yes" | "true" | "1"))
    }

    /// Set one, replacing what was there.
    pub fn set(&mut self, name: &str, value: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|(key, _)| key == name) {
            entry.1 = value.to_string();
            return;
        }
        self.entries.push((name.to_string(), value.to_string()));
    }

    /// How many settings there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write it out.
    ///
    /// In the order the settings were set, not sorted: a file that reorders
    /// itself every time it is written is a file whose diffs say nothing.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::from(
            "# NexusOS settings. Written by the system; safe to edit by hand.\n\
             # One `key = value` per line. A line that cannot be read is ignored\n\
             # rather than refused, so one bad line does not stop the machine.\n\n",
        );
        for (name, value) in &self.entries {
            out.push_str(&format!("{name} = {value}\n"));
        }
        out
    }
}

/// Bytes as hexadecimal, for the settings that are not text.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// And back. Returns how many bytes were read.
///
/// Anything that is not a pair of hex digits stops the read rather than being
/// skipped: a salt half read is a salt that will not match, and failing to
/// notice is worse than failing.
pub fn from_hex(text: &str, into: &mut [u8]) -> usize {
    let bytes = text.as_bytes();
    let mut written = 0;
    let mut index = 0;
    while index + 1 < bytes.len() && written < into.len() {
        let Some(high) = digit(bytes[index]) else {
            break;
        };
        let Some(low) = digit(bytes[index + 1]) else {
            break;
        };
        into[written] = (high << 4) | low;
        written += 1;
        index += 2;
    }
    written
}

/// One hex digit.
fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_can_be_read() {
        let mut settings = Settings::new();
        settings.set(key::CONFIGURED, "yes");
        settings.set(key::LANGUAGE, "ja-JP");
        settings.set(key::USER_NAME, "someone");

        let read = Settings::parse(&settings.to_text());
        assert!(read.is_yes(key::CONFIGURED));
        assert_eq!(read.get(key::LANGUAGE), Some("ja-JP"));
        assert_eq!(read.get(key::USER_NAME), Some("someone"));
        assert_eq!(read.len(), 3);
    }

    #[test]
    fn setting_something_twice_replaces_it() {
        let mut settings = Settings::new();
        settings.set(key::LANGUAGE, "en-US");
        settings.set(key::LANGUAGE, "ja-JP");
        assert_eq!(settings.len(), 1);
        assert_eq!(settings.get(key::LANGUAGE), Some("ja-JP"));
    }

    #[test]
    fn a_line_that_cannot_be_read_is_skipped_not_fatal() {
        // The failure that matters: one corrupt line must not cost the rest of
        // the file, because the rest of the file is how the machine starts.
        let settings = Settings::parse(
            "# a comment\n\
             system.language = en-US\n\
             this line has no equals sign\n\
             = a value with no name\n\
             \n\
             user.name = someone\n",
        );
        assert_eq!(settings.get(key::LANGUAGE), Some("en-US"));
        assert_eq!(settings.get(key::USER_NAME), Some("someone"));
        assert_eq!(settings.len(), 2);
    }

    #[test]
    fn whitespace_around_the_equals_does_not_matter() {
        let settings = Settings::parse("  user.name=  someone   \n");
        assert_eq!(settings.get(key::USER_NAME), Some("someone"));
    }

    #[test]
    fn a_value_may_contain_an_equals_sign() {
        // Only the first one separates. A password hash is hex and will not,
        // but a value that did would otherwise be silently truncated.
        let settings = Settings::parse("a = b = c\n");
        assert_eq!(settings.get("a"), Some("b = c"));
    }

    #[test]
    fn an_absent_setting_is_absent_rather_than_empty() {
        let settings = Settings::new();
        assert_eq!(settings.get(key::LANGUAGE), None);
        assert_eq!(settings.get_or(key::LANGUAGE, "en-US"), "en-US");
        // And "configured" is false when it is missing, which is the answer
        // that runs setup rather than the one that skips it.
        assert!(!settings.is_yes(key::CONFIGURED));
    }

    #[test]
    fn numbers_read_as_numbers() {
        let settings = Settings::parse("user.rounds = 4096\nbad = twelve\n");
        assert_eq!(settings.get_number(key::USER_ROUNDS), Some(4096));
        assert_eq!(settings.get_number("bad"), None);
        assert_eq!(settings.get_number("missing"), None);
    }

    #[test]
    fn hex_survives_the_round_trip() {
        let original = [0x00u8, 0x0f, 0xa5, 0xff, 0x10];
        let text = to_hex(&original);
        assert_eq!(text, "000fa5ff10");
        let mut back = [0u8; 5];
        assert_eq!(from_hex(&text, &mut back), 5);
        assert_eq!(back, original);
    }

    #[test]
    fn hex_that_is_not_hex_stops_rather_than_skipping() {
        // A salt half read is a salt that will not match, and a reader that
        // quietly skipped the bad pair would produce a different salt of the
        // right length -- which fails to log somebody in and says nothing.
        let mut back = [0u8; 4];
        assert_eq!(from_hex("00zz1122", &mut back), 1);
    }

    #[test]
    fn a_password_never_appears_in_the_file() {
        // What is stored is a salt and a derived hash. This is the check that
        // the shape of the file cannot drift towards holding the thing itself.
        let mut settings = Settings::new();
        settings.set(key::USER_SALT, &to_hex(&[1u8; 16]));
        settings.set(key::USER_HASH, &to_hex(&[2u8; 32]));
        let text = settings.to_text();
        assert!(!text.contains("password"));
        assert!(text.contains("user.salt"));
        assert!(text.contains("user.hash"));
    }
}
