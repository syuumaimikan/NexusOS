//! What is installed, what is available, and which of the two is newer.
//!
//! Everything here is arithmetic on version numbers and text. Nothing in it
//! touches a disk or a network, which is the point: deciding whether `1.10.0`
//! supersedes `1.9.3` is the part of updating a machine that has to be right
//! every time, and it is also the only part that can be tested on a desk
//! without booting anything.
//!
//! # What a version is
//!
//! Three numbers, `major.minor.patch`, compared as numbers and not as text.
//! That distinction is the whole reason this is a type rather than a string
//! comparison: `"1.10.0" < "1.9.3"` is true of text and false of versions, and
//! a machine that got it the wrong way round would refuse the update that
//! mattered and offer the one it already had.
//!
//! A missing component is zero, so `"2"` and `"2.0"` and `"2.0.0"` are the same
//! version. A component that is not a number makes the whole thing unreadable,
//! because a version nobody can parse is not a version somebody should be
//! guessing at.
//!
//! # What this deliberately is not
//!
//! There is no build metadata, no pre-release ordering, no epoch. Those exist in
//! package systems that have had to represent decades of other people's
//! decisions. This system has made none of those yet, and inventing the
//! machinery for them now would be inventing the bugs too.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A three-part version number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version {
    /// Ordered first, which is what makes the derived ordering the right one.
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// The version this text names, or `None` if it does not name one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let mut parts = text.split('.');
        let mut next = || -> Option<u32> {
            match parts.next() {
                // Absent is zero: "2" is version two point nothing.
                None => Some(0),
                Some(part) => part.trim().parse::<u32>().ok(),
            }
        };
        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        // Anything after the third component is a version this does not
        // understand, and understanding it wrongly is worse than saying so.
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// How it is written down.
    #[must_use]
    pub fn to_text(self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What is installed on this machine, by package name.
///
/// Stored as `name = version`, one per line, in the same shape as every other
/// text file this system keeps. A name that appears twice keeps its last value,
/// because that is what re-reading a file somebody has appended to should do.
#[derive(Default)]
pub struct Installed {
    entries: Vec<(String, Version)>,
}

impl Installed {
    /// Nothing installed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Read the record.
    ///
    /// Lines that are blank, commented, or unreadable are skipped rather than
    /// rejected. A record with one bad line in it still says something true
    /// about every other package, and refusing the whole file would turn one
    /// typo into a machine that reinstalls everything.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut record = Self::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                continue;
            };
            let Some(version) = Version::parse(value) else {
                continue;
            };
            record.set(name.trim(), version);
        }
        record
    }

    /// What version of `name` is installed, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Version> {
        self.entries
            .iter()
            .find(|(installed, _)| installed == name)
            .map(|(_, version)| *version)
    }

    /// Record that `name` is now at `version`.
    pub fn set(&mut self, name: &str, version: Version) {
        for entry in &mut self.entries {
            if entry.0 == name {
                entry.1 = version;
                return;
            }
        }
        self.entries.push((String::from(name), version));
    }

    /// How many packages are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write the record out, sorted by name so that two runs which installed the
    /// same things produce the same file.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut sorted: Vec<&(String, Version)> = self.entries.iter().collect();
        sorted.sort_by(|left, right| left.0.cmp(&right.0));
        let mut out = String::from("# What is installed on this machine.\n");
        for (name, version) in sorted {
            out.push_str(&format!("{name} = {}\n", version.to_text()));
        }
        out
    }
}

/// What should happen to one package that is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing is installed under that name yet.
    New,
    /// What is available is newer than what is installed.
    Newer,
    /// What is installed is the same version.
    Current,
    /// What is installed is newer than what is on offer.
    ///
    /// Not an error and not something to act on. It is what a machine sees when
    /// it is handed an older image than the one it is running, and quietly
    /// installing it would be a downgrade nobody asked for.
    Older,
}

impl Verdict {
    /// Whether this is a reason to install.
    #[must_use]
    pub const fn worth_doing(self) -> bool {
        matches!(self, Self::New | Self::Newer)
    }

    /// How to say it.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Newer => "newer",
            Self::Current => "already current",
            Self::Older => "older than what is installed",
        }
    }
}

/// What to do about `available`, given what is installed.
#[must_use]
pub fn judge(installed: Option<Version>, available: Version) -> Verdict {
    match installed {
        None => Verdict::New,
        Some(installed) if available > installed => Verdict::Newer,
        Some(installed) if available == installed => Verdict::Current,
        Some(_) => Verdict::Older,
    }
}

/// Which of what is offered to actually consider, when names repeat.
///
/// A directory can hold two releases of the same package -- it does the moment
/// an update arrives beside the release it supersedes -- and a machine that
/// installed both would install them in whatever order the directory happened
/// to list them. Half the time that ends with the newer one on disk and half
/// the time with the older, which is a machine whose version depends on how its
/// filesystem sorts.
///
/// So one per name, the highest, and the rest are not candidates at all. Ties
/// keep the first, because two files claiming the same name and version are
/// claiming to be the same package and installing either satisfies that claim.
///
/// Returns the indices to keep, in the order they were offered.
#[must_use]
pub fn newest_of_each(offered: &[(&str, Version)]) -> Vec<usize> {
    let mut kept: Vec<usize> = Vec::new();
    for (index, (name, version)) in offered.iter().enumerate() {
        match kept.iter().position(|other| offered[*other].0 == *name) {
            Some(slot) => {
                if *version > offered[kept[slot]].1 {
                    kept[slot] = index;
                }
            }
            None => kept.push(index),
        }
    }
    kept.sort_unstable();
    kept
}

