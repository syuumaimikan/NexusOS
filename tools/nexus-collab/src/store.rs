//! The directory itself: reading it, and writing it without ever leaving it
//! half-written.
//!
//! # Why atomically
//!
//! Two agents write here, neither knowing when the other will. A `STATE.json`
//! caught part-written is not a file with one bad field — it is a file that
//! does not parse, and the next thing either agent does is fail to read the
//! shared state. That has to be impossible rather than unlikely.
//!
//! So nothing is ever written in place. A new file is written beside the
//! target, flushed to the disk, and renamed over it. A rename within one
//! directory is atomic on both NTFS and every filesystem this project cares
//! about: a reader sees the old file or the new one, never a mixture.
//!
//! # Why backups
//!
//! Atomic writes stop a *torn* file. They do not stop a *wrong* one: a bad
//! edit, or a merge resolved badly, replaces the state completely and
//! correctly. So every write of `STATE.json` first copies what is there into
//! `backups/`, and `nexus-collab recover` lists them and puts one back.
//!
//! Sixteen of them, because they are two kilobytes each and the question a
//! person asks is "what did this look like before today", not "what did it
//! look like last year".

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use nexus_json::Value;

use crate::clock::Moment;

/// How many old copies of `STATE.json` to keep.
pub const BACKUPS: usize = 16;

/// Something that went wrong, said the way a person would say it.
#[derive(Debug)]
pub struct Trouble(pub String);

impl std::fmt::Display for Trouble {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

impl std::error::Error for Trouble {}

/// Shorthand for the result of anything that touches the directory.
pub type Answer<T> = Result<T, Trouble>;

/// Build a [`Trouble`] the way `format!` builds a string.
macro_rules! trouble {
    ($($argument:tt)*) => { $crate::store::Trouble(format!($($argument)*)) };
}
pub(crate) use trouble;

/// The `.ai_collaboration` directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Find the directory, starting at `from` and walking up.
    ///
    /// Walking up rather than requiring the repository root, because the
    /// natural place to run this from is wherever the work is.
    pub fn find(from: &Path) -> Answer<Self> {
        let mut at = from.to_path_buf();
        loop {
            let candidate = at.join(".ai_collaboration");
            if candidate.is_dir() {
                return Ok(Self { root: candidate });
            }
            if !at.pop() {
                return Err(trouble!(
                    "no .ai_collaboration directory here or in any directory above {}",
                    from.display()
                ));
            }
        }
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A path inside it.
    pub fn at(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Read a JSON file.
    ///
    /// A leading byte-order mark is skipped. Nothing this program writes has
    /// one, but these files get edited by hand and by other people's tools:
    /// Windows PowerShell's `Set-Content -Encoding utf8` writes one, as does
    /// Notepad, and a parser that refuses the result is a parser that says
    /// "this is not the start of a value at byte 0" about a file that looks
    /// perfectly fine in every editor.
    pub fn read_json(&self, relative: &str) -> Answer<Value> {
        let path = self.at(relative);
        let text = fs::read_to_string(&path)
            .map_err(|why| trouble!("{} could not be read: {why}", path.display()))?;
        nexus_json::parse(text.trim_start_matches('\u{feff}'))
            .map_err(|why| trouble!("{} is not valid JSON: {why}", path.display()))
    }

    /// Read a JSON file, or `None` if it is not there.
    ///
    /// Absent and unreadable are different, and conflating them is how a tool
    /// deletes things: a file that cannot be read because the disk is busy
    /// must not be treated as a file that says nothing.
    pub fn read_json_if_there(&self, relative: &str) -> Answer<Option<Value>> {
        if !self.at(relative).exists() {
            return Ok(None);
        }
        self.read_json(relative).map(Some)
    }

    /// Write a JSON file, atomically.
    pub fn write_json(&self, relative: &str, value: &Value) -> Answer<()> {
        self.write_bytes(relative, format!("{}\n", value.to_pretty()).as_bytes())
    }

    /// Write a file, atomically: a temporary beside it, flushed, then renamed.
    pub fn write_bytes(&self, relative: &str, bytes: &[u8]) -> Answer<()> {
        let path = self.at(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|why| trouble!("{} could not be made: {why}", parent.display()))?;
        }

        // Beside the target, not in a temporary directory: a rename across
        // filesystems is a copy, and a copy is not atomic.
        let temporary = path.with_extension(format!(
            "{}.writing",
            path.extension().and_then(|it| it.to_str()).unwrap_or("tmp")
        ));
        {
            let mut file = fs::File::create(&temporary)
                .map_err(|why| trouble!("{} could not be made: {why}", temporary.display()))?;
            file.write_all(bytes)
                .map_err(|why| trouble!("{} could not be written: {why}", temporary.display()))?;
            // Flushed before the rename. Without this the rename can land
            // while the bytes are still in the cache, and a machine that loses
            // power between the two has a name pointing at an empty file --
            // which is worse than the torn write this was meant to prevent.
            file.sync_all()
                .map_err(|why| trouble!("{} could not be flushed: {why}", temporary.display()))?;
        }

        fs::rename(&temporary, &path).map_err(|why| {
            // Leave the temporary where it is. Its contents are the work that
            // was about to be saved, and deleting it here would throw that
            // away at exactly the moment somebody needs it.
            trouble!(
                "{} could not be put in place: {why}. What was to be written is in {}",
                path.display(),
                temporary.display()
            )
        })
    }

    /// Copy `STATE.json` into `backups/` before it is replaced.
    ///
    /// Returns the name of the copy, or `None` if there was nothing to copy.
    ///
    /// The name carries a counter as well as the time, because the timestamp
    /// is only to the second and four commands in one second is an ordinary
    /// thing. Without the counter the second of those overwrote the first
    /// one's copy -- which is to say it threw away the *older* state and kept
    /// the newer, and older is the one somebody recovering wants.
    ///
    /// The counter is two digits and comes after the stamp, so the names still
    /// sort by time as plain strings. That matters: [`Self::backups`] sorts by
    /// name rather than by modification time, because copying the directory
    /// resets modification times and does not touch names.
    pub fn back_up_state(&self) -> Answer<Option<String>> {
        let state = self.at("STATE.json");
        if !state.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&state)
            .map_err(|why| trouble!("{} could not be read to back up: {why}", state.display()))?;

        let stamp = Moment::now().filename_stamp();
        let mut name = None;
        for counter in 0..100 {
            let candidate = format!("STATE-{stamp}-{counter:02}.json");
            if !self.at(&format!("backups/{candidate}")).exists() {
                name = Some(candidate);
                break;
            }
        }
        // A hundred writes inside one second is not a thing this program does,
        // and if it ever happened the right answer is still to save the state
        // rather than to refuse the write over where to put a copy of it.
        let name = name.unwrap_or_else(|| format!("STATE-{stamp}-99.json"));

        self.write_bytes(&format!("backups/{name}"), &bytes)?;
        self.trim_backups()?;
        Ok(Some(name))
    }

