//! One run, or one session, as a file: export, and read-only import (card OA10-04, tasks CX-12 and UX-10 of the OpenCode audit).
//!
//! A **bundle** is one JSON file per run for a bug report, for a second look on another machine, and for offline replay of what a run was asked and what it was shown
//! (`scripts/eval/replay_run.py` counts the prompt a run would have had, with no model):
//!
//! ```json
//! { "schema": 1, "kind": "aqua.run", "exported_at": 0, "run": { "id", "goal", "status", ... }, "steps": [ ... ], "marks": { "tainted", "private" }, "model": "...", "tokens": { "in", "out" } }
//! ```
//!
//! * **from the record, as it is kept**: every string was masked when it was written (`agent_runs::clean_text`), and the bundle is built from those, so a secret that the record never
//!   held is not in the file. Nothing is read from the files the run touched.
//! * **bounded**: a bundle is at most [`MAX_BYTES`] (2 MB). A run that is longer loses the text of its oldest observations and planner replies first (their digests stay), and says so
//!   (`truncated`).
//! * **a session** is `{ "schema": 1, "kind": "aqua.session", "session": { id, title, directory }, "runs": [ bundle, ... ] }`. With `sanitize` the text of every tool result, of the
//!   arguments of every call and of every planner reply is replaced by `[redacted:<kind>:<number>]`, and an absolute path outside the project's folder in a goal or an answer by
//!   `[redacted:path:<n>]`: what is left is what was asked, which tools were called, and how it came out.
//! * **import is never a replay**: the run is put in the record as `imported` (it is not gone on from, not counted, never started, never resumed: nothing in a bundle is a call to make),
//!   in a session of its own, and every string of the file is masked again, because the file is not Aqua's own word. A run that is there already is refused; the file's depth and
//!   the length of its strings are limited; and what is shown of it is text a stranger may have written: data, never instructions. The route needs the UI token.

use super::store::{status, AgentRunStore, R};
use regex::Regex;
use rusqlite::params;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

pub const SCHEMA: u64 = 1;
/// The most a bundle may take up as text.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// The most steps a bundle may hold.
pub const MAX_STEPS: usize = 2_000;
/// How deep a value of a bundle may nest, and how long a string of it may be (a longer one is cut).
pub const MAX_DEPTH: usize = 12;
pub const MAX_STRING_CHARS: usize = 20_000;
/// The most runs a session export or import may hold.
pub const MAX_RUNS: usize = 50;

/// Why a bundle was not taken in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// Not a bundle of a schema this Aqua reads. Carries what was wrong.
    Bad(String),
    TooBig,
    /// A run with this id is in the record already.
    Exists(String),
    Store(String),
}

impl ImportError {
    pub fn message(&self) -> String {
        match self {
            ImportError::Bad(why) => format!("this is not a run bundle this Aqua can read: {why}"),
            ImportError::TooBig => format!("a bundle is at most {} MB", MAX_BYTES / (1024 * 1024)),
            ImportError::Exists(id) => format!("there is a run called {id} already: nothing was imported"),
            ImportError::Store(e) => e.clone(),
        }
    }
}

fn is_id(id: &str) -> bool {
    super::sessions::valid_session_id(id)
}

// ---- export -------------------------------------------------------------------------------------------------------------------------------------

/// The bundle of a run, from its record; `None` when there is no such run.
pub fn export_run(store: &AgentRunStore, run_id: &str, now: i64) -> Option<Value> {
    let mut run = store.get(run_id).ok().flatten()?;
    let steps = run.as_object_mut()?.remove("steps").and_then(|s| s.as_array().cloned()).unwrap_or_default();
    let marks = run.get("taint").cloned().filter(|t| !t.is_null()).unwrap_or_else(|| json!({ "tainted": null, "private": null }));
    let (model, tokens) = (run["model"].clone(), json!({ "in": run["tokens_in"], "out": run["tokens_out"] }));
    let mut bundle = json!({ "schema": SCHEMA, "kind": "aqua.run", "exported_at": now, "run": run, "steps": steps, "marks": marks, "model": model, "tokens": tokens });
    fit(&mut bundle);
    Some(bundle)
}

