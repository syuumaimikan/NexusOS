//! Locks: who is working on which paths, and whether anybody has stopped.
//!
//! # A lock here is a claim, not a mechanism
//!
//! Nothing enforces these. The filesystem does not know about them and neither
//! agent's editor does. A lock is a note saying "I am working in here", and
//! its whole value is that the other party reads it before starting.
//!
//! That is worth being clear about, because it decides what this program does
//! when a lock is in the way: it **says so and stops**. It does not wait, it
//! does not retry, and above all it does not break the lock. Breaking another
//! agent's lock is the one thing this protocol names as forbidden, and the
//! reason is not politeness — a lock is how each of us knows the other's work
//! is not about to be overwritten, and a lock that can be taken away is not
//! that.
//!
//! # Stale locks
//!
//! An agent that stops without releasing leaves a lock forever. So a lock is
//! stale when nothing has touched it for [`STALE_AFTER`], and this **reports**
//! stale locks loudly and still refuses to release them. Reporting is the
//! useful half: the other agent, or a person, can then decide. Deciding for
//! them is how the forbidden thing happens by accident.
//!
//! `heartbeat` refreshes every lock the running agent holds, so a lock that
//! goes stale means its holder really has stopped.

use nexus_json::Value;

use crate::clock::{self, Moment};
use crate::store::{trouble, Answer, Store};

/// How long a lock may go untouched before it is called stale.
///
/// Four hours. Long enough that a long piece of work with no heartbeats in the
/// middle is not reported, short enough that a session that ended yesterday is.
pub const STALE_AFTER: i64 = 4 * 60 * 60;

/// Where locks live.
pub const DIRECTORY: &str = "locks";

/// One lock, as it is on disk.
#[derive(Debug, Clone)]
pub struct Lock {
    pub agent: String,
    pub task: String,
    pub paths: Vec<String>,
    pub created_at: Option<i64>,
    /// When the holder last said they were still there, if ever.
    pub beat_at: Option<i64>,
    /// The file it came from.
    pub file: String,
}

impl Lock {
    /// How long since anything touched this, or `None` if it does not say.
    pub fn quiet_for(&self) -> Option<i64> {
        let last = self.beat_at.or(self.created_at)?;
        Some(clock::unix_now().saturating_sub(last))
    }

    /// Whether nothing has touched it for [`STALE_AFTER`].
    ///
    /// A lock with no readable timestamp is **not** stale. It is unknown, and
    /// treating unknown as stale is how a tool talks somebody into breaking a
    /// lock that was fine.
    pub fn is_stale(&self) -> bool {
        self.quiet_for().is_some_and(|quiet| quiet > STALE_AFTER)
    }

    /// Whether this lock covers `path`.
    ///
    /// Prefix matching on path segments. `kernel/` covers `kernel/src/x.rs`;
    /// `shared/nexus-ai` does not cover `shared/nexus-ai-core` even though one
    /// string starts with the other, which is the trap this avoids.
    pub fn covers(&self, path: &str) -> bool {
        self.paths.iter().any(|held| covers(held, path))
    }
}

/// Whether a claimed path covers a given one.
pub fn covers(held: &str, path: &str) -> bool {
    let held = held.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    if held.is_empty() {
        return true;
    }
    if path == held {
        return true;
    }
    // The segment boundary is what makes `nexus-ai` not cover `nexus-ai-core`.
    path.strip_prefix(held)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Read one lock file.
fn read_one(store: &Store, name: &str) -> Answer<Lock> {
    let relative = format!("{DIRECTORY}/{name}");
    let value = store.read_json(&relative)?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);

    Ok(Lock {
        agent: text("agent").ok_or_else(|| trouble!("{relative} does not say whose it is"))?,
        task: text("task").unwrap_or_else(|| name.trim_end_matches(".json").to_string()),
        paths: value
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        created_at: text("created_at").as_deref().and_then(clock::parse),
        beat_at: text("heartbeat_at").as_deref().and_then(clock::parse),
        file: name.to_string(),
    })
}