    /// Every backup, newest first.
    ///
    /// By name, which sorts by time because the names are timestamps in a
    /// format that does. Modification times would be the obvious thing and are
    /// the wrong thing: copying the directory resets them all.
    pub fn backups(&self) -> Answer<Vec<String>> {
        let directory = self.at("backups");
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = fs::read_dir(&directory)
            .map_err(|why| trouble!("{} could not be listed: {why}", directory.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with("STATE-") && name.ends_with(".json"))
            .collect();
        names.sort();
        names.reverse();
        Ok(names)
    }

    /// Keep the newest [`BACKUPS`] and delete the rest.
    fn trim_backups(&self) -> Answer<()> {
        for name in self.backups()?.into_iter().skip(BACKUPS) {
            let path = self.at(&format!("backups/{name}"));
            // A backup that will not delete is not worth failing a write over.
            // Said, and then carried on.
            if let Err(why) = fs::remove_file(&path) {
                eprintln!("note: {} could not be removed: {why}", path.display());
            }
        }
        Ok(())
    }

    /// The names of the files in a subdirectory, sorted.
    pub fn list(&self, relative: &str) -> Answer<Vec<String>> {
        let directory = self.at(relative);
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = fs::read_dir(&directory)
            .map_err(|why| trouble!("{} could not be listed: {why}", directory.display()))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in a directory of its own, removed when the test ends.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("nexus-collab-test-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join(".ai_collaboration")).unwrap();
            Self { path }
        }

        fn store(&self) -> Store {
            Store::find(&self.path).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn the_directory_is_found_from_below_it() {
        let scratch = Scratch::new("found");
        let deep = scratch.path.join("one").join("two").join("three");
        fs::create_dir_all(&deep).unwrap();
        let store = Store::find(&deep).unwrap();
        assert_eq!(store.root(), scratch.path.join(".ai_collaboration"));
    }

    #[test]
    fn not_finding_it_says_so_rather_than_making_one() {
        // A tool that helpfully created the directory would create it in the
        // wrong place the first time somebody ran it from their home folder,
        // and the two agents would then be talking past each other.
        let empty = std::env::temp_dir().join("nexus-collab-test-nothing-here");
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();
        // Only if no ancestor happens to have one, which a temporary directory
        // will not.
        if Store::find(&empty).is_ok() {
            // A temporary directory that happens to sit under one. Nothing to
            // test here, and failing would be testing the machine rather than
            // the code.
            let _ = fs::remove_dir_all(&empty);
            return;
        }
        let Err(why) = Store::find(&empty) else {
            unreachable!("just checked that it is not found")
        };
        let why = why.to_string();
        assert!(why.contains(".ai_collaboration"), "{why}");
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn a_written_file_reads_back() {
        let scratch = Scratch::new("roundtrip");
        let store = scratch.store();
        let mut value = Value::object();
        value.set("schema_version", Value::number(1));
        value.set("who", Value::string("claude_code"));
        store.write_json("STATE.json", &value).unwrap();

        let back = store.read_json("STATE.json").unwrap();
        assert_eq!(back.get("who").and_then(Value::as_str), Some("claude_code"));
        assert_eq!(back.get("schema_version").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn nothing_is_left_beside_a_file_that_was_written() {
        // The temporary has to be gone. One left behind would be committed by
        // somebody eventually, and a `STATE.json.writing` in the repository is
        // a thing nobody can tell is rubbish.
        let scratch = Scratch::new("tidy");
        let store = scratch.store();
        store.write_json("STATE.json", &Value::object()).unwrap();
        let left: Vec<String> = store.list("").unwrap();
        assert_eq!(left, vec!["STATE.json".to_string()], "left: {left:?}");
    }

    #[test]
    fn a_byte_order_mark_does_not_make_a_file_unreadable() {
        // Found by the end-to-end test, which wrote its fixture with
        // PowerShell's `Set-Content -Encoding utf8` -- and that writes a BOM.
        // Every command then failed with "this is not the start of a value at
        // byte 0" about a file that reads perfectly in any editor.
        let scratch = Scratch::new("bom");
        let store = scratch.store();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"schema_version": 1}"#);
        fs::write(store.at("STATE.json"), bytes).unwrap();

        let state = store.read_json("STATE.json").unwrap();
        assert_eq!(state.get("schema_version").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn absent_and_unreadable_are_different() {
        let scratch = Scratch::new("absent");
        let store = scratch.store();
        assert!(store.read_json_if_there("STATE.json").unwrap().is_none());

        fs::write(store.at("STATE.json"), b"{ this is not json").unwrap();
        // Present and broken is an error, not an empty answer. Treating it as
        // empty is how a tool overwrites a state file it merely failed to
        // parse.
        assert!(store.read_json_if_there("STATE.json").is_err());
    }

    #[test]
    fn a_backup_is_taken_before_the_state_is_replaced() {
        let scratch = Scratch::new("backup");
        let store = scratch.store();

        let mut first = Value::object();
        first.set("mark", Value::string("first"));
        store.write_json("STATE.json", &first).unwrap();

        let name = store.back_up_state().unwrap().expect("there was a state");
        let mut second = Value::object();
        second.set("mark", Value::string("second"));
        store.write_json("STATE.json", &second).unwrap();

        let saved = store.read_json(&format!("backups/{name}")).unwrap();
        assert_eq!(saved.get("mark").and_then(Value::as_str), Some("first"));
        let now = store.read_json("STATE.json").unwrap();
        assert_eq!(now.get("mark").and_then(Value::as_str), Some("second"));
    }

    #[test]
    fn backing_up_nothing_is_not_an_error() {
        let scratch = Scratch::new("nobackup");
        assert!(scratch.store().back_up_state().unwrap().is_none());
    }

    #[test]
    fn two_backups_in_one_second_do_not_become_one() {
        // Four commands in a second is ordinary. Before the counter, the
        // second one's copy landed on the first one's name -- so the state
        // before the burst, which is the one worth having, was the one lost.
        let scratch = Scratch::new("samesecond");
        let store = scratch.store();

        for mark in ["first", "second", "third"] {
            let mut value = Value::object();
            value.set("mark", Value::string(mark));
            store.back_up_state().unwrap();
            store.write_json("STATE.json", &value).unwrap();
        }

        // Three writes; the first had nothing to copy, so two copies.
        let names = store.backups().unwrap();
        assert_eq!(names.len(), 2, "{names:?}");

        // Newest first, and the oldest copy is the earliest state.
        let oldest = store.read_json(&format!("backups/{}", names[1])).unwrap();
        assert_eq!(oldest.get("mark").and_then(Value::as_str), Some("first"));
        let newest = store.read_json(&format!("backups/{}", names[0])).unwrap();
        assert_eq!(newest.get("mark").and_then(Value::as_str), Some("second"));
    }

    #[test]
    fn old_backups_are_trimmed_and_the_newest_are_kept() {
        let scratch = Scratch::new("trim");
        let store = scratch.store();
        store.write_json("STATE.json", &Value::object()).unwrap();

        // Written directly rather than through back_up_state, which would
        // give them all the same timestamp inside one test.
        for number in 0..BACKUPS + 6 {
            let name = format!(
                "backups/STATE-2026010{}T{:06}Z-00.json",
                number / 10,
                number
            );
            store
                .write_json(&name, &Value::number(number as i64))
                .unwrap();
        }
        store.trim_backups().unwrap();

        let left = store.backups().unwrap();
        assert_eq!(left.len(), BACKUPS);
        // Newest first, and the newest is the highest-numbered.
        assert!(left[0] > left[BACKUPS - 1], "{left:?} is not newest first");
    }
}
