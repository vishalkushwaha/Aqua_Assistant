//! `aqua-cli support-bundle [--out DIR]`: a folder a person can look through and send with a bug report (card OA10-05; task UX-18 of the OpenCode audit).
//!
//! **An allow-list, not a copy.** Only the files named in [`FILES`] are written, each built here from something that cannot hold what the person said or a secret:
//!
//! | File | What it is |
//! |---|---|
//! | `README.txt` | what is in the folder and what is not |
//! | `versions.txt` | the version of Aqua and the system |
//! | `doctor.txt` | what `aqua-cli doctor` says (it never prints a secret), with the home folder shortened to `~` |
//! | `config_status.json` | which configuration files are valid, broken or missing: paths, states and positions, never what a file holds |
//! | `log_tail.txt` | the last 2,000 lines of the core's log, each run through the redactor of the tool host |
//! | `runs.jsonl` | the last 20 runs, **without content**: when, how it ended, and for each call the tool, whether it worked, how long it took and how a question was answered; no goal, no arguments, no result (`AQUA_TRACE_CONTENT` is ignored) |
//!
//! It never copies a token, the vault, the memory or any database file. Whatever it writes is also searched for the two tokens of this install (read from the data folder) and for the
//! home folder of the person, and a text that holds one is written with `[token]` or `~` in its place. It **prints the folder and never uploads anything.**

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The files of a bundle, and no others.
pub const FILES: [&str; 6] = ["README.txt", "versions.txt", "doctor.txt", "config_status.json", "log_tail.txt", "runs.jsonl"];
/// How many lines of the log, and how many runs.
pub const LOG_LINES: usize = 2000;
pub const RUNS: usize = 20;

/// Takes out what must not be in a text of a bundle: the tokens of this install (whole), whatever the redactor knows to be a secret, and the home folder.
pub struct Scrubber {
    secrets: Vec<String>,
    home: Option<String>,
    redactor: aqua_tool_host::output::Redactor,
}

impl Scrubber {
    /// `data_dir` holds the two token files; `home` is the folder of the person (their user name is in every path under it).
    pub fn new(data_dir: &Path, home: Option<String>) -> Self {
        let secrets = [".aqua_api_token", ".aqua_ui_token"].iter().filter_map(|f| std::fs::read_to_string(data_dir.join(f)).ok()).map(|t| t.trim().to_string()).filter(|t| t.len() >= 8).collect();
        Self { secrets, home: home.map(|h| h.trim_end_matches(['/', '\\']).to_string()).filter(|h| h.len() >= 4), redactor: aqua_tool_host::output::Redactor::new() }
    }

    pub fn text(&self, text: &str) -> String {
        // The tokens of this install first, whole, so that they are named for what they are; then the home folder (as it is written with either slash); then whatever the redactor knows.
        let mut out = text.to_string();
        for s in &self.secrets {
            out = out.replace(s.as_str(), "[token]");
        }
        if let Some(h) = &self.home {
            for form in [h.clone(), h.replace('\\', "/"), h.replace('/', "\\")] {
                out = out.replace(&form, "~");
            }
        }
        self.redactor.redact(&out)
    }
}

/// The last `RUNS` runs as one JSON object a line, without any of what they said or did: the run database is opened for reading only.
pub fn runs_without_content(db: &Path) -> String {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else {
        return String::new();
    };
    let Ok(mut runs) = conn.prepare("SELECT id, started_at, ended_at, status, stopped_reason, caller, route, rounds, tokens_in, tokens_out, length(goal_redacted) FROM agent_runs ORDER BY started_at DESC LIMIT ?1") else {
        return String::new();
    };
    let rows: Vec<(String, i64, Option<i64>, String, String, String, String, i64, i64, i64, i64)> = runs
        .query_map([RUNS as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)))
        .map(|m| m.flatten().collect())
        .unwrap_or_default();
    let mut out = String::new();
    for (id, started, ended, status, reason, caller, route, rounds, tin, tout, goal_len) in rows {
        let mut calls = Vec::new();
        if let Ok(mut q) = conn.prepare("SELECT kind, tool, ok, status, decision, approval, ms FROM agent_steps WHERE run_id = ?1 ORDER BY seq LIMIT 400") {
            for (kind, tool, ok, st, decision, approval, ms) in q
                .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<i64>>(6)?)))
                .map(|m| m.flatten().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                calls.push(json!({ "kind": kind, "tool": tool, "ok": ok.map(|o| o != 0), "status": st, "decision": decision, "approval": approval, "ms": ms }));
            }
        }
        // The goal is the person's words: only how long it was is kept.
        out.push_str(&json!({ "run_id": id, "started_at": started, "ended_at": ended, "status": status, "stopped_reason": reason, "caller": caller, "route": route, "rounds": rounds, "tokens_in": tin, "tokens_out": tout, "goal_chars": goal_len, "steps": calls }).to_string());
        out.push('\n');
    }
    out
}