/// Every lock, in file order.
///
/// A lock file that will not parse is reported and skipped rather than failing
/// the whole listing: one bad file must not stop somebody seeing the others,
/// which are what they are about to rely on.
pub fn all(store: &Store) -> Answer<Vec<Lock>> {
    let mut locks = Vec::new();
    for name in store.list(DIRECTORY)? {
        if !name.ends_with(".json") {
            continue;
        }
        match read_one(store, &name) {
            Ok(lock) => locks.push(lock),
            Err(why) => eprintln!("note: {why}"),
        }
    }
    Ok(locks)
}

/// Take a lock on some paths for a task.
///
/// Fails if another agent holds any of them. Succeeds, and replaces, if the
/// asking agent already holds that task's lock -- which is how paths are added
/// to a claim already made.
pub fn take(store: &Store, agent: &str, task: &str, paths: &[String]) -> Answer<Lock> {
    if paths.is_empty() {
        return Err(trouble!("a lock with no paths in it claims nothing"));
    }

    for existing in all(store)? {
        if existing.agent == agent && existing.task == task {
            continue;
        }
        for wanted in paths {
            // The one path that clashes, not the whole claim. A lock over five
            // directories reported all five, and the reader then had to work
            // out which of them was the problem.
            let against = existing
                .paths
                .iter()
                .find(|held| covers(held, wanted) || covers(wanted, held));
            if let Some(against) = against {
                let age = existing
                    .quiet_for()
                    .map(|quiet| format!(", untouched for {}", clock::describe(quiet)))
                    .unwrap_or_default();
                let stale = if existing.is_stale() {
                    "\nThat lock is stale. It is still not this program's to break: \
                     ask its holder, or ask a person."
                } else {
                    ""
                };
                return Err(trouble!(
                    "{wanted} clashes with {against}, held by {holder} for {held_task}{age}{stale}",
                    holder = existing.agent,
                    held_task = existing.task,
                ));
            }
        }
    }

    let now = Moment::now().stamp();
    let mut value = Value::object();
    value.set("agent", Value::string(agent));
    value.set("task", Value::string(task));
    let mut listed = Value::array();
    for path in paths {
        listed.push(Value::string(path.as_str()));
    }
    value.set("paths", listed);
    value.set("created_at", Value::string(now.clone()));
    value.set("heartbeat_at", Value::string(now));

    let name = format!("{task}.json");
    store.write_json(&format!("{DIRECTORY}/{name}"), &value)?;
    read_one(store, &name)
}

/// Give a lock back.
///
/// Refuses a lock somebody else holds. There is no flag to override it, and
/// that is deliberate: a `--force` would be used, and the thing it forces is
/// the one thing this protocol says neither agent may do.
pub fn release(store: &Store, agent: &str, task: &str) -> Answer<Lock> {
    let name = format!("{task}.json");
    let lock = read_one(store, &name).map_err(|_| trouble!("there is no lock for {task}"))?;

    if lock.agent != agent {
        let age = lock
            .quiet_for()
            .map(|quiet| format!(" It has been untouched for {}.", clock::describe(quiet)))
            .unwrap_or_default();
        return Err(trouble!(
            "{task} is {holder}'s lock, not {agent}'s.{age}\n\
             This program will not release another agent's lock. If it is in your way, \
             say so in a request: .ai_collaboration/{to}/",
            holder = lock.agent,
            to = if agent == "claude_code" {
                "claude_to_astra"
            } else {
                "astra_to_claude"
            },
        ));
    }

    let path = store.at(&format!("{DIRECTORY}/{name}"));
    std::fs::remove_file(&path)
        .map_err(|why| trouble!("{} could not be removed: {why}", path.display()))?;
    Ok(lock)
}

/// Touch every lock an agent holds, so they do not go stale.
///
/// Returns how many were touched.
pub fn beat(store: &Store, agent: &str) -> Answer<usize> {
    let now = Moment::now().stamp();
    let mut touched = 0;
    for lock in all(store)? {
        if lock.agent != agent {
            continue;
        }
        let relative = format!("{DIRECTORY}/{}", lock.file);
        let mut value = store.read_json(&relative)?;
        value.set("heartbeat_at", Value::string(now.clone()));
        store.write_json(&relative, &value)?;
        touched += 1;
    }
    Ok(touched)
}

