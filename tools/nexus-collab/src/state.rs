//! `STATE.json`: what shape it has to be, and how it is changed.
//!
//! # Every change is read-modify-write, and that is the honest limit
//!
//! There is no locking on the file. Two agents editing it in the same second
//! will have one of them win, and the loser's edit is in `backups/`. This is
//! not ideal and it is not pretended otherwise: making it better needs either
//! a lock file with all of the stale-lock problems the task locks already
//! have, or a log of changes rather than a document.
//!
//! What is done instead is to make each change **small and local**: a command
//! reads the file, changes one field or appends one entry, and writes it
//! straight back. The window is milliseconds, the backup is always taken, and
//! `nexus-collab check` will say if the result stopped making sense.
//!
//! # What this refuses to do
//!
//! Edit another agent's record. `state` commands take the agent from
//! `--agent` or `NEXUS_AGENT` and write only to that one. The tool cannot stop
//! somebody passing the other agent's name, and it does not pretend to — what
//! it stops is doing it *by accident*, which is the way it would actually
//! happen.

use nexus_json::Value;

use crate::clock::Moment;
use crate::store::{trouble, Answer, Store};

/// The version of the shape this program understands.
pub const SCHEMA: i64 = 1;

/// Where the shared state lives inside the directory.
pub const FILE: &str = "STATE.json";

/// Read `STATE.json` and check it is the shape this expects.
pub fn read(store: &Store) -> Answer<Value> {
    let state = store.read_json(FILE)?;
    let complaints = check(&state);
    if !complaints.is_empty() {
        return Err(trouble!(
            "{FILE} is not usable:\n  {}\n\
             `nexus-collab check` lists everything; \
             `nexus-collab recover` puts a backup back.",
            complaints.join("\n  ")
        ));
    }
    Ok(state)
}

/// Everything wrong with a state document, in the order it was found.
///
/// A list rather than the first problem, because somebody fixing a file by
/// hand wants to see all of it, and because `check` is a command in its own
/// right.
pub fn check(state: &Value) -> Vec<String> {
    let mut complaints = Vec::new();

    let Some(_) = state.as_object() else {
        complaints.push("the whole document is not an object".to_string());
        return complaints;
    };

    match state.get("schema_version").and_then(Value::as_i64) {
        Some(SCHEMA) => {}
        Some(other) => complaints.push(format!(
            "schema_version is {other}; this program understands {SCHEMA}"
        )),
        None => complaints.push("there is no schema_version".to_string()),
    }

    match state.get("agents").and_then(Value::as_object) {
        Some(agents) if !agents.is_empty() => {
            for (name, record) in agents {
                if record.as_object().is_none() {
                    complaints.push(format!("the record for {name} is not an object"));
                    continue;
                }
                if record.get("status").and_then(Value::as_str).is_none() {
                    complaints.push(format!("{name} has no status"));
                }
                if let Some(paths) = record.get("active_paths") {
                    if paths.as_array().is_none() {
                        complaints.push(format!("{name}'s active_paths is not a list"));
                    }
                }
                if let Some(seen) = record.get("last_activity").and_then(Value::as_str) {
                    if crate::clock::parse(seen).is_none() {
                        complaints
                            .push(format!("{name}'s last_activity is not a timestamp: {seen}"));
                    }
                }
            }
        }
        Some(_) => complaints.push("there are no agents".to_string()),
        None => complaints.push("agents is missing or is not an object".to_string()),
    }

    match state.get("tasks").and_then(Value::as_array) {
        Some(tasks) => {
            let mut seen: Vec<&str> = Vec::new();
            for (number, task) in tasks.iter().enumerate() {
                let Some(id) = task.get("id").and_then(Value::as_str) else {
                    complaints.push(format!("task {number} has no id"));
                    continue;
                };
                if seen.contains(&id) {
                    // Two tasks with one id is the quiet one: every command
                    // that looks a task up finds the first, and edits to the
                    // second are invisible.
                    complaints.push(format!("{id} appears more than once"));
                }
                seen.push(id);
                if task.get("status").and_then(Value::as_str).is_none() {
                    complaints.push(format!("{id} has no status"));
                }
                if task.get("owner").and_then(Value::as_str).is_none() {
                    complaints.push(format!("{id} has no owner"));
                }
            }
            // Every dependency has to name a task that is here.
            let ids: Vec<&str> = tasks
                .iter()
                .filter_map(|task| task.get("id").and_then(Value::as_str))
                .collect();
            for task in tasks {
                let (Some(id), Some(dependencies)) = (
                    task.get("id").and_then(Value::as_str),
                    task.get("dependencies").and_then(Value::as_array),
                ) else {
                    continue;
                };
                for dependency in dependencies {
                    match dependency.as_str() {
                        Some(named) if ids.contains(&named) => {}
                        Some(named) => {
                            complaints.push(format!("{id} depends on {named}, which is not here"));
                        }
                        None => {
                            complaints.push(format!("{id} has a dependency that is not a name"))
                        }
                    }
                }
            }
        }
        None => complaints.push("tasks is missing or is not a list".to_string()),
    }

    if let Some(seen) = state.get("updated_at").and_then(Value::as_str) {
        if crate::clock::parse(seen).is_none() {
            complaints.push(format!("updated_at is not a timestamp: {seen}"));
        }
    }

    complaints
}