/// What goes in `README.txt`.
fn readme() -> String {
    format!(
        "A support bundle of Aqua {} (made by `aqua-cli support-bundle`).\n\nIn this folder: {}.\n\nWhat is NOT in it, on purpose: your API and UI tokens, the vault, the memory, any database, the text of any request or answer, the arguments or results of any tool call. The log lines were run through Aqua's redactor and your home folder is written as ~.\nNothing was sent anywhere. Look through the files before you send them.\n",
        crate::version(),
        FILES.join(", ")
    )
}

/// Makes the bundle in `out` (a folder that does not exist yet) from the install whose data folder is `data_dir`; `log_file` is where the core's log is; `doctor` is what `aqua-cli doctor`
/// says (the program gives it: the doctor looks at the credential store and at the port of the person's own core, which a test of this file must not). Gives the files that were written.
pub fn make(data_dir: &Path, log_file: &Path, out: &Path, home: Option<String>, doctor: &str) -> Result<Vec<String>, String> {
    if out.exists() {
        return Err(format!("{} exists already: name a folder that does not (--out DIR)", out.display()));
    }
    std::fs::create_dir_all(out).map_err(|e| format!("the folder could not be made ({})", e.kind()))?;
    let scrub = Scrubber::new(data_dir, home);
    let log = crate::logging::run_command(&["--tail".to_string(), LOG_LINES.to_string()], log_file).unwrap_or_else(|e| format!("{e}\n"));
    let versions = format!("aqua {}\nos {} {}\n", crate::version(), std::env::consts::OS, std::env::consts::ARCH);
    let parts: Vec<(&str, String)> = vec![
        ("README.txt", readme()),
        ("versions.txt", versions),
        ("doctor.txt", scrub.text(doctor)),
        ("config_status.json", scrub.text(&serde_json::to_string_pretty(&crate::config_files::status_json(data_dir)).unwrap_or_default())),
        ("log_tail.txt", scrub.text(&log)),
        ("runs.jsonl", scrub.text(&runs_without_content(&data_dir.join("agent_runs.db")))),
    ];
    let mut written = Vec::new();
    for (name, text) in parts {
        debug_assert!(FILES.contains(&name));
        std::fs::write(out.join(name), text).map_err(|e| format!("{name} could not be written ({})", e.kind()))?;
        written.push(name.to_string());
    }
    Ok(written)
}

/// A name for a bundle made now, under the data folder: `support-bundle-<unix seconds>`.
pub fn default_folder(data_dir: &Path, now_secs: u64) -> PathBuf {
    data_dir.join("support").join(format!("support-bundle-{now_secs}"))
}

