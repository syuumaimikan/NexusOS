//! Localisation.
//!
//! Translations live in `locales/*.txt` and are compiled in by `build.rs`,
//! because the kernel has no filesystem to load them from yet. Keeping them in
//! data files regardless is what makes adding a language a matter of adding a
//! file: no kernel source changes, and `build.rs` fails the build if a locale
//! is missing a key rather than letting a label render blank.
//!
//! # What is and is not translated
//!
//! Anything a person reads on screen is translated. Serial-log messages are
//! not: they are a developer interface, they need to stay greppable across
//! machines, and a bug report is far more useful when its log reads the same
//! everywhere.
//!
//! # Placeholders
//!
//! Values may contain `{name}` placeholders, substituted at runtime. They are
//! named rather than positional because languages order their arguments
//! differently — the memory line puts the free figure first in English and the
//! total first in Japanese — and a positional format string cannot express
//! that without the translator silently getting it wrong.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

/// One language's strings.
pub struct Locale {
    /// BCP 47 tag, taken from the file name.
    pub tag: &'static str,
    /// The language's name, in that language.
    pub name: &'static str,
    /// Values, in the same order as [`KEYS`].
    pub values: &'static [&'static str; KEY_COUNT],
}

// Generated from locales/*.txt: KEY_COUNT, KEYS, LOCALE_COUNT, LOCALES.
include!(concat!(env!("OUT_DIR"), "/locales.rs"));

/// The locale in use, as an index into [`LOCALES`].
///
/// Plain atomic rather than lock-guarded: it is read on every string lookup,
/// including from the display thread while another thread may be changing it,
/// and a torn read is not possible for a `usize`. The worst a race can do is
/// render one label in the outgoing language, which the next repaint corrects.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Number of available locales.
#[must_use]
pub fn locale_count() -> usize {
    LOCALE_COUNT
}

/// The locale currently in use.
#[must_use]
pub fn current() -> &'static Locale {
    &LOCALES[CURRENT.load(Ordering::Relaxed).min(LOCALE_COUNT - 1)]
}

/// Index of the locale currently in use.
#[must_use]
pub fn current_index() -> usize {
    CURRENT.load(Ordering::Relaxed).min(LOCALE_COUNT - 1)
}

/// Switch to the locale with this BCP 47 tag.
///
/// Returns `false` and changes nothing when the tag is not available, so a
/// caller can fall back rather than end up with no strings at all.
pub fn set_locale(tag: &str) -> bool {
    for (index, locale) in LOCALES.iter().enumerate() {
        if locale.tag == tag {
            CURRENT.store(index, Ordering::Relaxed);
            return true;
        }
    }
    false
}

/// Switch to the next available locale, returning the one now in use.
pub fn next_locale() -> &'static Locale {
    let next = (current_index() + 1) % LOCALE_COUNT;
    CURRENT.store(next, Ordering::Relaxed);
    &LOCALES[next]
}

/// The translated string for `key` in the current locale.
///
/// An unknown key returns the key itself. That is deliberate: a missing string
/// then shows up on screen as `status.uptime` rather than as blank space, which
/// is unmistakable during development and harmless in production. `build.rs`
/// already rules out a key that exists in one locale and not another, so this
/// can only be reached by asking for a key that exists nowhere.
#[must_use]
pub fn text(key: &'static str) -> &'static str {
    match KEYS.binary_search(&key) {
        Ok(index) => current().values[index],
        Err(_) => key,
    }
}

/// The translated string for `key`, with `{name}` placeholders substituted.
///
/// Unmatched placeholders are left as written, so a translation referring to an
/// argument the caller did not supply is visible rather than silently empty.
#[must_use]
pub fn format(key: &'static str, arguments: &[(&str, &dyn core::fmt::Display)]) -> String {
    let template = text(key);
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];

        let Some(close) = after.find('}') else {
            // An unterminated brace is a malformed translation; emit the rest
            // verbatim rather than dropping it.
            out.push('{');
            out.push_str(after);
            return out;
        };

        let name = &after[..close];
        match arguments.iter().find(|(argument, _)| *argument == name) {
            Some((_, value)) => {
                // Writing into a `String` cannot fail.
                let _ = write!(out, "{value}");
            }
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }

        rest = &after[close + 1..];
    }

    out.push_str(rest);
    out
}

// The kernel binary has no host test harness, so what can be checked without
// running is checked at compile time.

/// Every locale must have a value for every key, or `values[index]` would read
/// past the end. `build.rs` enforces this too; this is the second lock.
const _: () = {
    let mut index = 0;
    while index < LOCALE_COUNT {
        assert!(LOCALES[index].values.len() == KEY_COUNT);
        index += 1;
    }
};

/// `text` binary-searches `KEYS`, which requires them sorted.
const _: () = {
    let mut index = 1;
    while index < KEY_COUNT {
        // `str` comparison is not available in const, so compare bytes.
        let previous = KEYS[index - 1].as_bytes();
        let current = KEYS[index].as_bytes();
        let mut position = 0;
        let shorter = if previous.len() < current.len() {
            previous.len()
        } else {
            current.len()
        };
        let mut decided = false;
        while position < shorter {
            if previous[position] != current[position] {
                assert!(
                    previous[position] < current[position],
                    "KEYS must be sorted"
                );
                decided = true;
                break;
            }
            position += 1;
        }
        if !decided {
            assert!(previous.len() < current.len(), "KEYS must be sorted");
        }
        index += 1;
    }
};