/// Who, if anyone, holds a path.
pub fn holder(store: &Store, path: &str) -> Answer<Option<Lock>> {
    Ok(all(store)?.into_iter().find(|lock| lock.covers(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("nexus-collab-lock-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(path.join(".ai_collaboration").join(DIRECTORY)).unwrap();
            Self { path }
        }

        fn store(&self) -> Store {
            Store::find(&self.path).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|it| it.to_string()).collect()
    }

    #[test]
    fn a_directory_covers_what_is_in_it() {
        assert!(covers("kernel/", "kernel/src/main.rs"));
        assert!(covers("kernel", "kernel/src/main.rs"));
        assert!(covers("kernel/src/main.rs", "kernel/src/main.rs"));
        assert!(!covers("kernel/", "user/main.rs"));
    }

    #[test]
    fn a_name_that_starts_the_same_is_not_inside() {
        // The one that would actually happen here: `shared/nexus-ai` is
        // Astra's and `shared/nexus-ai-core`... is also Astra's, but
        // `shared/nexus-image` and `shared/nexus-im` would not be, and a
        // string prefix test says they are.
        assert!(!covers("shared/nexus-ai", "shared/nexus-ai-core"));
        assert!(!covers("shared/nexus-im", "shared/nexus-image/src/lib.rs"));
        assert!(covers("shared/nexus-ai", "shared/nexus-ai/src/lib.rs"));
    }

    #[test]
    fn a_lock_can_be_taken_and_given_back() {
        let scratch = Scratch::new("take");
        let store = scratch.store();
        let lock = take(&store, "claude_code", "CLAUDE-9", &paths(&["kernel/"])).unwrap();
        assert_eq!(lock.agent, "claude_code");
        assert_eq!(all(&store).unwrap().len(), 1);

        release(&store, "claude_code", "CLAUDE-9").unwrap();
        assert!(all(&store).unwrap().is_empty());
    }

    #[test]
    fn another_agents_paths_cannot_be_taken() {
        let scratch = Scratch::new("clash");
        let store = scratch.store();
        take(
            &store,
            "gpt6_astra",
            "ASTRA-1",
            &paths(&["shared/nexus-ai/"]),
        )
        .unwrap();

        let why = take(
            &store,
            "claude_code",
            "CLAUDE-1",
            &paths(&["shared/nexus-ai/src/lib.rs"]),
        )
        .unwrap_err()
        .to_string();
        assert!(why.contains("gpt6_astra"), "{why}");
        assert!(why.contains("ASTRA-1"), "{why}");
    }

    #[test]
    fn a_clash_is_found_from_either_direction() {
        // Asking for a directory that contains something already held is as
        // much of a clash as asking for something inside one.
        let scratch = Scratch::new("bothways");
        let store = scratch.store();
        take(
            &store,
            "gpt6_astra",
            "ASTRA-1",
            &paths(&["shared/nexus-ai/src/lib.rs"]),
        )
        .unwrap();
        assert!(take(&store, "claude_code", "CLAUDE-1", &paths(&["shared/"])).is_err());
    }

    #[test]
    fn adding_a_path_to_your_own_lock_is_not_a_clash() {
        let scratch = Scratch::new("extend");
        let store = scratch.store();
        take(&store, "claude_code", "CLAUDE-1", &paths(&["kernel/"])).unwrap();
        let lock = take(
            &store,
            "claude_code",
            "CLAUDE-1",
            &paths(&["kernel/", "user/"]),
        )
        .unwrap();
        assert_eq!(lock.paths.len(), 2);
        assert_eq!(
            all(&store).unwrap().len(),
            1,
            "it replaced rather than added"
        );
    }

    #[test]
    fn another_agents_lock_cannot_be_released() {
        let scratch = Scratch::new("norelease");
        let store = scratch.store();
        take(&store, "gpt6_astra", "ASTRA-1", &paths(&["docs/AI/"])).unwrap();

        let why = release(&store, "claude_code", "ASTRA-1")
            .unwrap_err()
            .to_string();
        assert!(why.contains("gpt6_astra"), "{why}");
        assert!(why.contains("will not release"), "{why}");
        // And it is still there.
        assert_eq!(all(&store).unwrap().len(), 1);
    }

    #[test]
    fn a_stale_lock_is_reported_and_still_not_broken() {
        let scratch = Scratch::new("stale");
        let store = scratch.store();
        // A lock from long ago, written directly.
        let old = Moment::from_unix(clock::unix_now() - STALE_AFTER - 60).stamp();
        let mut value = Value::object();
        value.set("agent", Value::string("gpt6_astra"));
        value.set("task", Value::string("ASTRA-OLD"));
        let mut listed = Value::array();
        listed.push(Value::string("docs/"));
        value.set("paths", listed);
        value.set("created_at", Value::string(old));
        store
            .write_json(&format!("{DIRECTORY}/ASTRA-OLD.json"), &value)
            .unwrap();

        let lock = &all(&store).unwrap()[0];
        assert!(lock.is_stale());

        // Reported when it blocks something...
        let why = take(&store, "claude_code", "CLAUDE-1", &paths(&["docs/x.md"]))
            .unwrap_err()
            .to_string();
        assert!(why.contains("stale"), "{why}");
        // ...and still not this program's to break.
        assert!(release(&store, "claude_code", "ASTRA-OLD").is_err());
        assert_eq!(all(&store).unwrap().len(), 1);
    }

    #[test]
    fn a_lock_with_no_timestamp_is_unknown_rather_than_stale() {
        let scratch = Scratch::new("undated");
        let store = scratch.store();
        let mut value = Value::object();
        value.set("agent", Value::string("gpt6_astra"));
        value.set("task", Value::string("ASTRA-X"));
        value.set("paths", Value::array());
        store
            .write_json(&format!("{DIRECTORY}/ASTRA-X.json"), &value)
            .unwrap();

        let lock = &all(&store).unwrap()[0];
        assert_eq!(lock.quiet_for(), None);
        assert!(!lock.is_stale(), "unknown age must not read as stale");
    }

    #[test]
    fn a_heartbeat_touches_only_your_own_locks() {
        let scratch = Scratch::new("beat");
        let store = scratch.store();
        take(&store, "claude_code", "CLAUDE-1", &paths(&["kernel/"])).unwrap();
        take(&store, "gpt6_astra", "ASTRA-1", &paths(&["docs/AI/"])).unwrap();

        // Age both by rewriting their timestamps.
        let old = Moment::from_unix(clock::unix_now() - STALE_AFTER - 60).stamp();
        for task in ["CLAUDE-1", "ASTRA-1"] {
            let relative = format!("{DIRECTORY}/{task}.json");
            let mut value = store.read_json(&relative).unwrap();
            value.set("heartbeat_at", Value::string(old.clone()));
            store.write_json(&relative, &value).unwrap();
        }

        assert_eq!(beat(&store, "claude_code").unwrap(), 1);
        for lock in all(&store).unwrap() {
            match lock.agent.as_str() {
                "claude_code" => assert!(!lock.is_stale(), "the beat should have refreshed it"),
                _ => assert!(lock.is_stale(), "somebody else's lock must not be touched"),
            }
        }
    }

    #[test]
    fn a_lock_on_nothing_is_refused() {
        let scratch = Scratch::new("empty");
        let store = scratch.store();
        assert!(take(&store, "claude_code", "CLAUDE-1", &[]).is_err());
    }

    #[test]
    fn the_holder_of_a_path_can_be_asked_for() {
        let scratch = Scratch::new("holder");
        let store = scratch.store();
        take(
            &store,
            "gpt6_astra",
            "ASTRA-1",
            &paths(&["shared/nexus-ai/"]),
        )
        .unwrap();
        let found = holder(&store, "shared/nexus-ai/src/wire.rs").unwrap();
        assert_eq!(found.map(|lock| lock.agent), Some("gpt6_astra".to_string()));
        assert!(holder(&store, "kernel/src/main.rs").unwrap().is_none());
    }
}
