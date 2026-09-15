//! # `nexus-collab`
//!
//! Reads and writes `.ai_collaboration`, the directory two AI developers —
//! Claude Code and GPT-6 Astra — use to tell each other what they are doing.
//!
//! Before this existed, both of us edited `STATE.json` by hand. That worked
//! until it did not: a state file written by hand is a state file that is
//! sometimes half-written, sometimes invalid, sometimes silently missing the
//! field the other party was about to read, and always at risk of one of us
//! replacing the other's edit without noticing.
//!
//! So: every write is atomic and backed up, every write is validated before it
//! lands, and the one operation that could destroy the other agent's work —
//! releasing their lock — is refused outright rather than offered with a
//! warning.
//!
//! ## Who you are
//!
//! From `--agent` or `$NEXUS_AGENT`. Reading needs no identity; writing does,
//! and there is no default, because a default would eventually write somebody
//! else's name into the shared state.
//!
//! ## What it will not do
//!
//! * Release, break or edit another agent's lock. Not with a flag, not with a
//!   confirmation. The protocol names this as forbidden and a `--force` that
//!   exists is a `--force` that gets used.
//! * Write a state document that does not validate.
//! * Replace `STATE.json` without first copying it into `backups/`.
//! * Treat a file it could not read as a file that says nothing.
//!
//! ## Commands
//!
//! ```text
//! nexus-collab status               everything at a glance
//! nexus-collab check                validate the directory; non-zero if broken
//! nexus-collab recover [name]       put a backup of STATE.json back
//!
//! nexus-collab state <status> [--task ID] [--note TEXT]
//! nexus-collab paths add|remove|set PATH...
//! nexus-collab heartbeat            say you are still here
//!
//! nexus-collab task list [--owner WHO] [--status WHAT]
//! nexus-collab task add ID --summary TEXT [--priority P] [--depends ID]...
//! nexus-collab task set ID [--status S] [--verification TEXT] [--summary TEXT]
//!
//! nexus-collab lock list
//! nexus-collab lock take TASK PATH...
//! nexus-collab lock release TASK
//! nexus-collab lock check PATH...   may I touch these?
//!
//! nexus-collab request list
//! nexus-collab request show ID
//! nexus-collab event add TEXT
//! nexus-collab decision add ID --summary TEXT
//! ```
//!
//! `--json` on any reading command prints the answer as JSON instead.

mod clock;
mod lock;
mod state;
mod store;

use std::process::ExitCode;

use nexus_json::Value;

use store::{trouble, Answer, Store};

/// What was asked for, once the arguments have been read.
struct Asked {
    words: Vec<String>,
    flags: Vec<(String, String)>,
    json: bool,
}

impl Asked {
    /// Split `--name value` pairs off from the plain words.
    ///
    /// A flag with no value takes the empty string rather than swallowing the
    /// next word, so `--note --task X` is a mistake that shows up as an empty
    /// note rather than as a note called "--task".
    fn read(arguments: impl Iterator<Item = String>) -> Self {
        let mut words = Vec::new();
        let mut flags = Vec::new();
        let mut json = false;

        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            let Some(name) = argument.strip_prefix("--") else {
                words.push(argument);
                continue;
            };
            if name == "json" {
                json = true;
                continue;
            }
            // `--name=value` as well as `--name value`, because both get typed.
            if let Some((name, value)) = name.split_once('=') {
                flags.push((name.to_string(), value.to_string()));
                continue;
            }
            let value = match arguments.peek() {
                Some(next) if !next.starts_with("--") => arguments.next().unwrap_or_default(),
                _ => String::new(),
            };
            flags.push((name.to_string(), value));
        }