/// Write `STATE.json` back, having first backed it up and stamped it.
///
/// The stamp is set here rather than by each caller, because a caller that
/// forgot would leave the file saying it had not changed since the last time
/// somebody remembered.
pub fn write(store: &Store, state: &mut Value) -> Answer<()> {
    let complaints = check(state);
    if !complaints.is_empty() {
        // Refusing to write is the whole point: a command with a bug in it
        // should fail rather than replace the shared state with something
        // neither agent can read.
        return Err(trouble!(
            "refusing to write a state that does not make sense:\n  {}",
            complaints.join("\n  ")
        ));
    }
    state.set("updated_at", Value::string(Moment::now().stamp()));
    store.back_up_state()?;
    store.write_json(FILE, state)
}

/// The record for one agent, made if it was not there.
pub fn agent<'a>(state: &'a mut Value, name: &str) -> Answer<&'a mut Value> {
    let agents = state
        .get_mut("agents")
        .ok_or_else(|| trouble!("{FILE} has no agents"))?;
    if agents.get(name).is_none() {
        let mut fresh = Value::object();
        fresh.set("status", Value::string("idle"));
        fresh.set("current_task", Value::Null);
        fresh.set("active_paths", Value::array());
        fresh.set("last_activity", Value::string(Moment::now().stamp()));
        agents.set(name, fresh);
    }
    agents
        .get_mut(name)
        .ok_or_else(|| trouble!("the record for {name} could not be made"))
}

/// Find a task by id.
pub fn task<'a>(state: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    let Value::Array(tasks) = state.get_mut("tasks")? else {
        return None;
    };
    tasks
        .iter_mut()
        .find(|task| task.get("id").and_then(Value::as_str) == Some(id))
}