/// Makes a bundle fit [`MAX_BYTES`]: the text of the oldest observations and planner replies goes first (the digests stay), then the arguments.
fn fit(bundle: &mut Value) {
    if bundle.to_string().len() <= MAX_BYTES {
        return;
    }
    bundle["truncated"] = json!(true);
    let count = bundle["steps"].as_array().map_or(0, Vec::len);
    for field in ["observation", "planner_reply", "arguments"] {
        for i in 0..count {
            if bundle.to_string().len() <= MAX_BYTES {
                return;
            }
            if let Some(step) = bundle["steps"].get_mut(i) {
                if !step[field].is_null() {
                    step[field] = if field == "arguments" { Value::Null } else { json!("") };
                }
            }
        }
    }
    // Still too long: the oldest steps go.
    while bundle.to_string().len() > MAX_BYTES {
        match bundle["steps"].as_array_mut() {
            Some(steps) if steps.len() > 1 => {
                steps.remove(0);
            }
            _ => break,
        }
    }
}

/// The bundles of every run of a session, with its title and folder; `sanitize` takes the text of what the tools did out (see the module). `None` when there is no such session.
pub fn export_session(store: &AgentRunStore, session_id: &str, sanitize: bool, now: i64) -> Option<Value> {
    let session = store.get_session(session_id).ok().flatten()?;
    let directory = session["directory"].as_str().unwrap_or("").to_string();
    let mut counter = 0u32;
    let mut runs: Vec<Value> = Vec::new();
    for r in session["runs"].as_array().into_iter().flatten().take(MAX_RUNS) {
        let Some(mut bundle) = r["id"].as_str().and_then(|id| export_run(store, id, now)) else { continue };
        if sanitize {
            sanitize_bundle(&mut bundle, &directory, &mut counter);
        }
        runs.push(bundle);
    }
    // The title is the first sentence of the first goal unless the person gave one: a path in it is a path in the file.
    let title = match (sanitize, session["title"].as_str()) {
        (true, Some(t)) => json!(sanitize_text(t, &directory, &mut counter)),
        _ => session["title"].clone(),
    };
    Some(json!({
        "schema": SCHEMA,
        "kind": "aqua.session",
        "exported_at": now,
        "sanitized": sanitize,
        "session": { "id": session_id, "title": title, "directory": directory },
        "runs": runs,
    }))
}

fn path_regexes() -> &'static (Regex, Regex) {
    static RE: OnceLock<(Regex, Regex)> = OnceLock::new();
    RE.get_or_init(|| {
        (
            Regex::new(r#"(?i)[a-z]:[\\/][^\s"'<>|*?]*"#).expect("a valid pattern"),
            Regex::new(r#"(?:^|[\s("'=])(/(?:[\w.\-]+/)+[\w.\-]*)"#).expect("a valid pattern"),
        )
    })
}

/// A goal or an answer with every absolute path that is not inside `directory` replaced by `[redacted:path:<n>]` (the number counts up through the export).
pub fn sanitize_text(text: &str, directory: &str, counter: &mut u32) -> String {
    let inside = |p: &str| {
        let (p, d) = (p.replace('\\', "/").to_lowercase(), directory.replace('\\', "/").to_lowercase());
        !d.is_empty() && p.starts_with(d.trim_end_matches('/'))
    };
    let (windows, unix) = path_regexes();
    let step_one = windows.replace_all(text, |caps: &regex::Captures<'_>| {
        let whole = &caps[0];
        if inside(whole) {
            whole.to_string()
        } else {
            *counter += 1;
            format!("[redacted:path:{counter}]")
        }
    });
    unix.replace_all(&step_one, |caps: &regex::Captures<'_>| {
        let path = &caps[1];
        let lead = &caps[0][..caps[0].len() - path.len()];
        if inside(path) {
            caps[0].to_string()
        } else {
            *counter += 1;
            format!("{lead}[redacted:path:{counter}]")
        }
    })
    .into_owned()
}

