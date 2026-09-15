//! Who can write to whom, worked out rather than listed.
//!
//! # The bug this file exists to remove
//!
//! Two agents needed two directories, `claude_to_astra` and `astra_to_claude`,
//! and both names were written into the code. Then a third developer joined --
//! Gemini 3.1 Pro, on 2026-09-15 -- and every one of those lists was silently
//! wrong: `request list` looked in two places out of six and reported a clean
//! inbox to somebody who had mail.
//!
//! A silent wrong answer is the worst kind for this tool. Its whole job is to
//! tell three developers what the other two are doing, and an inbox that does
//! not list a request is indistinguishable from nobody having sent one.
//!
//! So nothing is listed here. A **mailbox is a directory named `<from>_to_<to>`**
//! and they are found by reading the collaboration directory. A fourth
//! developer needs a `mkdir` and no code change, which is the only arrangement
//! that stays true.
//!
//! # Matching a mailbox to an agent
//!
//! Agent identifiers are longer than the words in the directory names:
//! `claude_code`, `gpt6_astra`, `gemini_3_1_pro`. The rule is that **an agent
//! owns a side of a mailbox when its identifier contains that word**, so
//! `gpt6_astra` owns the `astra` side of `claude_to_astra`.
//!
//! That is deliberately loose. The alternative -- a table mapping identifiers
//! to nicknames -- is another list to forget to update, which is the thing
//! being fixed. The looseness costs nothing here because the words are chosen
//! by the people using them and an agent that matched two mailboxes would be
//! one that had been named carelessly.

use crate::store::{trouble, Answer, Store};

/// A one-way channel between two developers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// The directory, relative to the collaboration root.
    pub directory: String,
    /// Who writes into it.
    pub from: String,
    /// And who reads it.
    pub to: String,
}

impl Mailbox {
    /// Whether `agent` is the one who writes here.
    #[must_use]
    pub fn written_by(&self, agent: &str) -> bool {
        names(agent, &self.from)
    }

    /// Whether `agent` is the one who reads here.
    #[must_use]
    pub fn read_by(&self, agent: &str) -> bool {
        names(agent, &self.to)
    }
}

/// Whether an agent identifier names the party a mailbox calls `word`.
fn names(agent: &str, word: &str) -> bool {
    !word.is_empty() && agent.to_ascii_lowercase().contains(&word.to_ascii_lowercase())
}

/// Every mailbox in the collaboration directory.
///
/// Sorted, so that two runs list the same requests in the same order and a
/// difference between them is a difference in the directory.
///
/// # Errors
///
/// If the collaboration directory cannot be read at all.
pub fn all(store: &Store) -> Answer<Vec<Mailbox>> {
    let root = store.root();
    let entries = std::fs::read_dir(root)
        .map_err(|why| trouble!("{} could not be listed: {why}", root.display()))?;

    let mut found: Vec<Mailbox> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| split(&name))
        .collect();
    found.sort_by(|left, right| left.directory.cmp(&right.directory));
    Ok(found)
}

/// Every mailbox `agent` should be reading.
///
/// # Errors
///
/// If the collaboration directory cannot be read.
pub fn addressed_to(store: &Store, agent: &str) -> Answer<Vec<Mailbox>> {
    Ok(all(store)?
        .into_iter()
        .filter(|box_| box_.read_by(agent))
        .collect())
}

/// Where `from` writes to reach `to`, whether or not it exists yet.
#[must_use]
pub fn between(from: &str, to: &str) -> String {
    format!("{}_to_{}", short(from), short(to))
}

/// The word an agent is known by in a directory name.
///
/// The last part of the identifier that is not a version number, which turns
/// `claude_code` into `code`... and that is wrong, so it is not what this does.
/// It takes the *longest* part, which gives `claude`, `astra` and `gemini` from
/// the three identifiers in use. A tie-break on length is a guess, and it is
/// only ever used to *suggest* a directory in an error message -- never to find
/// one, which is done by reading the disk.
#[must_use]
pub fn short(agent: &str) -> String {
    agent
        .split(['_', '-'])
        .filter(|piece| !piece.is_empty() && !piece.chars().all(|c| c.is_ascii_digit()))
        .max_by_key(|piece| piece.len())
        .unwrap_or(agent)
        .to_ascii_lowercase()
}

/// Take a directory name apart, if it is a mailbox at all.
fn split(name: &str) -> Option<Mailbox> {
    let (from, to) = name.split_once("_to_")?;
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some(Mailbox {
        directory: name.to_string(),
        from: from.to_string(),
        to: to.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mailbox_name_comes_apart() {
        let box_ = split("claude_to_astra").expect("a mailbox");
        assert_eq!(box_.from, "claude");
        assert_eq!(box_.to, "astra");
    }

    #[test]
    fn directories_that_are_not_mailboxes_are_not_mailboxes() {
        // The collaboration directory has plenty of these in it.
        for name in ["backups", "locks", "tasks", "events", "decisions", "_to_"] {
            assert!(split(name).is_none(), "{name} is not a mailbox");
        }
    }

    #[test]
    fn an_agent_owns_the_side_its_name_is_on() {
        let box_ = split("claude_to_astra").expect("a mailbox");
        assert!(box_.written_by("claude_code"));
        assert!(box_.read_by("gpt6_astra"));
        assert!(!box_.read_by("claude_code"));
        assert!(!box_.written_by("gpt6_astra"));
    }

    #[test]
    fn a_third_developer_needs_no_change_here() {
        // The whole point. Gemini joined and these two directories have to work
        // without anybody editing this file.
        let to_gemini = split("claude_to_gemini").expect("a mailbox");
        assert!(to_gemini.written_by("claude_code"));
        assert!(to_gemini.read_by("gemini_3_1_pro"));

        let from_gemini = split("gemini_to_astra").expect("a mailbox");
        assert!(from_gemini.written_by("gemini_3_1_pro"));
        assert!(from_gemini.read_by("gpt6_astra"));
        assert!(!from_gemini.read_by("claude_code"));
    }

    #[test]
    fn the_short_name_is_the_one_people_would_choose() {
        assert_eq!(short("claude_code"), "claude");
        assert_eq!(short("gpt6_astra"), "astra");
        assert_eq!(short("gemini_3_1_pro"), "gemini");
    }

    #[test]
    fn between_names_a_directory_both_ways() {
        assert_eq!(between("claude_code", "gpt6_astra"), "claude_to_astra");
        assert_eq!(between("gpt6_astra", "claude_code"), "astra_to_claude");
        assert_eq!(
            between("gemini_3_1_pro", "claude_code"),
            "gemini_to_claude"
        );
    }

    #[test]
    fn an_empty_word_names_nobody() {
        // Guards the `contains` rule: every string contains the empty string,
        // so without this check an agent would own every side of every mailbox.
        assert!(!names("claude_code", ""));
    }
}