/// Whether a machine should install what it finds without being asked.
///
/// `automatic` by default, which is the answer that keeps a machine patched.
/// The alternative is `ask`, where the machine works out what is available,
/// writes it down, and waits. Anything else is read as `automatic`, because an
/// unreadable setting should not be a machine that silently stops updating.
#[must_use]
pub fn installs_by_itself(setting: Option<&str>) -> bool {
    !matches!(setting.map(str::trim), Some("ask") | Some("manual"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_three_numbers() {
        let version = Version::parse("1.2.3").expect("readable");
        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 2);
        assert_eq!(version.patch, 3);
        assert_eq!(version.to_text(), "1.2.3");
    }

    #[test]
    fn missing_components_are_zero() {
        assert_eq!(Version::parse("2"), Version::parse("2.0.0"));
        assert_eq!(Version::parse("2.1"), Version::parse("2.1.0"));
    }

    #[test]
    fn versions_compare_as_numbers_and_not_as_text() {
        let ten = Version::parse("1.10.0").expect("readable");
        let nine = Version::parse("1.9.3").expect("readable");
        // The whole reason this is a type: as text, "1.10.0" sorts first.
        assert!("1.10.0" < "1.9.3");
        assert!(ten > nine);
    }

    #[test]
    fn a_version_nobody_can_read_is_not_guessed_at() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("1.2.x"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("-1.0.0"), None);
    }

    #[test]
    fn the_record_round_trips() {
        let mut installed = Installed::new();
        installed.set("demo", Version::parse("1.0.0").expect("readable"));
        installed.set("system", Version::parse("0.2.1").expect("readable"));
        let text = installed.to_text();
        let read = Installed::parse(&text);
        assert_eq!(read.len(), 2);
        assert_eq!(read.get("demo"), Version::parse("1.0.0"));
        assert_eq!(read.get("system"), Version::parse("0.2.1"));
        assert_eq!(read.get("absent"), None);
    }

    #[test]
    fn setting_a_name_twice_replaces_it() {
        let mut installed = Installed::new();
        installed.set("demo", Version::parse("1.0.0").expect("readable"));
        installed.set("demo", Version::parse("1.1.0").expect("readable"));
        assert_eq!(installed.len(), 1);
        assert_eq!(installed.get("demo"), Version::parse("1.1.0"));
    }

    #[test]
    fn a_bad_line_costs_only_that_line() {
        let record = Installed::parse(
            "# a comment\n\
             good = 1.2.3\n\
             broken = not-a-version\n\
             \n\
             also_good = 2.0.0\n",
        );
        assert_eq!(record.len(), 2);
        assert_eq!(record.get("good"), Version::parse("1.2.3"));
        assert_eq!(record.get("broken"), None);
        assert_eq!(record.get("also_good"), Version::parse("2.0.0"));
    }

    #[test]
    fn the_verdicts() {
        let one = Version::parse("1.0.0").expect("readable");
        let two = Version::parse("2.0.0").expect("readable");
        assert_eq!(judge(None, one), Verdict::New);
        assert_eq!(judge(Some(one), two), Verdict::Newer);
        assert_eq!(judge(Some(one), one), Verdict::Current);
        assert_eq!(judge(Some(two), one), Verdict::Older);
        assert!(Verdict::New.worth_doing());
        assert!(Verdict::Newer.worth_doing());
        assert!(!Verdict::Current.worth_doing());
        // A downgrade is never worth doing on its own account.
        assert!(!Verdict::Older.worth_doing());
    }

    #[test]
    fn two_releases_of_one_package_leave_only_the_newer() {
        let one = Version::parse("1.0.0").expect("readable");
        let two = Version::parse("1.1.0").expect("readable");
        let three = Version::parse("0.9.0").expect("readable");
        let offered = [("demo", one), ("other", three), ("demo", two)];
        // The newer `demo`, and the unrelated package, and not the older one.
        assert_eq!(newest_of_each(&offered), alloc::vec![1, 2]);
    }

    #[test]
    fn the_order_they_are_offered_in_does_not_decide() {
        let one = Version::parse("1.0.0").expect("readable");
        let two = Version::parse("1.1.0").expect("readable");
        assert_eq!(
            newest_of_each(&[("demo", one), ("demo", two)]),
            alloc::vec![1]
        );
        assert_eq!(
            newest_of_each(&[("demo", two), ("demo", one)]),
            alloc::vec![0]
        );
    }

    #[test]
    fn a_tie_keeps_the_first() {
        let one = Version::parse("1.0.0").expect("readable");
        assert_eq!(
            newest_of_each(&[("demo", one), ("demo", one)]),
            alloc::vec![0]
        );
    }

    #[test]
    fn a_machine_updates_itself_unless_told_otherwise() {
        assert!(installs_by_itself(None));
        assert!(installs_by_itself(Some("automatic")));
        assert!(installs_by_itself(Some("something unreadable")));
        assert!(!installs_by_itself(Some("ask")));
        assert!(!installs_by_itself(Some(" ask ")));
        assert!(!installs_by_itself(Some("manual")));
    }
}