pub fn summary(out: &Path, written: &[String]) -> Value {
    json!({ "folder": out.display().to_string(), "files": written, "uploaded": false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runs::store::{status, NewRun, NewStep, RunEnd, StepOutcome};
    use crate::agent_runs::AgentRunStore;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_cli_tools").join(format!("bundle_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bundle_holds_only_its_allow_list_and_no_token_no_goal_and_no_result() {
        let dir = scratch("allow_list");
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("tools")).unwrap();
        let (api, ui) = ("api-token-AAAAAAAAAAAAAAAAAAAA", "ui-token-BBBBBBBBBBBBBBBBBBBBBB");
        std::fs::write(data.join(".aqua_api_token"), format!("{api}\n")).unwrap();
        std::fs::write(data.join(".aqua_ui_token"), format!("{ui}\n")).unwrap();
        // Things that must never be copied sit next to what is read.
        std::fs::write(data.join("memory.db"), b"MEMORY OF THE PERSON").unwrap();
        std::fs::create_dir_all(data.join("vault")).unwrap();
        std::fs::write(data.join("vault").join("vault.db"), b"VAULT").unwrap();
        // The log has a line with a token in it (a core that printed it by mistake) and a path under the home folder.
        let home = dir.join("home_of_the_person");
        let log = dir.join("core.log");
        std::fs::write(&log, format!("2026-10-10T10:00:00Z  INFO core: started\n2026-10-10T10:00:01Z  WARN core: odd line with {api} in it\n2026-10-10T10:00:02Z  INFO core: reading {}/notes.txt\n", home.display().to_string().replace('\\', "/"))).unwrap();
        // The run database: one run with words of the person in its goal and a result.
        let s = AgentRunStore::open(data.join("agent_runs.db")).unwrap();
        s.create_run(&NewRun { id: "r1".into(), started_at: 1_000, goal_redacted: "MY PRIVATE GOAL".into(), goal_full: "MY PRIVATE GOAL".into(), caller: "api".into(), route: "agent".into(), parent_run_id: None }).unwrap();
        let seq = s.add_step("r1", &NewStep { step: 1, kind: "tool".into(), tool: "fs.read".into(), args_redacted: "ARGS WITH PRIVATE PATH".into(), idempotency_digest: String::new(), planner_reply: "PLANNER SAID PRIVATE THINGS".into(), ts: 1_001 }).unwrap();
        s.finish_step("r1", seq, &StepOutcome { ok: true, status: "completed".into(), observation_head: "RESULT WITH PRIVATE TEXT".into(), approval: "approved".into(), ms: 12, ..Default::default() }).unwrap();
        s.end_run("r1", 2_000, &RunEnd { status: status::COMPLETED.into(), stopped_reason: "final".into(), final_answer_head: "ANSWER WITH PRIVATE TEXT".into(), ..Default::default() }).unwrap();
        drop(s);
        let out = dir.join("bundle");
        let written = make(&data, &log, &out, Some(home.display().to_string()), "doctor: all checks ran\n").unwrap();
        assert_eq!(written, FILES.iter().map(|f| f.to_string()).collect::<Vec<_>>());
        // Nothing but the allow-list is in the folder.
        let mut found: Vec<String> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        found.sort();
        let mut expected: Vec<String> = FILES.iter().map(|f| f.to_string()).collect();
        expected.sort();
        assert_eq!(found, expected);
        let all: String = FILES.iter().map(|f| std::fs::read_to_string(out.join(f)).unwrap()).collect::<Vec<_>>().join("\n");
        for must_not in [api, ui, "MEMORY OF THE PERSON", "VAULT", "MY PRIVATE GOAL", "ARGS WITH PRIVATE PATH", "PLANNER SAID", "RESULT WITH PRIVATE TEXT", "ANSWER WITH PRIVATE TEXT", "home_of_the_person"] {
            assert!(!all.contains(must_not), "the bundle holds {must_not:?}");
        }
        // What it is for is there: the run, its calls and the log, with the token and the home folder taken out.
        let runs = std::fs::read_to_string(out.join("runs.jsonl")).unwrap();
        let run: Value = serde_json::from_str(runs.lines().next().unwrap()).unwrap();
        assert_eq!((run["run_id"].as_str(), run["status"].as_str(), run["goal_chars"].as_i64()), (Some("r1"), Some("completed"), Some(15)));
        assert_eq!((run["steps"][0]["tool"].as_str(), run["steps"][0]["ok"].as_bool(), run["steps"][0]["approval"].as_str(), run["steps"][0]["ms"].as_i64()), (Some("fs.read"), Some(true), Some("approved"), Some(12)));
        let tail = std::fs::read_to_string(out.join("log_tail.txt")).unwrap();
        assert!(tail.contains("odd line with [token] in it") && tail.contains("reading ~/notes.txt"), "{tail}");
        assert!(std::fs::read_to_string(out.join("README.txt")).unwrap().contains("Nothing was sent anywhere"));
    }

    #[test]
    fn a_bundle_is_made_in_a_folder_that_is_new_and_a_missing_log_or_database_is_said_not_fatal() {
        let dir = scratch("new_folder");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = dir.join("bundle");
        let written = make(&data, &dir.join("no_log_here.log"), &out, None, "").unwrap();
        assert_eq!(written.len(), FILES.len());
        assert!(std::fs::read_to_string(out.join("log_tail.txt")).unwrap().contains("there is no log file"));
        assert_eq!(std::fs::read_to_string(out.join("runs.jsonl")).unwrap(), "");
        // Never into a folder that exists: nothing is overwritten and nothing is mixed in.
        assert!(make(&data, &dir.join("no_log_here.log"), &out, None, "").unwrap_err().contains("exists already"));
    }

    #[test]
    fn the_scrubber_takes_out_tokens_and_the_home_folder_in_either_slash() {
        let dir = scratch("scrubber");
        std::fs::write(dir.join(".aqua_api_token"), "abcdefgh12345678\n").unwrap();
        let s = Scrubber::new(&dir, Some("C:\\Users\\someone".into()));
        assert_eq!(s.text("t=abcdefgh12345678 in C:\\Users\\someone\\x and C:/Users/someone/y"), "t=[token] in ~\\x and ~/y");
        // A token that is too short to be one is not hunted for (it would cut ordinary words).
        std::fs::write(dir.join(".aqua_ui_token"), "ab\n").unwrap();
        assert_eq!(Scrubber::new(&dir, None).text("ab cd"), "ab cd");
    }
}