fn sanitize_bundle(bundle: &mut Value, directory: &str, counter: &mut u32) {
    for key in ["goal", "final_answer"] {
        if let Some(text) = bundle["run"][key].as_str().map(str::to_string) {
            bundle["run"][key] = json!(sanitize_text(&text, directory, counter));
        }
    }
    for key in ["plan", "triage", "diagnosis", "record", "taint", "checklist", "steered", "plan_approval"] {
        // What the model wrote about the task is words of the run; its detail is not needed to read how the run went.
        if !bundle["run"][key].is_null() && key != "taint" {
            bundle["run"][key] = Value::Null;
        }
    }
    for step in bundle["steps"].as_array_mut().into_iter().flatten() {
        let seq = step["seq"].as_i64().unwrap_or(0);
        for (field, kind) in [("observation", "observation"), ("arguments", "arguments"), ("planner_reply", "reply")] {
            let has = match &step[field] {
                Value::Null => false,
                Value::String(s) => !s.is_empty(),
                _ => true,
            };
            if has {
                step[field] = json!(format!("[redacted:{kind}:{seq}]"));
            }
        }
    }
}

/// The session as text for a person to read: what was asked and answered in each run, which tools were called and how each came out. With `sanitize` that is all there is (no
/// arguments, no results); without, each call has the start of its arguments and its result.
pub fn transcript_md(export: &Value) -> String {
    let session = &export["session"];
    let mut out = format!("# {}\n\nSession `{}`{}\n", session["title"].as_str().unwrap_or("Session"), session["id"].as_str().unwrap_or(""), if export["sanitized"] == true { " (sanitized)" } else { "" });
    for bundle in export["runs"].as_array().into_iter().flatten() {
        let run = &bundle["run"];
        out.push_str(&format!("\n## Run `{}`: {} ({})\n\n", run["id"].as_str().unwrap_or(""), run["status"].as_str().unwrap_or(""), run["stopped_reason"].as_str().unwrap_or("")));
        out.push_str(&format!("**Asked:** {}\n\n**Answered:** {}\n", one(run["goal"].as_str().unwrap_or(""), 600), one(run["final_answer"].as_str().unwrap_or(""), 600)));
        let calls: Vec<&Value> = bundle["steps"].as_array().into_iter().flatten().filter(|s| s["kind"] == "tool").collect();
        if !calls.is_empty() {
            out.push_str("\nCalls:\n\n");
        }
        for (i, call) in calls.iter().enumerate() {
            let how = if call["ok"] == true { "worked" } else if call["ok"] == false { "failed" } else { "did not finish" };
            out.push_str(&format!("{}. `{}`: {how}", i + 1, call["tool"].as_str().unwrap_or("?")));
            if export["sanitized"] != true {
                out.push_str(&format!(" (arguments {}; result {})", one(&call["arguments"].to_string(), 160), one(call["observation"].as_str().unwrap_or(""), 240)));
            }
            out.push('\n');
        }
    }
    out
}

fn one(text: &str, max: usize) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        t
    } else {
        format!("{}…", t.chars().take(max).collect::<String>())
    }
}

// ---- import -------------------------------------------------------------------------------------------------------------------------------------