/// Put a line at the front of `recent_changes`, keeping the list short.
///
/// Newest first, because the question is always "what just happened".
pub fn note_change(state: &mut Value, line: &str) {
    const KEEP: usize = 12;
    let mut lines = vec![Value::string(line)];
    if let Some(existing) = state.get("recent_changes").and_then(Value::as_array) {
        lines.extend(existing.iter().take(KEEP - 1).cloned());
    }
    state.set("recent_changes", Value::Array(lines));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_state() -> Value {
        let text = r#"{
            "schema_version": 1,
            "updated_at": "2026-09-14T21:26:44+00:00",
            "agents": {
                "claude_code": {
                    "status": "working",
                    "current_task": "CLAUDE-1",
                    "active_paths": ["kernel/"],
                    "last_activity": "2026-09-14T21:26:44+00:00"
                }
            },
            "tasks": [
                {"id": "CLAUDE-1", "status": "working", "owner": "claude_code",
                 "summary": "something", "dependencies": []}
            ]
        }"#;
        nexus_json::parse(text).unwrap()
    }

    #[test]
    fn fields_this_program_does_not_know_about_survive_a_write() {
        // The one that would be a disaster. Astra's tools write `build`,
        // `qemu`, `integration`, `conflicts` and fields inside task records
        // that this program has never heard of. A write that dropped them
        // would be this program quietly deleting the other agent's work --
        // the exact thing the protocol forbids -- and it would look like a
        // successful command.
        //
        // It works because nexus_json keeps an object as the pairs it was
        // given, in order, and `set` replaces one key without rebuilding the
        // rest. That is a property of that crate, so it is pinned here.
        let text = r#"{
            "schema_version": 1,
            "updated_at": "2026-09-14T21:26:44+00:00",
            "agents": {
                "gpt6_astra": {
                    "status": "working",
                    "active_paths": [],
                    "last_activity": "2026-09-14T21:26:44+00:00",
                    "a_field_claude_has_never_heard_of": {"nested": [1, 2, 3]}
                }
            },
            "tasks": [
                {"id": "ASTRA-1", "status": "working", "owner": "gpt6_astra",
                 "qemu": "build/ai-3ad2/serial.log",
                 "files": ["shared/nexus-ai/src/lib.rs"]}
            ],
            "build": {"ai_user_target": "passed"},
            "integration": {"status": "partial", "note": "theirs"}
        }"#;
        let mut state = nexus_json::parse(text).unwrap();
        assert!(check(&state).is_empty(), "{:?}", check(&state));

        // Something this program does understand is changed...
        agent(&mut state, "claude_code")
            .unwrap()
            .set("status", Value::string("working"));
        note_change(&mut state, "claude_code: did a thing");
        state.set("updated_at", Value::string("2026-09-15T00:00:00+00:00"));

        // ...and everything it does not is still there, unchanged.
        let written = state.to_pretty();
        let back = nexus_json::parse(&written).unwrap();

        assert_eq!(
            back.at(["build", "ai_user_target"]).and_then(Value::as_str),
            Some("passed")
        );
        assert_eq!(
            back.at(["integration", "note"]).and_then(Value::as_str),
            Some("theirs")
        );
        assert_eq!(
            back.at([
                "agents",
                "gpt6_astra",
                "a_field_claude_has_never_heard_of",
                "nested"
            ])
            .and_then(Value::as_array)
            .map(<[Value]>::len),
            Some(3)
        );
        let task = &back.get("tasks").and_then(Value::as_array).unwrap()[0];
        assert_eq!(
            task.get("qemu").and_then(Value::as_str),
            Some("build/ai-3ad2/serial.log")
        );
        assert_eq!(
            task.get("files")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(1)
        );
        // And the change this program made really did happen.
        assert_eq!(
            back.at(["agents", "claude_code", "status"])
                .and_then(Value::as_str),
            Some("working")
        );
    }

    #[test]
    fn changing_one_field_does_not_reorder_the_rest() {
        // A reordered document is a diff that touches every line, which makes
        // "what did the other agent change" unanswerable by reading it.
        let text = r#"{"schema_version": 1, "agents": {"a": {"status": "idle"}},
                       "tasks": [], "zebra": 1, "apple": 2, "middle": 3}"#;
        let mut state = nexus_json::parse(text).unwrap();
        let before: Vec<String> = state
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        state.set("updated_at", Value::string("2026-09-15T00:00:00+00:00"));
        state.set("schema_version", Value::number(1));

        let after: Vec<String> = state
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .filter(|key| key != "updated_at")
            .collect();
        assert_eq!(before, after, "the keys moved");
    }

    #[test]
    fn a_good_state_has_nothing_wrong_with_it() {
        assert_eq!(check(&a_state()), Vec::<String>::new());
    }

    #[test]
    fn the_real_state_file_passes() {
        // The one in this repository, which both agents have been writing by
        // hand. If this program's idea of the shape disagrees with the file
        // that exists, this program is wrong.
        let store = Store::find(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let state = store.read_json(FILE).unwrap();
        assert_eq!(check(&state), Vec::<String>::new());
    }

    #[test]
    fn a_missing_schema_version_is_a_complaint() {
        let mut state = a_state();
        state.remove("schema_version");
        assert!(check(&state).iter().any(|it| it.contains("schema_version")));
    }

    #[test]
    fn a_future_schema_version_is_refused_rather_than_guessed_at() {
        let mut state = a_state();
        state.set("schema_version", Value::number(99));
        let complaints = check(&state);
        assert!(
            complaints.iter().any(|it| it.contains("99")),
            "{complaints:?}"
        );
    }

    #[test]
    fn two_tasks_with_one_id_are_found() {
        let mut state = a_state();
        let twin =
            nexus_json::parse(r#"{"id": "CLAUDE-1", "status": "done", "owner": "gpt6_astra"}"#)
                .unwrap();
        state.get_mut("tasks").unwrap().push(twin);
        let complaints = check(&state);
        assert!(
            complaints.iter().any(|it| it.contains("more than once")),
            "{complaints:?}"
        );
    }

    #[test]
    fn a_dependency_on_a_task_that_is_not_there_is_found() {
        let mut state = a_state();
        let task = super::task(&mut state, "CLAUDE-1").unwrap();
        task.set("dependencies", {
            let mut list = Value::array();
            list.push(Value::string("NOBODY-7"));
            list
        });
        let complaints = check(&state);
        assert!(
            complaints.iter().any(|it| it.contains("NOBODY-7")),
            "{complaints:?}"
        );
    }

    #[test]
    fn a_timestamp_that_is_not_one_is_found() {
        let mut state = a_state();
        state.set("updated_at", Value::string("the other day"));
        let complaints = check(&state);
        assert!(
            complaints.iter().any(|it| it.contains("the other day")),
            "{complaints:?}"
        );
    }

    #[test]
    fn an_agent_record_is_made_if_it_is_not_there() {
        let mut state = a_state();
        let record = agent(&mut state, "gpt6_astra").unwrap();
        assert_eq!(record.get("status").and_then(Value::as_str), Some("idle"));
        // And found rather than remade the second time.
        let record = agent(&mut state, "gpt6_astra").unwrap();
        record.set("status", Value::string("working"));
        let record = agent(&mut state, "gpt6_astra").unwrap();
        assert_eq!(
            record.get("status").and_then(Value::as_str),
            Some("working")
        );
    }

    #[test]
    fn a_change_goes_on_the_front_and_the_list_stays_short() {
        let mut state = a_state();
        for number in 0..20 {
            note_change(&mut state, &format!("change {number}"));
        }
        let changes = state
            .get("recent_changes")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(changes.len(), 12);
        assert_eq!(changes[0].as_str(), Some("change 19"));
        assert_eq!(changes[11].as_str(), Some("change 8"));
    }
}