        Self { words, flags, json }
    }

    fn word(&self, at: usize) -> Option<&str> {
        self.words.get(at).map(String::as_str)
    }

    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(had, _)| had == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every value given for a flag, for the ones that may repeat.
    fn every(&self, name: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(had, _)| had == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// The words from `at` onwards.
    fn rest(&self, at: usize) -> Vec<String> {
        self.words.iter().skip(at).cloned().collect()
    }

    /// Who is running this.
    fn agent(&self) -> Answer<String> {
        if let Some(named) = self.flag("agent") {
            if !named.is_empty() {
                return Ok(named.to_string());
            }
        }
        match std::env::var("NEXUS_AGENT") {
            Ok(named) if !named.is_empty() => Ok(named),
            _ => Err(trouble!(
                "this command writes, so it needs to know who you are.\n\
                 Set NEXUS_AGENT, or pass --agent. For example:\n\
                 \x20   NEXUS_AGENT=claude_code nexus-collab heartbeat"
            )),
        }
    }
}

fn main() -> ExitCode {
    let asked = Asked::read(std::env::args().skip(1));
    let Ok(here) = std::env::current_dir() else {
        eprintln!("error: this program cannot tell where it is running");
        return ExitCode::FAILURE;
    };

    match run(&here, &asked) {
        Ok(true) => ExitCode::SUCCESS,
        // A command that ran and found something wrong. Distinct from an
        // error, so that `check` can be used in a script.
        Ok(false) => ExitCode::from(2),
        Err(why) => {
            eprintln!("error: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Do what was asked. `false` means "ran, and the answer is no".
fn run(here: &std::path::Path, asked: &Asked) -> Answer<bool> {
    let command = asked.word(0).unwrap_or("status");
    if matches!(command, "help" | "-h" | "--help") {
        print!("{}", usage());
        return Ok(true);
    }

    let store = Store::find(here)?;
    match command {
        "status" => status(&store, asked),
        "check" => check(&store, asked),
        "recover" => recover(&store, asked),
        "state" => set_state(&store, asked),
        "paths" => set_paths(&store, asked),
        "heartbeat" => heartbeat(&store, asked),
        "task" => task(&store, asked),
        "lock" => locks(&store, asked),
        "request" => requests(&store, asked),
        "event" => event(&store, asked),
        "decision" => decision(&store, asked),
        other => Err(trouble!(
            "there is no `{other}` command. `nexus-collab help` lists them."
        )),
    }
}

fn usage() -> String {
    // The module comment is the documentation; this is the short form somebody
    // gets when they type the wrong thing.
    "\
nexus-collab -- the shared state two AI developers keep in .ai_collaboration

  status                        everything at a glance
  check                         validate the directory; exits 2 if broken
  recover [name]                put a backup of STATE.json back

  state <status> [--task ID] [--note TEXT]
  paths add|remove|set PATH...
  heartbeat                     say you are still here

  task list [--owner WHO] [--status WHAT]
  task add ID --summary TEXT [--priority P] [--depends ID]...
  task set ID [--status S] [--summary TEXT] [--verification TEXT]

  lock list
  lock take TASK PATH...
  lock release TASK
  lock check PATH...            may I touch these?

  request list
  request show ID
  event add TEXT
  decision add ID --summary TEXT

  --json                        print the answer as JSON
  --agent NAME                  who you are (or set NEXUS_AGENT)

It will not release another agent's lock, write a state that does not
validate, or replace STATE.json without backing it up first.
"
    .to_string()
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status(store: &Store, asked: &Asked) -> Answer<bool> {
    let state = store.read_json(state::FILE)?;
    let locks = lock::all(store)?;

    if asked.json {
        let mut out = Value::object();
        out.set(
            "agents",
            state.get("agents").cloned().unwrap_or(Value::Null),
        );
        out.set(
            "updated_at",
            state.get("updated_at").cloned().unwrap_or(Value::Null),
        );

        let mut listed = Value::array();
        for held in &locks {
            let mut one = Value::object();
            one.set("task", Value::string(held.task.as_str()));
            one.set("agent", Value::string(held.agent.as_str()));
            let mut paths = Value::array();
            for path in &held.paths {
                paths.push(Value::string(path.as_str()));
            }
            one.set("paths", paths);
            one.set("stale", Value::Bool(held.is_stale()));
            if let Some(quiet) = held.quiet_for() {
                one.set("quiet_seconds", Value::number(quiet));
            }
            listed.push(one);
        }
        out.set("locks", listed);
        out.set("open_tasks", open_tasks(&state));
        out.set(
            "pending_requests",
            state
                .get("pending_requests")
                .cloned()
                .unwrap_or(Value::array()),
        );
        out.set("complaints", complaints_of(store, &state)?);
        println!("{}", out.to_pretty());
        return Ok(true);
    }

    println!("{}", store.root().display());
    if let Some(when) = state.get("updated_at").and_then(Value::as_str) {
        let age = clock::parse(when)
            .map(|at| clock::describe(clock::unix_now() - at))
            .unwrap_or_else(|| "an unreadable time".to_string());
        println!("last changed {age} ago ({when})");
    }

    println!("\nagents");
    if let Some(agents) = state.get("agents").and_then(Value::as_object) {
        for (name, record) in agents {
            let status = record.get("status").and_then(Value::as_str).unwrap_or("?");
            let task = record
                .get("current_task")
                .and_then(Value::as_str)
                .unwrap_or("nothing");
            let quiet = record
                .get("last_activity")
                .and_then(Value::as_str)
                .and_then(clock::parse)
                .map(|at| format!(", quiet for {}", clock::describe(clock::unix_now() - at)))
                .unwrap_or_default();
            println!("  {name:<14} {status} on {task}{quiet}");
            if let Some(paths) = record.get("active_paths").and_then(Value::as_array) {
                if !paths.is_empty() {
                    let listed: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
                    println!("  {:<14} in {}", "", listed.join(", "));
                }
            }
        }
    }

    println!("\nlocks");
    if locks.is_empty() {
        println!("  none");
    }
    for held in &locks {
        let age = held
            .quiet_for()
            .map(clock::describe)
            .unwrap_or_else(|| "an unknown time".to_string());
        let stale = if held.is_stale() { "  STALE" } else { "" };
        println!("  {:<20} {} for {age}{stale}", held.task, held.agent);
        println!("  {:<20} {}", "", held.paths.join(", "));
    }

    let open = open_tasks(&state);
    println!("\nopen tasks");
    if open.as_array().is_some_and(<[Value]>::is_empty) {
        println!("  none");
    }
    if let Some(tasks) = open.as_array() {
        for task in tasks {
            println!(
                "  {:<34} {:<11} {}",
                task.get("id").and_then(Value::as_str).unwrap_or("?"),
                task.get("status").and_then(Value::as_str).unwrap_or("?"),
                task.get("owner").and_then(Value::as_str).unwrap_or("?"),
            );
        }
    }

    let complaints = complaints_of(store, &state)?;
    if let Some(list) = complaints.as_array() {
        if !list.is_empty() {
            println!("\nwrong");
            for complaint in list {
                println!("  {}", complaint.as_str().unwrap_or("?"));
            }
            return Ok(false);
        }
    }
    Ok(true)
}

/// Tasks that are not finished.
fn open_tasks(state: &Value) -> Value {
    let mut open = Value::array();
    let Some(tasks) = state.get("tasks").and_then(Value::as_array) else {
        return open;
    };
    for task in tasks {
        let status = task.get("status").and_then(Value::as_str).unwrap_or("");
        if !matches!(status, "completed" | "done" | "cancelled" | "abandoned") {
            open.push(task.clone());
        }
    }
    open
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Everything wrong with the directory, not only with `STATE.json`.
fn complaints_of(store: &Store, state: &Value) -> Answer<Value> {
    let mut complaints: Vec<String> = state::check(state);

    // A lock whose task is not in the state, and the other way round: a task
    // somebody is working on with no lock is fine, but a lock for a task
    // nobody has written down is somebody's tool having half-finished.
    let ids: Vec<&str> = state
        .get("tasks")
        .and_then(Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| task.get("id").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    for held in lock::all(store)? {
        if !ids.contains(&held.task.as_str()) {
            complaints.push(format!(
                "there is a lock for {}, which is not a task in {}",
                held.task,
                state::FILE
            ));
        }
        if held.is_stale() {
            let quiet = held
                .quiet_for()
                .map(clock::describe)
                .unwrap_or_else(|| "an unknown time".to_string());
            complaints.push(format!(
                "{}'s lock on {} has been untouched for {quiet}",
                held.agent, held.task
            ));
        }
    }

    // Two agents claiming the same ground. Not an error -- they may have
    // agreed -- but it is the thing that turns into a conflict, so it is said.
    if let Some(agents) = state.get("agents").and_then(Value::as_object) {
        for (one_name, one) in agents {
            for (other_name, other) in agents {
                if one_name >= other_name {
                    continue;
                }
                let (Some(mine), Some(theirs)) = (
                    one.get("active_paths").and_then(Value::as_array),
                    other.get("active_paths").and_then(Value::as_array),
                ) else {
                    continue;
                };
                for path in mine.iter().filter_map(Value::as_str) {
                    for against in theirs.iter().filter_map(Value::as_str) {
                        // Both paths named, not just the outer one. Two of
                        // Astra's paths inside one of mine produced the same
                        // sentence twice, which reads like a bug in the tool
                        // rather than like two overlaps.
                        let overlap = if lock::covers(path, against) {
                            format!("{other_name}'s {against} is inside {one_name}'s {path}")
                        } else if lock::covers(against, path) {
                            format!("{one_name}'s {path} is inside {other_name}'s {against}")
                        } else {
                            continue;
                        };
                        complaints.push(format!("both are working in one place: {overlap}"));
                    }
                }
            }
        }
    }

    let mut listed = Value::array();
    for complaint in complaints {
        listed.push(Value::string(complaint));
    }
    Ok(listed)
}

fn check(store: &Store, asked: &Asked) -> Answer<bool> {
    // Read raw rather than through `state::read`, which refuses a bad file --
    // and a bad file is exactly what this command is for looking at. Absent is
    // different again, and is a sentence rather than an operating-system
    // error about a path.
    let Some(state) = store.read_json_if_there(state::FILE)? else {
        println!("there is no {} in {}", state::FILE, store.root().display());
        return Ok(false);
    };
    let complaints = complaints_of(store, &state)?;
    let list = complaints.as_array().unwrap_or(&[]);

    if asked.json {
        println!("{}", complaints.to_pretty());
    } else if list.is_empty() {
        println!("nothing wrong");
    } else {
        for complaint in list {
            println!("{}", complaint.as_str().unwrap_or("?"));
        }
    }
    Ok(list.is_empty())
}

// ---------------------------------------------------------------------------
// recover
// ---------------------------------------------------------------------------

fn recover(store: &Store, asked: &Asked) -> Answer<bool> {
    let backups = store.backups()?;
    let Some(which) = asked.word(1) else {
        if backups.is_empty() {
            println!("there are no backups");
            return Ok(false);
        }
        println!("backups, newest first:");
        for name in &backups {
            let stamp = name
                .trim_start_matches("STATE-")
                .trim_end_matches(".json")
                .to_string();
            println!("  {name}   ({stamp})");
        }
        println!("\nput one back with: nexus-collab recover <name>");
        return Ok(true);
    };

    if !backups.iter().any(|name| name == which) {
        return Err(trouble!(
            "there is no backup called {which}. `nexus-collab recover` lists them."
        ));
    }

    // Validated before it goes back. A backup of a broken file is a broken
    // file, and restoring one without looking would turn a recoverable mess
    // into the same mess with the good copy overwritten.
    let saved = store.read_json(&format!("backups/{which}"))?;
    let complaints = state::check(&saved);
    if !complaints.is_empty() {
        return Err(trouble!(
            "{which} does not validate, so it is not going back:\n  {}",
            complaints.join("\n  ")
        ));
    }

    // And the current one is itself backed up first, so recovering can be
    // undone.
    let kept = store.back_up_state()?;
    store.write_json(state::FILE, &saved)?;
    println!("{} restored from {which}", state::FILE);
    if let Some(kept) = kept {
        println!("what was there is in backups/{kept}");
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// state, paths, heartbeat
// ---------------------------------------------------------------------------

fn set_state(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let Some(status) = asked.word(1) else {
        return Err(trouble!(
            "say what you are doing: nexus-collab state working --task CLAUDE-1"
        ));
    };

    let mut state = state::read(store)?;
    let now = clock::Moment::now().stamp();
    {
        let record = state::agent(&mut state, &me)?;
        record.set("status", Value::string(status));
        record.set("last_activity", Value::string(now));
        if let Some(task) = asked.flag("task") {
            record.set(
                "current_task",
                if task.is_empty() {
                    Value::Null
                } else {
                    Value::string(task)
                },
            );
        }
        if let Some(note) = asked.flag("note") {
            record.set("note", Value::string(note));
        }
    }
    state::write(store, &mut state)?;
    println!("{me} is {status}");
    Ok(true)
}

fn set_paths(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let how = asked.word(1).unwrap_or("");
    let given = asked.rest(2);
    if given.is_empty() {
        return Err(trouble!("which paths?"));
    }

    let mut state = state::read(store)?;
    let now = clock::Moment::now().stamp();
    {
        let record = state::agent(&mut state, &me)?;
        let mut paths: Vec<String> = record
            .get("active_paths")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        match how {
            "add" => {
                for path in &given {
                    if !paths.contains(path) {
                        paths.push(path.clone());
                    }
                }
            }
            "remove" => paths.retain(|path| !given.contains(path)),
            "set" => paths = given.clone(),
            other => {
                return Err(trouble!("`paths {other}`? It is add, remove or set."));
            }
        }

        let mut listed = Value::array();
        for path in &paths {
            listed.push(Value::string(path.as_str()));
        }
        record.set("active_paths", listed);
        record.set("last_activity", Value::string(now));
    }
    state::write(store, &mut state)?;

    // And say whether anybody else holds any of them. Not a refusal -- saying
    // where you are working is always allowed -- but the moment to find out.
    for path in &given {
        if how == "remove" {
            break;
        }
        if let Some(held) = lock::holder(store, path)? {
            if held.agent != me {
                println!(
                    "note: {path} is inside {}'s lock for {}",
                    held.agent, held.task
                );
            }
        }
    }
    println!("{me} is working in {}", given.join(", "));
    Ok(true)
}

fn heartbeat(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let mut state = state::read(store)?;
    let now = clock::Moment::now().stamp();
    state::agent(&mut state, &me)?.set("last_activity", Value::string(now));
    state::write(store, &mut state)?;

    let touched = lock::beat(store, &me)?;
    println!("{me} is still here; {touched} lock(s) refreshed");
    Ok(true)
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

fn task(store: &Store, asked: &Asked) -> Answer<bool> {
    match asked.word(1).unwrap_or("list") {
        "list" => list_tasks(store, asked),
        "add" => add_task(store, asked),
        "set" => set_task(store, asked),
        other => Err(trouble!("`task {other}`? It is list, add or set.")),
    }
}

fn list_tasks(store: &Store, asked: &Asked) -> Answer<bool> {
    let state = store.read_json(state::FILE)?;
    let empty: Vec<Value> = Vec::new();
    let tasks = state
        .get("tasks")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let wanted: Vec<&Value> = tasks
        .iter()
        .filter(|task| {
            asked
                .flag("owner")
                .is_none_or(|owner| task.get("owner").and_then(Value::as_str) == Some(owner))
                && asked
                    .flag("status")
                    .is_none_or(|status| task.get("status").and_then(Value::as_str) == Some(status))
        })
        .collect();

    if asked.json {
        let mut listed = Value::array();
        for task in wanted {
            listed.push(task.clone());
        }
        println!("{}", listed.to_pretty());
        return Ok(true);
    }

    for task in wanted {
        println!(
            "{:<34} {:<11} {:<12} {}",
            task.get("id").and_then(Value::as_str).unwrap_or("?"),
            task.get("status").and_then(Value::as_str).unwrap_or("?"),
            task.get("owner").and_then(Value::as_str).unwrap_or("?"),
            task.get("summary").and_then(Value::as_str).unwrap_or(""),
        );
    }
    Ok(true)
}

fn add_task(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let Some(id) = asked.word(2) else {
        return Err(trouble!("what is the task called?"));
    };
    let Some(summary) = asked.flag("summary") else {
        return Err(trouble!("a task with no --summary says nothing to anybody"));
    };

    let mut state = state::read(store)?;
    if state::task(&mut state, id).is_some() {
        return Err(trouble!(
            "{id} is already a task. `nexus-collab task set {id}` changes it."
        ));
    }

    let mut fresh = Value::object();
    fresh.set("id", Value::string(id));
    fresh.set(
        "status",
        Value::string(asked.flag("status").unwrap_or("working")),
    );
    fresh.set(
        "priority",
        Value::string(asked.flag("priority").unwrap_or("normal")),
    );
    fresh.set("owner", Value::string(asked.flag("owner").unwrap_or(&me)));
    fresh.set("summary", Value::string(summary));
    let mut depends = Value::array();
    for on in asked.every("depends") {
        depends.push(Value::string(on));
    }
    fresh.set("dependencies", depends);
    fresh.set("created_at", Value::string(clock::Moment::now().stamp()));

    state
        .get_mut("tasks")
        .ok_or_else(|| trouble!("{} has no tasks", state::FILE))?
        .push(fresh);
    state::note_change(&mut state, &format!("{me}: {id} started -- {summary}"));
    state::write(store, &mut state)?;
    println!("{id} added");
    Ok(true)
}

fn set_task(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let Some(id) = asked.word(2) else {
        return Err(trouble!("which task?"));
    };

    let mut state = state::read(store)?;
    let mut said = String::new();
    {
        let Some(task) = state::task(&mut state, id) else {
            return Err(trouble!("there is no task called {id}"));
        };
        for (flag, key) in [
            ("status", "status"),
            ("summary", "summary"),
            ("verification", "verification"),
            ("priority", "priority"),
            ("owner", "owner"),
            ("scope", "scope"),
        ] {
            if let Some(value) = asked.flag(flag) {
                task.set(key, Value::string(value));
                said = format!("{said} {key}={value}");
            }
        }
        if asked.flag("status") == Some("completed") {
            task.set("completed_at", Value::string(clock::Moment::now().stamp()));
        }
    }
    if said.is_empty() {
        return Err(trouble!("nothing to change: pass --status, --summary, ..."));
    }

    state::note_change(&mut state, &format!("{me}: {id}{said}"));
    state::write(store, &mut state)?;
    println!("{id}{said}");
    Ok(true)
}

// ---------------------------------------------------------------------------
// locks
// ---------------------------------------------------------------------------

fn locks(store: &Store, asked: &Asked) -> Answer<bool> {
    match asked.word(1).unwrap_or("list") {
        "list" => list_locks(store, asked),
        "take" => take_lock(store, asked),
        "release" => release_lock(store, asked),
        "check" => check_paths(store, asked),
        other => Err(trouble!(
            "`lock {other}`? It is list, take, release or check."
        )),
    }
}

fn list_locks(store: &Store, asked: &Asked) -> Answer<bool> {
    let locks = lock::all(store)?;
    if asked.json {
        let mut listed = Value::array();
        for held in &locks {
            let mut one = Value::object();
            one.set("task", Value::string(held.task.as_str()));
            one.set("agent", Value::string(held.agent.as_str()));
            let mut paths = Value::array();
            for path in &held.paths {
                paths.push(Value::string(path.as_str()));
            }
            one.set("paths", paths);
            one.set("stale", Value::Bool(held.is_stale()));
            listed.push(one);
        }
        println!("{}", listed.to_pretty());
        return Ok(true);
    }

    if locks.is_empty() {
        println!("nothing is locked");
    }
    for held in &locks {
        let age = held
            .quiet_for()
            .map(clock::describe)
            .unwrap_or_else(|| "an unknown time".to_string());
        println!(
            "{:<22} {:<12} {}{}",
            held.task,
            held.agent,
            held.paths.join(", "),
            if held.is_stale() { "   STALE" } else { "" }
        );
        println!("{:<22} untouched for {age}", "");
    }
    Ok(true)
}

fn take_lock(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let Some(task) = asked.word(2) else {
        return Err(trouble!(
            "a lock belongs to a task: nexus-collab lock take CLAUDE-1 kernel/"
        ));
    };
    let paths = asked.rest(3);
    let held = lock::take(store, &me, task, &paths)?;
    println!("{} holds {} for {task}", held.agent, held.paths.join(", "));
    Ok(true)
}

fn release_lock(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    let Some(task) = asked.word(2) else {
        return Err(trouble!("which lock?"));
    };
    let held = lock::release(store, &me, task)?;
    println!("{task} released ({})", held.paths.join(", "));
    Ok(true)
}

fn check_paths(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent().unwrap_or_default();
    let paths = asked.rest(2);
    if paths.is_empty() {
        return Err(trouble!("which paths?"));
    }

    let mut all_clear = true;
    let mut answers = Value::array();
    for path in &paths {
        let held = lock::holder(store, path)?;
        let mine = held.as_ref().is_none_or(|lock| lock.agent == me);
        if !mine {
            all_clear = false;
        }
        if asked.json {
            let mut one = Value::object();
            one.set("path", Value::string(path.as_str()));
            one.set("free", Value::Bool(mine));
            if let Some(held) = &held {
                one.set("agent", Value::string(held.agent.as_str()));
                one.set("task", Value::string(held.task.as_str()));
                one.set("stale", Value::Bool(held.is_stale()));
            }
            answers.push(one);
        } else {
            match held {
                None => println!("{path}: free"),
                Some(held) if held.agent == me => {
                    println!("{path}: yours, under {}", held.task)
                }
                Some(held) => println!(
                    "{path}: {}'s, under {}{}",
                    held.agent,
                    held.task,
                    if held.is_stale() { " (stale)" } else { "" }
                ),
            }
        }
    }
    if asked.json {
        println!("{}", answers.to_pretty());
    }
    Ok(all_clear)
}

// ---------------------------------------------------------------------------
// requests, events, decisions
// ---------------------------------------------------------------------------

fn requests(store: &Store, asked: &Asked) -> Answer<bool> {
    match asked.word(1).unwrap_or("list") {
        "list" => {
            let mut found = Value::array();
            for directory in ["claude_to_astra", "astra_to_claude"] {
                for name in store.list(directory)? {
                    if !name.starts_with("REQUEST") {
                        continue;
                    }
                    if asked.json {
                        let mut one = Value::object();
                        one.set("from", Value::string(directory));
                        one.set("file", Value::string(name.as_str()));
                        found.push(one);
                    } else {
                        println!("{directory}/{name}");
                    }
                }
            }
            if asked.json {
                println!("{}", found.to_pretty());
            }
            Ok(true)
        }
        "show" => {
            let Some(which) = asked.word(2) else {
                return Err(trouble!("which request?"));
            };
            for directory in ["claude_to_astra", "astra_to_claude"] {
                for name in store.list(directory)? {
                    if name.contains(which) {
                        let path = store.at(&format!("{directory}/{name}"));
                        let text = std::fs::read_to_string(&path).map_err(|why| {
                            trouble!("{} could not be read: {why}", path.display())
                        })?;
                        println!("{text}");
                        return Ok(true);
                    }
                }
            }
            Err(trouble!("nothing here is called {which}"))
        }
        other => Err(trouble!("`request {other}`? It is list or show.")),
    }
}

fn event(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    if asked.word(1) != Some("add") {
        return Err(trouble!("`event add <what happened>`"));
    }
    let what = asked.rest(2).join(" ");
    if what.is_empty() {
        return Err(trouble!("an event with nothing in it records nothing"));
    }

    let moment = clock::Moment::now();
    let mut value = Value::object();
    value.set("agent", Value::string(me.as_str()));
    value.set("at", Value::string(moment.stamp()));
    value.set("what", Value::string(what.as_str()));
    let name = format!("events/{}-{}.json", moment.filename_stamp(), me);
    store.write_json(&name, &value)?;

    let mut state = state::read(store)?;
    state::note_change(&mut state, &format!("{me}: {what}"));
    state::write(store, &mut state)?;
    println!("recorded in {name}");
    Ok(true)
}

fn decision(store: &Store, asked: &Asked) -> Answer<bool> {
    let me = asked.agent()?;
    if asked.word(1) != Some("add") {
        return Err(trouble!("`decision add <id> --summary <what was decided>`"));
    }
    let Some(id) = asked.word(2) else {
        return Err(trouble!("what is the decision called?"));
    };
    let Some(summary) = asked.flag("summary") else {
        return Err(trouble!("a decision with no --summary records nothing"));
    };

    let mut value = Value::object();
    value.set("id", Value::string(id));
    value.set("agent", Value::string(me.as_str()));
    value.set("at", Value::string(clock::Moment::now().stamp()));
    value.set("summary", Value::string(summary));
    if let Some(why) = asked.flag("why") {
        value.set("why", Value::string(why));
    }
    let name = format!("decisions/{id}.json");
    store.write_json(&name, &value)?;
    println!("recorded in {name}");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(line: &str) -> Asked {
        Asked::read(line.split_whitespace().map(str::to_string))
    }

    #[test]
    fn words_and_flags_are_told_apart() {
        let it = asked("task add CLAUDE-1 --summary something --priority high");
        assert_eq!(it.word(0), Some("task"));
        assert_eq!(it.word(1), Some("add"));
        assert_eq!(it.word(2), Some("CLAUDE-1"));
        assert_eq!(it.flag("summary"), Some("something"));
        assert_eq!(it.flag("priority"), Some("high"));
        assert_eq!(it.flag("nothing"), None);
    }

    #[test]
    fn an_equals_sign_works_as_well_as_a_space() {
        let it = asked("state working --task=CLAUDE-1");
        assert_eq!(it.flag("task"), Some("CLAUDE-1"));
    }

    #[test]
    fn a_flag_with_no_value_does_not_eat_the_next_flag() {
        // `--note --task X` is a typing mistake. Swallowing `--task` as the
        // note would write "--task" into the shared state and silently drop
        // the task, which is the worst of both.
        let it = asked("state working --note --task CLAUDE-1");
        assert_eq!(it.flag("note"), Some(""));
        assert_eq!(it.flag("task"), Some("CLAUDE-1"));
    }

    #[test]
    fn a_flag_may_repeat() {
        let it = asked("task add X --summary s --depends A --depends B");
        assert_eq!(it.every("depends"), vec!["A", "B"]);
    }

    #[test]
    fn json_is_a_flag_and_not_a_word() {
        let it = asked("status --json");
        assert!(it.json);
        assert_eq!(it.words.len(), 1);
    }

    #[test]
    fn writing_without_saying_who_you_are_is_refused() {
        // The alternative is a default, and a default eventually writes
        // somebody else's name into the shared state.
        let it = asked("heartbeat");
        // Only meaningful when the variable is not set in this environment.
        if std::env::var("NEXUS_AGENT").is_err() {
            let why = it.agent().unwrap_err().to_string();
            assert!(why.contains("NEXUS_AGENT"), "{why}");
        }
    }

    #[test]
    fn an_agent_flag_says_who_you_are() {
        let it = asked("heartbeat --agent claude_code");
        assert_eq!(it.agent().unwrap(), "claude_code");
    }

    #[test]
    fn an_unfinished_task_is_open_and_a_finished_one_is_not() {
        let state = nexus_json::parse(
            r#"{"tasks": [
                {"id": "A", "status": "working", "owner": "x"},
                {"id": "B", "status": "completed", "owner": "x"},
                {"id": "C", "status": "review", "owner": "x"}
            ]}"#,
        )
        .unwrap();
        let open = open_tasks(&state);
        let ids: Vec<&str> = open
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|task| task.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["A", "C"]);
    }
}