/// The value with every string masked and cut, and its depth checked.
fn clean(value: &Value, depth: usize) -> Result<Value, ImportError> {
    if depth > MAX_DEPTH {
        return Err(ImportError::Bad(format!("it nests deeper than {MAX_DEPTH} levels")));
    }
    Ok(match value {
        Value::String(s) => {
            let masked = aqua_tool_host::output::Redactor::new().redact(s);
            Value::String(if masked.chars().count() > MAX_STRING_CHARS { format!("{}…", masked.chars().take(MAX_STRING_CHARS).collect::<String>()) } else { masked })
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| clean(v, depth + 1)).collect::<Result<_, _>>()?),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.chars().take(80).collect(), clean(v, depth + 1)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

fn text(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or("").chars().take(max).collect()
}

/// Puts a run bundle in the record as an imported run, in a session of its own. The id of the run is the one in the file. Returns it.
pub fn import_run(store: &AgentRunStore, bundle: &Value, session: Option<&str>, now: i64) -> Result<String, ImportError> {
    if bundle.to_string().len() > MAX_BYTES {
        return Err(ImportError::TooBig);
    }
    if bundle["schema"].as_u64() != Some(SCHEMA) || bundle["kind"] != "aqua.run" {
        return Err(ImportError::Bad("its schema or kind is not one this Aqua reads (schema 1, kind aqua.run)".into()));
    }
    let bundle = clean(bundle, 0)?;
    let run = &bundle["run"];
    let id = run["id"].as_str().unwrap_or("").to_string();
    if !is_id(&id) {
        return Err(ImportError::Bad("the run has no usable id (1 to 64 letters, digits and _ . : -)".into()));
    }
    let steps = bundle["steps"].as_array().cloned().unwrap_or_default();
    if steps.len() > MAX_STEPS {
        return Err(ImportError::Bad(format!("it holds more than {MAX_STEPS} steps")));
    }
    let c = store.conn();
    let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM agent_runs WHERE id = ?1)", [&id], |r| r.get(0)).unwrap_or(false);
    if exists {
        return Err(ImportError::Exists(id));
    }
    let session_id = session.map(str::to_string).unwrap_or_else(|| format!("imported:{id}"));
    let write = |c: &rusqlite::Connection| -> Result<(), rusqlite::Error> {
        c.execute(
            "INSERT INTO agent_runs (id, started_at, ended_at, goal_redacted, goal_full, caller, route, status, model, rounds, tokens_in, tokens_out, stopped_reason, final_answer_head, parent_run_id, plan_json, taint_json, stopped_by, profile)
             VALUES (?1, ?2, ?3, ?4, '', ?5, 'imported', ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13, ?14, ?15, ?16)",
            params![
                id,
                run["started_at"].as_i64().unwrap_or(now),
                run["ended_at"].as_i64(),
                text(&run["goal"], 600),
                text(&run["caller"], 40),
                status::IMPORTED,
                text(&bundle["model"], 120),
                run["rounds"].as_i64().unwrap_or(0),
                run["tokens_in"].as_i64().unwrap_or(0),
                run["tokens_out"].as_i64().unwrap_or(0),
                text(&run["stopped_reason"], 200),
                text(&run["final_answer"], 600),
                (!run["plan"].is_null()).then(|| run["plan"].to_string().chars().take(4_000).collect::<String>()),
                (!bundle["marks"].is_null()).then(|| bundle["marks"].to_string()),
                text(&run["stopped_by"], 40),
                text(&run["profile"], 20),
            ],
        )?;
        for (i, s) in steps.iter().enumerate() {
            c.execute(
                "INSERT INTO agent_steps (run_id, step, seq, kind, tool, args_redacted, observation_digest, observation_head, ok, status, decision, approval, ms, tokens_in, tokens_out, cache_read, planner_reply, idempotency_digest, ts, ttft_ms, decode_tps_x10)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
                params![
                    id,
                    s["step"].as_i64().unwrap_or(0),
                    (i + 1) as i64,
                    text(&s["kind"], 20),
                    text(&s["tool"], 80),
                    if s["arguments"].is_string() { text(&s["arguments"], 2_000) } else { s["arguments"].to_string().chars().take(2_000).collect::<String>() },
                    text(&s["observation_digest"], 64),
                    text(&s["observation"], 12_000),
                    s["ok"].as_bool().map(|b| b as i64),
                    text(&s["status"], 20),
                    text(&s["decision"], 20),
                    text(&s["approval"], 20),
                    s["ms"].as_i64(),
                    s["tokens_in"].as_i64(),
                    s["tokens_out"].as_i64(),
                    s["cache_read"].as_i64(),
                    text(&s["planner_reply"], 4_000),
                    text(&s["idempotency_digest"], 32),
                    s["ts"].as_i64().unwrap_or(now),
                    s["ttft_ms"].as_i64(),
                    s["decode_tokens_per_second"].as_f64().map(|v| (v * 10.0) as i64),
                ],
            )?;
        }
        c.execute("INSERT INTO run_sessions (run_id, session_id, directory) VALUES (?1, ?2, '')", params![id, session_id])?;
        c.execute("INSERT INTO run_meta (run_id, origin) VALUES (?1, 'ui') ON CONFLICT(run_id) DO UPDATE SET origin = 'ui'", [&id])?;
        Ok(())
    };
    c.execute_batch("BEGIN IMMEDIATE").map_err(|e| ImportError::Store(format!("agent run store: {e}")))?;
    match write(&c) {
        Ok(()) => c.execute_batch("COMMIT").map_err(|e| ImportError::Store(format!("agent run store: {e}")))?,
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            return Err(ImportError::Store(format!("agent run store: {e}")));
        }
    }
    Ok(id)
}

/// Reads a file that is either a run bundle or a session export: the ids of the runs that were imported, in the order of the file. A session export puts all its runs in one imported session
/// (`imported:<its id>`). Nothing is imported if one run of it is refused.
pub fn import(store: &AgentRunStore, file: &Value, now: i64) -> Result<(Vec<String>, String), ImportError> {
    match file["kind"].as_str() {
        Some("aqua.run") => {
            let id = import_run(store, file, None, now)?;
            let session = store.session_id_of(&id);
            Ok((vec![id], session))
        }
        Some("aqua.session") if file["schema"].as_u64() == Some(SCHEMA) => {
            let runs = file["runs"].as_array().cloned().unwrap_or_default();
            if runs.is_empty() || runs.len() > MAX_RUNS {
                return Err(ImportError::Bad(format!("a session export holds 1 to {MAX_RUNS} runs")));
            }
            let session_id = {
                let given = file["session"]["id"].as_str().unwrap_or("");
                format!("imported:{}", if is_id(given) { given.to_string() } else { "session".to_string() })
            };
            // Every run is checked before one is written.
            for r in &runs {
                let id = r["run"]["id"].as_str().unwrap_or("");
                if !is_id(id) {
                    return Err(ImportError::Bad("a run of the file has no usable id".into()));
                }
                if store.get(id).ok().flatten().is_some() {
                    return Err(ImportError::Exists(id.to_string()));
                }
            }
            let mut ids = Vec::new();
            for r in &runs {
                match import_run(store, r, Some(&session_id), now) {
                    Ok(id) => ids.push(id),
                    Err(e) => {
                        for done in &ids {
                            let _ = store.delete_session_run(done);
                        }
                        return Err(e);
                    }
                }
            }
            if let Some(title) = file["session"]["title"].as_str().map(super::sessions::clean_title).filter(|t| !t.is_empty()) {
                let _ = store.rename_session(&session_id, &title, now);
            }
            Ok((ids, session_id))
        }
        _ => Err(ImportError::Bad("its kind is neither aqua.run nor aqua.session".into())),
    }
}

impl AgentRunStore {
    /// Takes one run out of the record with every row of it (what an import that failed half way undoes).
    pub fn delete_session_run(&self, run_id: &str) -> R<()> {
        super::store::delete_run_rows(&self.conn(), run_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::{NewRun, NewStep, RunEnd, StepOutcome};
    use super::*;

    fn store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_agent_bundle_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    /// A run of the coding kind: one read, one failing call, an answer.
    fn a_run(s: &AgentRunStore, id: &str, session: Option<&str>) {
        s.create_run(&NewRun { id: id.into(), started_at: 1_000, goal_redacted: format!("Fix the bug in C:\\Users\\asha\\secret\\lib.rs and D:\\proj\\a.rs ({id})"), goal_full: "x".into(), caller: "api".into(), route: "agent".into(), parent_run_id: None }).unwrap();
        if let Some(session) = session {
            s.set_run_session(id, session, "D:\\proj").unwrap();
        }
        let seq = s.add_step(id, &NewStep { step: 1, kind: "tool".into(), tool: "fs.read".into(), args_redacted: r#"{"path":"D:\\proj\\a.rs"}"#.into(), idempotency_digest: "d1".into(), ts: 1_001, ..Default::default() }).unwrap();
        s.finish_step(id, seq, &StepOutcome { ok: true, status: "completed".into(), observation_head: "fn main() { /* the file's text */ }".into(), observation_digest: "abcd".into(), decision: "allowed".into(), ms: 3, ..Default::default() }).unwrap();
        s.add_step(id, &NewStep { step: 2, kind: "plan".into(), planner_reply: "{\"tool\":\"fs.edit\"}".into(), ts: 1_002, ..Default::default() }).unwrap();
        s.end_run(id, 2_000, &RunEnd { status: "completed".into(), stopped_reason: "final".into(), rounds: 2, final_answer_head: "Fixed /home/asha/private/notes/todo.txt and D:\\proj\\a.rs".into(), model: "qwen".into(), tokens_in: 100, tokens_out: 20, taint_json: Some(r#"{"tainted":true,"private":false}"#.into()), ..Default::default() }).unwrap();
    }

    #[test]
    fn a_run_goes_out_as_a_bundle_and_comes_in_as_an_imported_run_that_is_never_gone_on_from() {
        let (a, b) = (store("round_trip_a"), store("round_trip_b"));
        a_run(&a, "run_1", Some("ses_x"));
        let bundle = export_run(&a, "run_1", 9).unwrap();
        assert_eq!((bundle["schema"].as_u64(), bundle["kind"].as_str(), bundle["model"].as_str(), bundle["marks"]["tainted"].as_bool()), (Some(1), Some("aqua.run"), Some("qwen"), Some(true)));
        assert_eq!(bundle["steps"].as_array().unwrap().len(), 2);
        assert!(bundle["run"].get("steps").is_none(), "the steps are beside the run, not inside it");
        let (ids, session) = import(&b, &bundle, 50).unwrap();
        assert_eq!((ids, session.as_str()), (vec!["run_1".to_string()], "imported:run_1"));
        let shown = b.get("run_1").unwrap().unwrap();
        assert_eq!((shown["status"].as_str(), shown["route"].as_str(), shown["model"].as_str(), shown["rounds"].as_i64(), shown["steps"].as_array().unwrap().len()), (Some("imported"), Some("imported"), Some("qwen"), Some(2), 2));
        assert_eq!(shown["steps"][0]["tool"], "fs.read");
        assert_eq!(shown["steps"][0]["observation"], "fn main() { /* the file's text */ }");
        assert_eq!(shown["taint"]["tainted"], true, "what the run held is kept");
        assert_eq!(shown["continuable"], false);
        assert_eq!(b.session_of("run_1").as_deref(), Some("imported:run_1"));
        // It is read only: it cannot be resumed, continued or answered (none of those accepts the state).
        let dead = |_: &str| false;
        assert!(matches!(super::super::resume::plan(&b, "run_1", &dead, &|_| false, None), Err(super::super::resume::Refused::Finished(s)) if s == "imported"));
        assert!(matches!(super::super::continuation::plan(&b, "run_1", &dead), Err(super::super::continuation::Refused::NotContinuable(..))));
        assert!(b.list_resumable(10).unwrap().is_empty());
        // And the same run again is refused, and nothing changes.
        assert_eq!(import(&b, &bundle, 51), Err(ImportError::Exists("run_1".into())));
    }

    #[test]
    fn what_comes_in_is_masked_again_whatever_the_file_says() {
        let (a, b) = (store("remask_a"), store("remask_b"));
        a_run(&a, "run_1", None);
        let mut bundle = export_run(&a, "run_1", 9).unwrap();
        // A file that somebody wrote by hand: secrets where the record would have none.
        bundle["run"]["goal"] = json!("deploy with password=hunter2secret99 now");
        bundle["run"]["final_answer"] = json!("the token=abcdef123456789012 was used");
        bundle["steps"][0]["observation"] = json!("api_key=sk-live-abcdefghijklmnop12345678 in the output");
        bundle["steps"][0]["arguments"] = json!({ "path": "a.rs", "secret": "password=hunter2secret99" });
        import(&b, &bundle, 50).unwrap();
        let shown = b.get("run_1").unwrap().unwrap().to_string();
        for secret in ["hunter2secret99", "abcdef123456789012", "sk-live-abcdefghijklmnop12345678"] {
            assert!(!shown.contains(secret), "{secret} survived the import: {shown}");
        }
    }

    #[test]
    fn a_file_that_is_too_deep_too_big_or_not_a_bundle_is_refused_and_a_long_string_is_cut() {
        let b = store("limits");
        let ok = json!({ "schema": 1, "kind": "aqua.run", "run": { "id": "run_z", "goal": "g", "status": "completed" }, "steps": [], "marks": null, "model": "m", "tokens": {} });
        assert!(matches!(import(&b, &json!({ "schema": 2, "kind": "aqua.run" }), 1), Err(ImportError::Bad(_))));
        assert!(matches!(import(&b, &json!({ "schema": 1, "kind": "something.else" }), 1), Err(ImportError::Bad(_))));
        let mut no_id = ok.clone();
        no_id["run"]["id"] = json!("../../etc/passwd");
        assert!(matches!(import(&b, &no_id, 1), Err(ImportError::Bad(_))));
        let mut deep = json!("x");
        for _ in 0..30 {
            deep = json!({ "a": deep });
        }
        let mut hostile = ok.clone();
        hostile["run"]["plan"] = deep;
        assert!(matches!(import(&b, &hostile, 1), Err(ImportError::Bad(why)) if why.contains("deeper")));
        let mut big = ok.clone();
        big["run"]["goal"] = json!("x".repeat(MAX_BYTES + 10));
        assert_eq!(import(&b, &big, 1), Err(ImportError::TooBig));
        let mut many = ok.clone();
        many["steps"] = json!(vec![json!({ "kind": "tool", "tool": "fs.read" }); MAX_STEPS + 1]);
        assert!(matches!(import(&b, &many, 1), Err(ImportError::Bad(_))));
        assert!(b.get("run_z").unwrap().is_none(), "none of the refused files left a run");
        let mut long = ok.clone();
        long["steps"] = json!([{ "kind": "tool", "tool": "fs.read", "observation": "y".repeat(MAX_STRING_CHARS * 2), "status": "completed" }]);
        import(&b, &long, 1).unwrap();
        assert!(b.get("run_z").unwrap().unwrap()["steps"][0]["observation"].as_str().unwrap().chars().count() <= 12_001);
    }

    #[test]
    fn a_bundle_that_is_longer_than_2_mb_loses_the_text_of_its_old_steps_and_says_so() {
        let s = store("fit");
        a_run(&s, "run_big", None);
        for i in 0..40 {
            let seq = s.add_step("run_big", &NewStep { step: 3 + i, kind: "tool".into(), tool: "fs.read".into(), ts: 2_000 + i as i64, ..Default::default() }).unwrap();
            s.finish_step("run_big", seq, &StepOutcome { ok: true, status: "completed".into(), observation_head: "z".repeat(12_000), ..Default::default() }).unwrap();
        }
        let mut bundle = export_run(&s, "run_big", 1).unwrap();
        assert!(bundle.to_string().len() < MAX_BYTES);
        // Make it long by hand (the record cuts each observation, so a real run is not this long) and fit it again.
        for step in bundle["steps"].as_array_mut().unwrap() {
            step["observation"] = json!("q".repeat(100_000));
        }
        assert!(bundle.to_string().len() > MAX_BYTES);
        fit(&mut bundle);
        assert!(bundle.to_string().len() <= MAX_BYTES && bundle["truncated"] == true);
        assert_eq!(bundle["steps"][0]["tool"], "fs.read", "the steps are still there: only the oldest text went");
    }

    #[test]
    fn a_session_is_exported_with_its_runs_and_sanitized_it_has_no_tool_text_and_no_path_outside_the_project() {
        let s = store("session");
        a_run(&s, "run_1", Some("ses_x"));
        a_run(&s, "run_2", Some("ses_x"));
        s.rename_session("ses_x", "Parser fix", 1).unwrap();
        let plain = export_session(&s, "ses_x", false, 5).unwrap();
        assert_eq!((plain["kind"].as_str(), plain["session"]["title"].as_str(), plain["runs"].as_array().unwrap().len(), plain["sanitized"].as_bool()), (Some("aqua.session"), Some("Parser fix"), 2, Some(false)));
        assert!(plain.to_string().contains("the file's text") && plain.to_string().contains("C:\\\\Users\\\\asha\\\\secret"));
        let clean = export_session(&s, "ses_x", true, 5).unwrap();
        let text = clean.to_string();
        assert!(!text.contains("the file's text") && !text.contains("fs.edit"), "tool results and planner replies are gone: {text}");
        assert!(!text.contains("asha") && !text.contains("private/notes"), "paths outside the project are gone: {text}");
        assert!(text.contains("D:\\\\proj\\\\a.rs"), "a path inside the project stays: {text}");
        assert!(text.contains("[redacted:observation:1]") && text.contains("[redacted:path:1]") && text.contains("[redacted:reply:2]"), "{text}");
        assert_eq!(clean["runs"][0]["steps"][0]["tool"], "fs.read", "which tool was called stays");
        // A session nobody gave a title is titled by its first goal, which holds a path: the title is sanitized too.
        a_run(&s, "run_3", Some("ses_y"));
        let titled = export_session(&s, "ses_y", true, 5).unwrap();
        assert!(!titled["session"]["title"].as_str().unwrap().contains("asha") && titled["session"]["title"].as_str().unwrap().contains("[redacted:path:"), "{}", titled["session"]["title"]);
        assert!(export_session(&s, "ses_y", false, 5).unwrap()["session"]["title"].as_str().unwrap().contains("asha"), "unsanitized, it is as it was");
        // The page for a person: the same, as text.
        let md = transcript_md(&clean);
        assert!(md.starts_with("# Parser fix\n") && md.contains("## Run `run_1`: completed (final)") && md.contains("1. `fs.read`: worked") && !md.contains("the file's text"), "{md}");
        let md_plain = transcript_md(&plain);
        assert!(md_plain.contains("the file's text"), "{md_plain}");
        assert!(export_session(&s, "nobody", true, 5).is_none() && export_run(&s, "nobody", 5).is_none());
        // And it comes back in as one imported session with its title; a half-bad file imports nothing.
        let t = store("session_in");
        let (ids, session) = import(&t, &plain, 9).unwrap();
        assert_eq!((ids.len(), session.as_str()), (2, "imported:ses_x"));
        assert_eq!(t.get_session("imported:ses_x").unwrap().unwrap()["title"], "Parser fix");
        let mut half = plain.clone();
        half["runs"][1]["run"]["id"] = json!("run_1");
        let u = store("session_half");
        assert!(matches!(import(&u, &half, 9), Err(ImportError::Exists(_))), "a run that appears twice is refused before any is written");
        assert_eq!(u.count().unwrap(), 0);
    }
}
