//! Where agent runs live: `data/agent_runs.db`.
//!
//! A run is a row in `agent_runs`; every planner reply, tool call and answer is a row in `agent_steps`. A tool step is written as
//! `started` **before** the tool runs and completed after, so a crash leaves a step that says "this was in flight" instead of a
//! gap; on start-up the runs and steps that were still going are marked `interrupted` (never assumed to have worked or failed).
//!
//! Nothing here keeps secrets: arguments and text are redacted before they reach the store (see `agent_runs::record`).
//! The schema version is `PRAGMA user_version` (migration `0001` creates both tables). The file is opened and upgraded by `storage::sqlite_store`: every step runs in a
//! transaction together with its version bump, a newer database than this build is refused, a damaged file is moved aside (`agent_runs.db.corrupt-<unix>`) and runs are recorded
//! again in a fresh one, and a copy of the old file (`agent_runs.db.bak-v<n>`) is kept before a step changes its shape.

use super::events::{self, Event, Stored};
use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("agent run store: {e}")
}

/// Runs older than this are removed (30 days), and only this many are kept.
pub const RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;
pub const RETENTION_RUNS: i64 = 2_000;

/// The state a run is in. `Running` and `Stalled` are the two that are not final: a stalled run is one that is going on but has written nothing
/// for a while (see `AgentRunStore::mark_stalled`); its next step puts it back to running.
pub mod status {
    pub const RUNNING: &str = "running";
    pub const STALLED: &str = "stalled";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    pub const INTERRUPTED: &str = "interrupted";
    pub const WAITING_USER: &str = "waiting_user";
    /// The run asked the person something, and nobody answered in time (or Aqua was restarted: the question is kept in memory only).
    pub const EXPIRED: &str = "expired";
    /// A run that was read in from a bundle (`agent_runs::bundle`): shown read-only as data, never gone on from and never counted.
    pub const IMPORTED: &str = "imported";
}

#[derive(Debug, Clone, Default)]
pub struct NewRun {
    pub id: String,
    pub started_at: i64,
    pub goal_redacted: String,
    /// The goal with secrets masked and cut at [`GOAL_RESUME_CHARS`]: what a run that goes on after a stop is started with. Never sent out by the API.
    pub goal_full: String,
    pub caller: String,
    pub route: String,
    pub parent_run_id: Option<String>,
}

/// How much of a goal is kept for resuming it (a workflow's agent goal is at most 2,000).
pub const GOAL_RESUME_CHARS: usize = 4_000;

/// A call that was running when the run stopped, as the record kept it.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownStep {
    pub seq: i64,
    pub step: i64,
    pub tool: String,
    pub args_redacted: String,
    pub digest: String,
}

#[derive(Debug, Clone, Default)]
pub struct NewStep {
    pub step: u32,
    /// `plan`, `tool`, `observe`, `final` or `final_rejected`.
    pub kind: String,
    pub tool: String,
    pub args_redacted: String,
    pub idempotency_digest: String,
    pub planner_reply: String,
    pub ts: i64,
}

/// What a step turned out to be, written when it ends.
#[derive(Debug, Clone, Default)]
pub struct StepOutcome {
    pub ok: bool,
    /// `completed` or `failed`.
    pub status: String,
    pub observation_digest: String,
    pub observation_head: String,
    /// What the host (or the guard) decided: `allowed`, `denied` or `error`.
    pub decision: String,
    /// How a question was answered: `auto`, `approved`, `denied`, `timeout`, or empty when nothing was asked.
    pub approval: String,
    pub ms: i64,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cache_read: Option<i64>,
    /// Milliseconds before the first token (the server reading the prompt) and tokens written per second, times ten.
    pub ttft_ms: Option<i64>,
    pub decode_tps_x10: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct RunEnd {
    pub status: String,
    pub stopped_reason: String,
    pub rounds: u32,
    pub final_answer_head: String,
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub taint_json: Option<String>,
    /// The cap of the autonomy setting (`max_seconds`, `max_tool_calls`, `max_files_changed`, `max_changed_bytes`) the run reached, or empty (card R7-18).
    pub stopped_by: String,
}

pub struct AgentRunStore {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl AgentRunStore {
    pub fn open(path: impl AsRef<Path>) -> R<Self> {
        let path = path.as_ref().to_path_buf();
        let opened = sqlite_store::open_migrated(&path, &Opts::standard().synchronous_normal(true), STEPS).map_err(|e| format!("agent run store: {e}"))?;
        Ok(Self { path, conn: Mutex::new(opened.conn) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn create_run(&self, r: &NewRun) -> R<()> {
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO agent_runs (id, started_at, goal_redacted, goal_full, caller, route, status, parent_run_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
                params![r.id, r.started_at, r.goal_redacted, r.goal_full, r.caller, r.route, r.parent_run_id],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Writes a step as `started` and returns its sequence number (the order inside the run).
    pub fn add_step(&self, run_id: &str, s: &NewStep) -> R<i64> {
        let c = self.conn();
        let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM agent_steps WHERE run_id = ?1", [run_id], |r| r.get(0)).map_err(db)?;
        // The row that says a call is about to run is the one a restart reads to know that a write may have happened: it is on the disk before the call runs,
        // also when the power goes (the other writes of the record can be a moment late, which `NORMAL` allows).
        let _ = c.execute_batch("PRAGMA synchronous = FULL;");
        let written = c.execute(
            "INSERT INTO agent_steps (run_id, step, seq, kind, tool, args_redacted, status, planner_reply, idempotency_digest, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'started', ?7, ?8, ?9)",
            params![run_id, s.step, seq, s.kind, s.tool, s.args_redacted, s.planner_reply, s.idempotency_digest, s.ts],
        );
        let _ = c.execute_batch("PRAGMA synchronous = NORMAL;");
        written.map_err(db)?;
        // A run that was marked stalled and writes a step again was only slow.
        c.execute("UPDATE agent_runs SET status = 'running' WHERE id = ?1 AND status = 'stalled'", [run_id]).map_err(db)?;
        Ok(seq)
    }

    pub fn finish_step(&self, run_id: &str, seq: i64, o: &StepOutcome) -> R<()> {
        self.conn()
            .execute(
                "UPDATE agent_steps SET status = ?3, ok = ?4, observation_digest = ?5, observation_head = ?6, decision = ?7, approval = ?8, ms = ?9, tokens_in = ?10, tokens_out = ?11, cache_read = ?12,
                   ttft_ms = ?13, decode_tps_x10 = ?14
                 WHERE run_id = ?1 AND seq = ?2",
                params![run_id, seq, o.status, o.ok as i64, o.observation_digest, o.observation_head, o.decision, o.approval, o.ms, o.tokens_in, o.tokens_out, o.cache_read, o.ttft_ms, o.decode_tps_x10],
            )
            .map_err(db)?;
        Ok(())
    }

    /// What kind of task the run is (`coding`, `files`, `web` or `desktop`, see `modules::run_profile`). Written when the run starts, and again with `coding` when a run that began as
    /// something else changes code.
    pub fn set_profile(&self, run_id: &str, profile: &str) -> R<()> {
        self.conn().execute("UPDATE agent_runs SET profile = ?2 WHERE id = ?1", params![run_id, profile]).map_err(db)?;
        Ok(())
    }

    /// What kind of task the run was, as written ([`set_profile`](Self::set_profile)); `None` for a run that is unknown or has none (one from before the column).
    pub fn profile_of(&self, run_id: &str) -> Option<String> {
        let found: Option<String> = self.conn().query_row("SELECT profile FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)).optional().ok().flatten();
        found.filter(|p| !p.is_empty())
    }

    pub fn set_model(&self, run_id: &str, model: &str) -> R<()> {
        self.conn().execute("UPDATE agent_runs SET model = ?2 WHERE id = ?1 AND model = ''", params![run_id, model]).map_err(db)?;
        Ok(())
    }

    /// Records the plan the run is working to (a flow-style plan, when there is one).
    pub fn set_plan(&self, run_id: &str, plan_json: &str) -> R<()> {
        self.conn().execute("UPDATE agent_runs SET plan_json = ?2 WHERE id = ?1", params![run_id, plan_json]).map_err(db)?;
        Ok(())
    }

    pub fn end_run(&self, run_id: &str, ended_at: i64, e: &RunEnd) -> R<()> {
        // What the run held at its end goes through `set_marks`, which keeps the time each mark was first taken on (the safety page says when text from outside arrived).
        if let Some(v) = e.taint_json.as_deref().and_then(|t| serde_json::from_str::<Value>(t).ok()) {
            self.set_marks(run_id, v["tainted"].as_bool().unwrap_or(true), v["private"].as_bool().unwrap_or(true))?;
        }
        self.conn()
            .execute(
                "UPDATE agent_runs SET ended_at = ?2, status = ?3, stopped_reason = ?4, rounds = ?5, final_answer_head = ?6,
                   model = CASE WHEN ?7 != '' THEN ?7 ELSE model END, tokens_in = ?8, tokens_out = ?9, stopped_by = ?10
                 WHERE id = ?1",
                params![run_id, ended_at, e.status, e.stopped_reason, e.rounds, e.final_answer_head, e.model, e.tokens_in, e.tokens_out, e.stopped_by],
            )
            .map_err(db)?;
        // Anything still open when the run ended did not finish.
        self.conn().execute("UPDATE agent_steps SET status = 'interrupted' WHERE run_id = ?1 AND status = 'started'", [run_id]).map_err(db)?;
        Ok(())
    }

    /// Start-up: runs and steps an earlier process left going did not end. Returns how many runs were marked. The log of each ends with a `run.finished` that says so, so a client
    /// that reads it from the start, or takes it up again, is told the run is over and does not wait for more.
    pub fn mark_interrupted(&self, now_ms: i64) -> R<usize> {
        let (n, swept) = {
            let c = self.conn();
            let swept: Vec<(String, i64)> = {
                let mut st = c.prepare("SELECT id, started_at FROM agent_runs WHERE status IN ('running', 'stalled')").map_err(db)?;
                let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(db)?.filter_map(Result::ok).collect();
                rows
            };
            c.execute("UPDATE agent_steps SET status = 'interrupted' WHERE status = 'started'", []).map_err(db)?;
            let n = c
                .execute("UPDATE agent_runs SET status = 'interrupted', ended_at = ?1, stopped_reason = CASE WHEN stopped_reason = '' THEN 'process_ended' ELSE stopped_reason END WHERE status IN ('running', 'stalled')", [now_ms])
                .map_err(db)?;
            (n, swept)
        };
        for (id, started) in swept {
            self.note_ended_without_a_word(&id, "process_ended", now_ms.saturating_sub(started));
        }
        Ok(n)
    }

    /// The run ended and nothing in this process said so (a start-up sweep, the watchdog): its log gets the `run.finished` that tells a reader to stop waiting.
    fn note_ended_without_a_word(&self, run_id: &str, reason: &str, ms: i64) {
        let finished = Event::RunFinished { status: status::INTERRUPTED.into(), stopped_reason: reason.into(), answer_head: String::new(), why_it_stopped: None, files_changed: 0, ms: ms.max(0) };
        if let Err(e) = self.append_event(run_id, &finished) {
            tracing::warn!("{e}");
        }
    }

    /// Runs that asked the person something and were not answered for `idle_ms` are `expired` (the question lives in memory for a while and is gone after a restart,
    /// so with `0` at start-up every one of an earlier process expires). Returns how many.
    pub fn expire_waiting(&self, now_ms: i64, idle_ms: i64) -> R<usize> {
        self.conn()
            .execute(
                "UPDATE agent_runs SET status = 'expired', stopped_reason = 'question_expired' WHERE status = 'waiting_user' AND COALESCE(ended_at, started_at) <= ?1",
                [now_ms - idle_ms],
            )
            .map_err(db)
    }

    /// The newest run that goes on from this one (its `parent_run_id` is `run_id`), if any.
    pub fn child_of(&self, run_id: &str) -> R<Option<String>> {
        self.conn().query_row("SELECT id FROM agent_runs WHERE parent_run_id = ?1 ORDER BY started_at DESC, id DESC LIMIT 1", [run_id], |r| r.get(0)).optional().map_err(db)
    }

    /// A run that stopped is ended on purpose (the person chose not to go on from it): it becomes `cancelled` with `reason`.
    pub fn end_stopped(&self, run_id: &str, now_ms: i64, reason: &str) -> R<()> {
        self.conn()
            .execute("UPDATE agent_runs SET status = 'cancelled', ended_at = ?2, stopped_reason = ?3 WHERE id = ?1 AND status IN ('interrupted', 'stalled', 'failed')", params![run_id, now_ms, reason])
            .map_err(db)?;
        Ok(())
    }

    // ---- the question a run ended with (card OA10-03, AG-03) --------------------------------------------------------------------

    /// Keeps the question a run ended with (`text` and `choices` are masked and cut by the caller). A run asks once: a second call for the same run replaces the first.
    pub fn set_question(&self, run_id: &str, text: &str, choices: &[String], asked: u32, caller: &str, at_ms: i64) -> R<()> {
        let choices = serde_json::to_string(choices).unwrap_or_else(|_| "[]".into());
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO run_questions (run_id, question, choices, asked, caller, asked_at, answered_at, answered_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, '')",
                params![run_id, text, choices, asked, caller, at_ms],
            )
            .map_err(db)?;
        Ok(())
    }

    /// The question a run ended with, answered or not; `None` for a run that asked nothing.
    pub fn question_of(&self, run_id: &str) -> R<Option<QuestionRecord>> {
        self.conn()
            .query_row("SELECT run_id, question, choices, asked, caller, asked_at, answered_at, answered_by FROM run_questions WHERE run_id = ?1", [run_id], |r| {
                let choices: String = r.get(2)?;
                Ok(QuestionRecord {
                    run_id: r.get(0)?,
                    question: r.get(1)?,
                    choices: serde_json::from_str(&choices).unwrap_or_default(),
                    asked: r.get::<_, i64>(3)?.clamp(0, i64::from(u32::MAX)) as u32,
                    caller: r.get(4)?,
                    asked_at: r.get(5)?,
                    answered_at: r.get(6)?,
                    answered_by: r.get(7)?,
                })
            })
            .optional()
            .map_err(db)
    }

    /// Takes the answer to a question, once: `true` for the request that took it, `false` when it was taken already (a second click, a retry) or there is no such question. One
    /// statement decides, so two requests at once cannot both win, in this process or in another that shares the file.
    pub fn mark_answered(&self, run_id: &str, at_ms: i64, by: &str) -> R<bool> {
        let n = self.conn().execute("UPDATE run_questions SET answered_at = ?2, answered_by = ?3 WHERE run_id = ?1 AND answered_at IS NULL", params![run_id, at_ms, by]).map_err(db)?;
        Ok(n == 1)
    }

    /// Gives the answer back: the run that was to go on from it could not be started (nothing took the model's turn), so the question waits as it did.
    pub fn unmark_answered(&self, run_id: &str) -> R<()> {
        self.conn().execute("UPDATE run_questions SET answered_at = NULL, answered_by = '' WHERE run_id = ?1", [run_id]).map_err(db)?;
        Ok(())
    }

    /// Ends the wait of a question nobody answered in time: the run becomes `expired` (`question_expired`), as `expire_waiting` makes it for every question at a start.
    pub fn expire_question(&self, run_id: &str) -> R<bool> {
        let n = self
            .conn()
            .execute("UPDATE agent_runs SET status = 'expired', stopped_reason = 'question_expired' WHERE id = ?1 AND status = 'waiting_user'", [run_id])
            .map_err(db)?;
        Ok(n == 1)
    }

    /// The host says which call a step is, just before it dispatches it: the tool as the host knows it and the digest of the arguments as sent. Both
    /// replace what the loop announced (the model's own spelling of the tool).
    pub fn set_step_call(&self, run_id: &str, seq: i64, tool: &str, digest: &str) -> R<()> {
        self.conn().execute("UPDATE agent_steps SET tool = ?3, idempotency_digest = ?4 WHERE run_id = ?1 AND seq = ?2", params![run_id, seq, tool, digest]).map_err(db)?;
        Ok(())
    }

    /// The calls made since `since_ms`: (tool, the start of its result), for what went wrong with which tool; and how the runs of that time ended.
    pub fn calls_since(&self, since_ms: i64) -> R<(Vec<(String, String)>, Vec<String>)> {
        let c = self.conn();
        let calls = {
            let mut st = c.prepare("SELECT tool, observation_head FROM agent_steps WHERE kind = 'tool' AND ts >= ?1 AND status IN ('completed', 'failed')").map_err(db)?;
            let rows = st.query_map([since_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(db)?.filter_map(Result::ok).collect();
            rows
        };
        let reasons = {
            let mut st = c.prepare("SELECT stopped_reason FROM agent_runs WHERE started_at >= ?1 AND status != 'running'").map_err(db)?;
            let rows = st.query_map([since_ms], |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok).collect();
            rows
        };
        Ok((calls, reasons))
    }

    /// What a run holds, written as soon as it changes so that a crash does not lose it: `(read untrusted text, holds private data)`.
    pub fn set_marks(&self, run_id: &str, tainted: bool, private: bool) -> R<()> {
        // When each was first taken on is kept: a mark that was already there keeps its time.
        let before = self.marks_detail(run_id)?.unwrap_or_default();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let tainted_at = tainted.then(|| before.tainted_at.filter(|_| before.tainted).unwrap_or(now));
        let private_at = private.then(|| before.private_at.filter(|_| before.private).unwrap_or(now));
        self.conn()
            .execute("UPDATE agent_runs SET taint_json = ?2 WHERE id = ?1", params![run_id, json!({"tainted": tainted, "private": private, "tainted_at": tainted_at, "private_at": private_at}).to_string()])
            .map_err(db)?;
        Ok(())
    }

    /// What a run held and when each was taken on (see `agent_runs::safety`).
    pub fn marks_detail(&self, run_id: &str) -> R<Option<super::safety::MarksDetail>> {
        let text: Option<Option<String>> = self.conn().query_row("SELECT taint_json FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)).optional().map_err(db)?;
        Ok(text.flatten().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|v| super::safety::MarksDetail {
            tainted: v["tainted"].as_bool().unwrap_or(true),
            private: v["private"].as_bool().unwrap_or(true),
            tainted_at: v["tainted_at"].as_i64(),
            private_at: v["private_at"].as_i64(),
        }))
    }

    /// What `set_marks` kept, or `None` when the run is unknown or nothing was written.
    pub fn marks(&self, run_id: &str) -> R<Option<(bool, bool)>> {
        let text: Option<Option<String>> = self.conn().query_row("SELECT taint_json FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)).optional().map_err(db)?;
        Ok(text.flatten().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|v| (v["tainted"].as_bool().unwrap_or(true), v["private"].as_bool().unwrap_or(true))))
    }

    /// Why a run that did not go well stopped, written when it ended (see `triage`): one JSON object, as text.
    pub fn set_triage(&self, run_id: &str, triage_json: &str) -> R<()> {
        self.conn().execute("UPDATE agent_runs SET triage_json = ?2 WHERE id = ?1", params![run_id, triage_json]).map_err(db)?;
        Ok(())
    }

    /// What was written by [`Self::set_triage`], or `None` when nothing was (a run that went well, or one from before it existed).
    pub fn triage_json(&self, run_id: &str) -> R<Option<String>> {
        let found: Option<Option<String>> = self.conn().query_row("SELECT triage_json FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)).optional().map_err(db)?;
        Ok(found.flatten())
    }

    /// What the language model guessed about a failure the rules could not name (see `triage::diagnose`): one JSON object, as text. It is a record of a
    /// suggestion; nothing reads it back to act on it.
    pub fn set_diagnosis(&self, run_id: &str, diagnosis_json: &str) -> R<()> {
        self.conn().execute("UPDATE agent_runs SET diagnosis_json = ?2 WHERE id = ?1", params![run_id, diagnosis_json]).map_err(db)?;
        Ok(())
    }

    /// The goal as it was kept for resuming (secrets masked, cut at [`GOAL_RESUME_CHARS`]); the short one when no long one was kept.
    pub fn goal_for_resume(&self, run_id: &str) -> R<Option<String>> {
        let found: Option<(String, String)> = self.conn().query_row("SELECT goal_full, goal_redacted FROM agent_runs WHERE id = ?1", [run_id], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db)?;
        Ok(found.map(|(full, short)| if full.is_empty() { short } else { full }))
    }

    /// The calls of a run that were running when it stopped (the run ended, or Aqua did, before they came back), oldest first.
    pub fn unknown_steps(&self, run_id: &str) -> R<Vec<UnknownStep>> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT seq, step, tool, args_redacted, idempotency_digest FROM agent_steps WHERE run_id = ?1 AND kind = 'tool' AND status IN ('started', 'interrupted') ORDER BY seq")
            .map_err(db)?;
        let rows = st
            .query_map([run_id], |r| Ok(UnknownStep { seq: r.get(0)?, step: r.get(1)?, tool: r.get(2)?, args_redacted: r.get(3)?, digest: r.get(4)? }))
            .map_err(db)?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    /// When the run last wrote anything (a step, or its start): milliseconds since 1970, or `None` for an unknown run.
    pub fn last_progress(&self, run_id: &str) -> R<Option<i64>> {
        self.conn()
            .query_row("SELECT MAX(started_at, COALESCE((SELECT MAX(ts) FROM agent_steps s WHERE s.run_id = agent_runs.id), 0)) FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0))
            .optional()
            .map_err(db)
    }

    /// The watchdog: runs that are going but wrote nothing for `idle_ms` are marked stalled, and their ids returned. `alive` says whether this
    /// process is still working on a run; one it is not working on is not stalled but interrupted (nothing will ever write to it again).
    pub fn mark_stalled(&self, now_ms: i64, idle_ms: i64, alive: &dyn Fn(&str) -> bool) -> R<Vec<String>> {
        let ids: Vec<String> = {
            let c = self.conn();
            let mut st = c.prepare("SELECT id FROM agent_runs WHERE status = 'running'").map_err(db)?;
            let found = st.query_map([], |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok).collect();
            found
        };
        let mut stalled = Vec::new();
        for id in ids {
            let Some(last) = self.last_progress(&id)? else { continue };
            if now_ms - last < idle_ms {
                continue;
            }
            if alive(&id) {
                self.conn().execute("UPDATE agent_runs SET status = 'stalled' WHERE id = ?1 AND status = 'running'", [&id]).map_err(db)?;
                stalled.push(id);
            } else {
                self.conn().execute("UPDATE agent_steps SET status = 'interrupted' WHERE run_id = ?1 AND status = 'started'", [&id]).map_err(db)?;
                let marked = self
                    .conn()
                    .execute("UPDATE agent_runs SET status = 'interrupted', ended_at = ?2, stopped_reason = CASE WHEN stopped_reason = '' THEN 'no_longer_running' ELSE stopped_reason END WHERE id = ?1 AND status = 'running'", params![id, now_ms])
                    .map_err(db)?;
                if marked > 0 {
                    let started = self.started_at(&id).unwrap_or(last);
                    self.note_ended_without_a_word(&id, "no_longer_running", now_ms.saturating_sub(started));
                }
            }
        }
        Ok(stalled)
    }

    /// Removes runs older than `max_age_ms` and all but the newest `max_runs`; a run still going is never removed.
    pub fn prune(&self, now_ms: i64, max_age_ms: i64, max_runs: i64) -> R<usize> {
        let c = self.conn();
        let cutoff = now_ms - max_age_ms;
        let mut ids: Vec<String> = Vec::new();
        {
            let mut st = c.prepare("SELECT id FROM agent_runs WHERE status NOT IN ('running', 'stalled') AND started_at < ?1").map_err(db)?;
            ids.extend(st.query_map([cutoff], |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok));
            let mut st = c
                .prepare("SELECT id FROM agent_runs WHERE status NOT IN ('running', 'stalled') ORDER BY started_at DESC, id DESC LIMIT -1 OFFSET ?1")
                .map_err(db)?;
            ids.extend(st.query_map([max_runs.max(0)], |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok));
        }
        ids.sort();
        ids.dedup();
        for id in &ids {
            delete_run_rows(&c, id)?;
        }
        // A title or a fork mark of a session that has no run left goes too.
        super::sessions::forget_orphans(&c, now_ms, max_age_ms)?;
        Ok(ids.len())
    }

    /// Newest first. `status` narrows to one state.
    pub fn list(&self, limit: u32, offset: u32, status: Option<&str>) -> R<Vec<Value>> {
        let c = self.conn();
        let sql = "SELECT id, started_at, ended_at, goal_redacted, caller, route, status, model, rounds, stopped_reason, final_answer_head, parent_run_id,
                          (SELECT COUNT(*) FROM agent_steps s WHERE s.run_id = agent_runs.id AND s.kind = 'tool'), stopped_by, profile
                   FROM agent_runs WHERE (?1 IS NULL OR status = ?1) ORDER BY started_at DESC, id DESC LIMIT ?2 OFFSET ?3";
        let mut st = c.prepare(sql).map_err(db)?;
        let rows = st
            .query_map(params![status, limit.min(500), offset], |r| Ok(run_json(r, true)))
            .map_err(db)?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    /// The runs that stopped and can be gone on from (interrupted or stalled ones, and ones a panic ended) which nothing has gone on from yet, newest first. Nothing in
    /// them is started by this: the person is offered them.
    pub fn list_resumable(&self, limit: u32) -> R<Vec<Value>> {
        let c = self.conn();
        let sql = "SELECT id, started_at, ended_at, goal_redacted, caller, route, status, model, rounds, stopped_reason, final_answer_head, parent_run_id,
                          (SELECT COUNT(*) FROM agent_steps s WHERE s.run_id = agent_runs.id AND s.kind = 'tool'), stopped_by, profile
                   FROM agent_runs
                   WHERE (status IN ('interrupted', 'stalled') OR (status = 'failed' AND stopped_reason = 'aborted'))
                     AND NOT EXISTS (SELECT 1 FROM agent_runs child WHERE child.parent_run_id = agent_runs.id)
                   ORDER BY started_at DESC, id DESC LIMIT ?1";
        let mut st = c.prepare(sql).map_err(db)?;
        let rows = st.query_map(params![limit.min(500)], |r| Ok(run_json(r, true))).map_err(db)?.filter_map(Result::ok).collect();
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> R<Option<Value>> {
        let c = self.conn();
        let run = c
            .query_row(
                "SELECT id, started_at, ended_at, goal_redacted, caller, route, status, model, rounds, stopped_reason, final_answer_head, parent_run_id,
                        (SELECT COUNT(*) FROM agent_steps s WHERE s.run_id = agent_runs.id AND s.kind = 'tool'),
                        tokens_in, tokens_out, plan_json, taint_json, triage_json, diagnosis_json, stopped_by, profile
                 FROM agent_runs WHERE id = ?1",
                [id],
                |r| Ok(run_json(r, false)),
            )
            .optional()
            .map_err(db)?;
        let Some(mut run) = run else { return Ok(None) };
        run["steps"] = Value::Array(steps_of(&c, id)?);
        // The coding record of the run, as it was last written (migration 0009, card OA4-04); `null` for a run that has none.
        run["record"] = c.query_row("SELECT record FROM coding_records WHERE run_id = ?1", [id], |r| r.get::<_, String>(0)).optional().ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        // The session it is in, the checklist and the lines the person added to it, the plan it wrote and whether it can be gone on from (migration 0011, card OA10-04).
        super::sessions::decorate(&c, id, &mut run);
        // Why it stopped, in a sentence: what was written when it ended, or, for a run that has none, worked out now (card R7-06).
        crate::triage::add_to_run(&mut run);
        Ok(Some(run))
    }

    /// Keeps the coding record of a run (see migration 0009), replacing the one written before. `record` is the JSON of `coding_driver::CodingRecord`.
    pub fn set_coding_record(&self, run_id: &str, record: &str, now_ms: i64) -> R<()> {
        self.conn()
            .execute(
                "INSERT INTO coding_records (run_id, record, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(run_id) DO UPDATE SET record = excluded.record, updated_at = excluded.updated_at",
                params![run_id, record, now_ms],
            )
            .map_err(db)?;
        Ok(())
    }

    /// The coding record of a run as it was last written; `None` for a run that has none.
    pub fn coding_record_of(&self, run_id: &str) -> Option<Value> {
        let json: Option<String> = self.conn().query_row("SELECT record FROM coding_records WHERE run_id = ?1", [run_id], |r| r.get(0)).optional().ok().flatten();
        json.and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn steps(&self, id: &str) -> R<Vec<Value>> {
        steps_of(&self.conn(), id)
    }

    pub fn count(&self) -> R<i64> {
        self.conn().query_row("SELECT COUNT(*) FROM agent_runs", [], |r| r.get(0)).map_err(db)
    }

    fn started_at(&self, run_id: &str) -> Option<i64> {
        self.conn().query_row("SELECT started_at FROM agent_runs WHERE id = ?1", [run_id], |r| r.get(0)).ok()
    }

    // ---- the events of a run (card OA10-01) ----------------------------------------------------------------------------------

    /// Keeps the tools a command allowed the run (see migration 0010). Only a list that narrows is kept: the caller does not call this for a run no command narrowed.
    pub fn set_allowed_tools(&self, run_id: &str, tools: &[String]) -> R<()> {
        let text = serde_json::to_string(tools).unwrap_or_default();
        self.conn().execute("INSERT OR REPLACE INTO run_allowed_tools (run_id, tools) VALUES (?1, ?2)", params![run_id, text]).map_err(db)?;
        Ok(())
    }

    /// What [`Self::set_allowed_tools`] kept: empty for a run that was not narrowed by a command, one that is not recorded, and one from before the list was kept. When the run was narrowed by
    /// more than one list (card OA11-04: a command's and an agent's) this is the first of them; [`Self::allowed_lists_of`] has all.
    pub fn allowed_tools_of(&self, run_id: &str) -> R<Vec<String>> {
        Ok(self.allowed_lists_of(run_id)?.into_iter().next().unwrap_or_default())
    }

    /// Keeps every list that narrows the run (card OA11-04): the tools a command allowed and the tools the agent the run is allows. A run a call has to be in **every** list of, so none may be
    /// lost when it goes on after a restart. One list is kept exactly as [`Self::set_allowed_tools`] keeps it (the format the earlier versions read); more than one are kept as a list of lists.
    /// An empty list narrows nothing and is not kept.
    pub fn set_allowed_lists(&self, run_id: &str, lists: &[Vec<String>]) -> R<()> {
        let lists: Vec<&Vec<String>> = lists.iter().filter(|l| !l.is_empty()).collect();
        let text = match lists.as_slice() {
            [] => return Ok(()),
            [one] => serde_json::to_string(one),
            many => serde_json::to_string(many),
        }
        .unwrap_or_default();
        self.conn().execute("INSERT OR REPLACE INTO run_allowed_tools (run_id, tools) VALUES (?1, ?2)", params![run_id, text]).map_err(db)?;
        Ok(())
    }

    /// Every list [`Self::set_allowed_lists`] (or `set_allowed_tools`) kept for the run, in the order they were given: none for a run that was not narrowed, one that is not recorded and one from
    /// before the lists were kept. A row that cannot be read is **not** read as "no list" (that would let the run have every tool): it is one list that names no tool, so the run may call
    /// none but the host's own exceptions.
    pub fn allowed_lists_of(&self, run_id: &str) -> R<Vec<Vec<String>>> {
        let found: Option<String> = self.conn().query_row("SELECT tools FROM run_allowed_tools WHERE run_id = ?1", [run_id], |r| r.get(0)).optional().map_err(db)?;
        let Some(text) = found else { return Ok(Vec::new()) };
        if let Ok(one) = serde_json::from_str::<Vec<String>>(&text) {
            return Ok(if one.is_empty() { Vec::new() } else { vec![one] });
        }
        if let Ok(many) = serde_json::from_str::<Vec<Vec<String>>>(&text) {
            return Ok(many.into_iter().filter(|l| !l.is_empty()).collect());
        }
        Ok(vec![vec!["\u{0}no tool of this name".to_string()]])
    }

    /// Who asked for the run: `ui` (the app's token), `api` (the API token), or nothing. Only a run marked `api` is readable with the API token (`run_api`).
    pub fn set_origin(&self, run_id: &str, origin: &str) -> R<()> {
        self.conn().execute("INSERT INTO run_meta (run_id, origin) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET origin = excluded.origin", params![run_id, origin]).map_err(db)?;
        Ok(())
    }

    /// Writes an event to the log of `run_id` and wakes the streams that wait for one. The number inside the run is the next one (1, 2, 3 ...), given while the connection is held, so
    /// two writers on one run cannot take the same. `None` when nothing was written: the log is switched off (`AQUA_RUN_EVENTS=off`), the event is a frame of a stream and not an event
    /// of a run, or it is a second `run.finished` (a run ends once).
    pub fn append_event(&self, run_id: &str, event: &Event) -> R<Option<Stored>> {
        if !events::enabled() || !event.is_logged() {
            return Ok(None);
        }
        let prepared = event.prepare();
        let ts = super::now_ms();
        let row = {
            let c = self.conn();
            if prepared.kind == "run.finished" {
                let ended: Option<i64> = c.query_row("SELECT 1 FROM run_events WHERE run_id = ?1 AND type = 'run.finished' LIMIT 1", [run_id], |r| r.get(0)).optional().map_err(db)?;
                if ended.is_some() {
                    return Ok(None);
                }
            }
            let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1", [run_id], |r| r.get(0)).map_err(db)?;
            let ui_text = (!prepared.ui.is_empty()).then(|| Value::Object(prepared.ui.clone()).to_string());
            c.execute(
                "INSERT INTO run_events (run_id, seq, ts, type, data, ui) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![run_id, seq, ts, prepared.kind, Value::Object(prepared.public.clone()).to_string(), ui_text],
            )
            .map_err(db)?;
            Stored { id: c.last_insert_rowid(), run_id: run_id.to_string(), seq, ts, kind: prepared.kind.to_string(), data: prepared.public, ui: prepared.ui }
        };
        events::wake();
        Ok(Some(row))
    }

    /// The events of one run after number `after` (0: from the start), oldest first, at most `limit`.
    pub fn events_after(&self, run_id: &str, after: i64, limit: u32) -> R<Vec<Stored>> {
        let c = self.conn();
        let mut st = c
            .prepare("SELECT id, run_id, seq, ts, type, data, ui FROM run_events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3")
            .map_err(db)?;
        let rows = st.query_map(params![run_id, after.max(0), limit.clamp(1, 1_000)], stored_row).map_err(db)?.filter_map(Result::ok).collect();
        Ok(rows)
    }

    /// The events of every run after the global number `after_id`, oldest first, at most `limit`: the feed of the app.
    pub fn events_since(&self, after_id: i64, limit: u32) -> R<Vec<Stored>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, run_id, seq, ts, type, data, ui FROM run_events WHERE id > ?1 ORDER BY id LIMIT ?2").map_err(db)?;
        let rows = st.query_map(params![after_id.max(0), limit.clamp(1, 1_000)], stored_row).map_err(db)?.filter_map(Result::ok).collect();
        Ok(rows)
    }

    /// The newest global number ever given (rows deleted since do not lower it), 0 for a log that never had a row.
    pub fn last_event_id(&self) -> R<i64> {
        let found: Option<i64> = self.conn().query_row("SELECT seq FROM sqlite_sequence WHERE name = 'run_events'", [], |r| r.get(0)).optional().map_err(db)?;
        Ok(found.unwrap_or(0))
    }

    /// The number of the last event of a run, 0 when it has none.
    pub fn last_event_seq(&self, run_id: &str) -> R<i64> {
        self.conn().query_row("SELECT COALESCE(MAX(seq), 0) FROM run_events WHERE run_id = ?1", [run_id], |r| r.get(0)).map_err(db)
    }

    /// What a run is doing now, as its log last said (the `state` of its newest `status` event: `running`, `waiting_approval`, `waiting_user`, `queued`, `retry`, `idle`), or `None`
    /// for a run that has no such event (card OA10-03: the answer of `GET /api/v1/runs/active` and of the wait).
    pub fn last_state(&self, run_id: &str) -> R<Option<String>> {
        let found: Option<String> = self
            .conn()
            .query_row("SELECT data FROM run_events WHERE run_id = ?1 AND type = 'status' ORDER BY seq DESC LIMIT 1", [run_id], |r| r.get(0))
            .optional()
            .map_err(db)?;
        Ok(found.and_then(|data| serde_json::from_str::<Value>(&data).ok()).and_then(|v| v["state"].as_str().map(str::to_string)))
    }

    /// The runs that are not over (going, stalled, or waiting for an answer to their question), newest first, with who asked for each (`ui`, `api`, or empty): the list of
    /// `GET /api/v1/runs/active`.
    pub fn active_runs(&self, limit: u32) -> R<Vec<ActiveRun>> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT agent_runs.id, agent_runs.status, agent_runs.started_at, agent_runs.goal_redacted, agent_runs.caller, agent_runs.route, COALESCE(run_meta.origin, '')
                 FROM agent_runs LEFT JOIN run_meta ON run_meta.run_id = agent_runs.id
                 WHERE agent_runs.status IN ('running', 'stalled', 'waiting_user') ORDER BY agent_runs.started_at DESC, agent_runs.id DESC LIMIT ?1",
            )
            .map_err(db)?;
        let rows = st
            .query_map([limit.min(200)], |r| Ok(ActiveRun { id: r.get(0)?, status: r.get(1)?, started_at: r.get(2)?, goal: r.get(3)?, caller: r.get(4)?, route: r.get(5)?, origin: r.get(6)? }))
            .map_err(db)?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    /// What a reader of the log needs to know about a run before it reads: who asked for it, and whether its log is complete (see [`LogHead::finished`]). `None` for a run that is
    /// not recorded.
    pub fn log_head(&self, run_id: &str) -> R<Option<LogHead>> {
        let c = self.conn();
        let found: Option<(String, Option<String>)> = c
            .query_row("SELECT agent_runs.status, (SELECT origin FROM run_meta WHERE run_meta.run_id = agent_runs.id) FROM agent_runs WHERE agent_runs.id = ?1", [run_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(db)?;
        let Some((state, origin)) = found else { return Ok(None) };
        let has_end: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id = ?1 AND type = 'run.finished')", [run_id], |r| r.get(0)).map_err(db)?;
        // A run whose log has no `run.finished` is finished only when it never had a log (it ended before the log existed, or with the log switched off): waiting for an end that
        // will not come would hold a stream open for ever. A run that is still going, with or without events, is not finished.
        let any: bool = if has_end { true } else { c.query_row("SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id = ?1)", [run_id], |r| r.get(0)).map_err(db)? };
        let going = matches!(state.as_str(), status::RUNNING | status::STALLED);
        Ok(Some(LogHead { origin: origin.unwrap_or_default(), status: state, finished: has_end || (!any && !going) }))
    }
}

/// Every row of a run in every table, and the run itself: what pruning a run and deleting a session do.
pub(super) fn delete_run_rows(c: &Connection, id: &str) -> R<()> {
    c.execute("DELETE FROM agent_steps WHERE run_id = ?1", [id]).map_err(db)?;
    // The events of a run go with it (they are pruned together, as the spec of the log says).
    c.execute("DELETE FROM run_events WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM run_meta WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM run_questions WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM coding_records WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM run_allowed_tools WHERE run_id = ?1", [id]).map_err(db)?;
    // What belongs to the run in the sessions and the state tables (migration 0011) goes with it.
    super::sessions::forget_run_rows(c, id)?;
    c.execute("DELETE FROM agent_runs WHERE id = ?1", [id]).map_err(db)?;
    Ok(())
}

/// A run that is not over, as the list of active runs shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub id: String,
    /// `running`, `stalled` or `waiting_user`.
    pub status: String,
    pub started_at: i64,
    /// The goal, masked and cut (the short one the record keeps for listing).
    pub goal: String,
    pub caller: String,
    pub route: String,
    /// `ui`, `api`, or empty (nobody marked it: the voice orb, a workflow).
    pub origin: String,
}

/// What a reader of a run's log is told before it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogHead {
    /// `ui`, `api`, or empty.
    pub origin: String,
    /// The state in the run record.
    pub status: String,
    /// Nothing will be added to the log: it holds the run's `run.finished`, or the run ended and never had a log. Read this **before** the rows: if it is true, every row there will
    /// ever be is already written when the rows are read.
    pub finished: bool,
}

fn stored_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    let parse = |text: Option<String>| match text.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    Ok(Stored { id: r.get(0)?, run_id: r.get(1)?, seq: r.get(2)?, ts: r.get(3)?, kind: r.get(4)?, data: parse(r.get(5)?), ui: parse(r.get(6)?) })
}

/// The schema, one step per change; never edit a step that has shipped, add one at the end. `storage::sqlite_store` runs each in its own transaction together with the
/// version bump (it used to be autocommit: a crash between the two `ALTER`s of 0004 left a column added and the old version, and the next start failed on "duplicate column name").
pub(crate) const STEPS: &[Migration] = &[
    // 0001_agent_runs
    Migration {
        id: 1,
        sql: "CREATE TABLE IF NOT EXISTS agent_runs (
                 id TEXT PRIMARY KEY,
                 started_at INTEGER NOT NULL,
                 ended_at INTEGER,
                 goal_redacted TEXT NOT NULL DEFAULT '',
                 caller TEXT NOT NULL DEFAULT '',
                 route TEXT NOT NULL DEFAULT '',
                 status TEXT NOT NULL,
                 model TEXT NOT NULL DEFAULT '',
                 rounds INTEGER NOT NULL DEFAULT 0,
                 tokens_in INTEGER NOT NULL DEFAULT 0,
                 tokens_out INTEGER NOT NULL DEFAULT 0,
                 stopped_reason TEXT NOT NULL DEFAULT '',
                 final_answer_head TEXT NOT NULL DEFAULT '',
                 parent_run_id TEXT,
                 plan_json TEXT,
                 taint_json TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_agent_runs_started ON agent_runs(started_at);
             CREATE TABLE IF NOT EXISTS agent_steps (
                 run_id TEXT NOT NULL,
                 step INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 tool TEXT NOT NULL DEFAULT '',
                 args_redacted TEXT NOT NULL DEFAULT '',
                 observation_digest TEXT NOT NULL DEFAULT '',
                 observation_head TEXT NOT NULL DEFAULT '',
                 ok INTEGER,
                 status TEXT NOT NULL DEFAULT 'started',
                 decision TEXT NOT NULL DEFAULT '',
                 approval TEXT NOT NULL DEFAULT '',
                 ms INTEGER,
                 tokens_in INTEGER,
                 tokens_out INTEGER,
                 cache_read INTEGER,
                 planner_reply TEXT NOT NULL DEFAULT '',
                 idempotency_digest TEXT NOT NULL DEFAULT '',
                 ts INTEGER NOT NULL,
                 PRIMARY KEY (run_id, seq)
             );",
    },
    // 0002: what the model server measured for a planner reply.
    Migration {
        id: 2,
        sql: "ALTER TABLE agent_steps ADD COLUMN ttft_ms INTEGER;
             ALTER TABLE agent_steps ADD COLUMN decode_tps_x10 INTEGER;",
    },
    // 0003: the goal as it was said (secrets masked), so that a run that stopped can be started again from it.
    Migration {
        id: 3,
        sql: "ALTER TABLE agent_runs ADD COLUMN goal_full TEXT NOT NULL DEFAULT '';",
    },
    // 0004: why a run that did not go well stopped, in a sentence (card R7-06), and what the model guessed when the rules had nothing to say (card R7-07).
    // Both stay empty for a run that went well and for every run from before.
    Migration {
        id: 4,
        sql: "ALTER TABLE agent_runs ADD COLUMN triage_json TEXT;
             ALTER TABLE agent_runs ADD COLUMN diagnosis_json TEXT;",
    },
    // 0005: the cap of the autonomy setting that stopped the run (card R7-18). Empty for a run no cap stopped and for every run from before.
    Migration {
        id: 5,
        sql: "ALTER TABLE agent_runs ADD COLUMN stopped_by TEXT NOT NULL DEFAULT '';",
    },
    // 0006: the events of a run (card OA10-01). `id` counts across all runs and only grows (AUTOINCREMENT: a deleted row's number is not given again, so a client that remembers
    // one never meets it for another event); `seq` counts inside the run (1, 2, 3 ...). `data` is the part of the event every reader gets, `ui` the part only the UI token gets.
    // `run_meta.origin` is who asked for the run: `ui`, `api`, or empty for a run nobody marked (the voice orb, a workflow); only a run marked `api` is readable with the API token.
    // `IF NOT EXISTS` throughout: a database that was put back to an earlier version by hand upgrades again without an error.
    Migration {
        id: 6,
        sql: "CREATE TABLE IF NOT EXISTS run_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 ts INTEGER NOT NULL,
                 type TEXT NOT NULL,
                 data TEXT NOT NULL,
                 ui TEXT,
                 UNIQUE (run_id, seq)
             );
             CREATE TABLE IF NOT EXISTS run_meta (
                 run_id TEXT PRIMARY KEY,
                 origin TEXT NOT NULL DEFAULT ''
             );",
    },
    // 0007: what kind of task the run was (card OA4-01): `coding`, `files`, `web` or `desktop`. Empty for a run from before and for a run that was not given one: a run that goes on
    // from this one is the same kind, whatever the words of its goal say.
    Migration {
        id: 7,
        sql: "ALTER TABLE agent_runs ADD COLUMN profile TEXT NOT NULL DEFAULT '';",
    },
    // 0008: the question a run ended with (card OA10-03, AG-03), kept with the run so that `POST /api/v1/runs/{id}/answer` can go on from it without the person's conversation:
    // what was asked and the answers offered, how many questions the task has asked (a task may ask two), who the run was (`voice`, `api` or `agent`: the answer goes on with
    // the powers that run had), when it was asked, and, once, when and by whom it was answered. One row a run; a run that goes on after the answer is a new run with its own.
    // `IF NOT EXISTS`: a database put back to an earlier version by hand upgrades again without an error.
    Migration {
        id: 8,
        sql: "CREATE TABLE IF NOT EXISTS run_questions (
                 run_id TEXT PRIMARY KEY,
                 question TEXT NOT NULL,
                 choices TEXT NOT NULL DEFAULT '[]',
                 asked INTEGER NOT NULL DEFAULT 1,
                 caller TEXT NOT NULL DEFAULT '',
                 asked_at INTEGER NOT NULL,
                 answered_at INTEGER,
                 answered_by TEXT NOT NULL DEFAULT ''
             );",
    },
    // 0009: the coding record of a run (card OA4-04, WF-06): what the goal is, what was read and changed, the newest check of each kind, whether the code changed after the newest one, what
    // is in the way and what to do next, as the model was shown it, so that what a coding run came to is still there when it is read later. One row a run, written again as the run goes on.
    // A table of its own and `IF NOT EXISTS`, as 0006 and 0008: a database put back to an earlier version by hand upgrades again without an error.
    Migration {
        id: 9,
        sql: "CREATE TABLE IF NOT EXISTS coding_records (
                 run_id TEXT PRIMARY KEY,
                 record TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
    },
    // 0010: the tools a command allowed the run (`allowed-tools` of the command that started it, card OA11-02), as a JSON list, one row a run. A run that goes on after a question is narrowed
    // to them again, also when this process no longer remembers the run (it keeps the newest 64: without this the narrowing was lost with the run's place in that list). No row for a run no
    // command narrowed and for every run from before. `IF NOT EXISTS`: a database put back to an earlier version by hand upgrades again without an error.
    Migration {
        id: 10,
        sql: "CREATE TABLE IF NOT EXISTS run_allowed_tools (
                 run_id TEXT PRIMARY KEY,
                 tools TEXT NOT NULL DEFAULT '[]'
             );",
    },
    // 0011: sessions over runs, the state a run goes on from, and the record of a coding session (card OA10-04).
    // * `run_sessions`: which session a run is in (a run is in its own session unless it continues another run or is given a `session_id`), and the folder of the project it worked in.
    //   A run from before has no row: its session is its own id.
    // * `session_titles`: the title a person gave a session (without one the title is the first sentence of its first goal); `session_forks`: a session made from a run of another.
    // * `run_search`: the words of what a run was asked and what it answered (both already masked), for `GET /api/v1/sessions?q=`. Never a tool's arguments or result.
    // * `run_state`: the checklist and the lines the person added to a run (so that a run that goes on has them), and the plan a plan run wrote and what the person did with it.
    // * `coding_sessions`: the record of what coding runs in one project came to (file names, the last check, notes; never file contents), with the time it was last written.
    // Every statement is `IF NOT EXISTS`, as 0006, 0008 and 0009: a database put back to an earlier version by hand upgrades again without an error.
    Migration {
        id: 11,
        sql: "CREATE TABLE IF NOT EXISTS run_sessions (
                 run_id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 directory TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_run_sessions_session ON run_sessions(session_id);
             CREATE TABLE IF NOT EXISTS session_titles (
                 session_id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS session_forks (
                 session_id TEXT PRIMARY KEY,
                 from_session TEXT NOT NULL,
                 from_run TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS run_search USING fts5(run_id UNINDEXED, text, tokenize='unicode61');
             CREATE TABLE IF NOT EXISTS run_state (
                 run_id TEXT PRIMARY KEY,
                 checklist TEXT NOT NULL DEFAULT '',
                 steered TEXT NOT NULL DEFAULT '',
                 plan_state TEXT NOT NULL DEFAULT '',
                 plan_text TEXT NOT NULL DEFAULT '',
                 plan_at INTEGER,
                 approved_at INTEGER,
                 approved_by TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS coding_sessions (
                 id TEXT PRIMARY KEY,
                 project_key TEXT NOT NULL,
                 started_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 status TEXT NOT NULL DEFAULT 'open',
                 record TEXT NOT NULL DEFAULT '{}',
                 summarised INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_coding_sessions_project ON coding_sessions(project_key, updated_at);",
    },
];

/// The question a run ended with, as its record keeps it (see migration 0008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRecord {
    pub run_id: String,
    pub question: String,
    pub choices: Vec<String>,
    /// How many questions the task has asked, this one included (a task may ask two).
    pub asked: u32,
    /// How the run was started (`voice`, `api` or `agent`, as `browser_agent::windows_caller_name` names it): the answer goes on as that caller.
    pub caller: String,
    /// When it was asked (milliseconds since 1970).
    pub asked_at: i64,
    /// When it was answered, and by whom (`ui`); `None` while it waits.
    pub answered_at: Option<i64>,
    pub answered_by: String,
}

impl QuestionRecord {
    /// The record as the API shows it: what was asked, what was offered, and where it stands. Never the run's goal or what its calls touched.
    pub fn view(&self) -> Value {
        json!({
            "text": self.question,
            "choices": self.choices,
            "asked": self.asked,
            "asked_at": self.asked_at,
            "answered_at": self.answered_at,
            "answered": self.answered_at.is_some(),
        })
    }
}

pub(super) fn run_json(r: &rusqlite::Row<'_>, summary: bool) -> Value {
    let mut v = json!({
        "id": r.get::<_, String>(0).unwrap_or_default(),
        "started_at": r.get::<_, i64>(1).unwrap_or(0),
        "ended_at": r.get::<_, Option<i64>>(2).unwrap_or(None),
        "goal": r.get::<_, String>(3).unwrap_or_default(),
        "caller": r.get::<_, String>(4).unwrap_or_default(),
        "route": r.get::<_, String>(5).unwrap_or_default(),
        "status": r.get::<_, String>(6).unwrap_or_default(),
        "model": r.get::<_, String>(7).unwrap_or_default(),
        "rounds": r.get::<_, i64>(8).unwrap_or(0),
        "stopped_reason": r.get::<_, String>(9).unwrap_or_default(),
        "final_answer": r.get::<_, String>(10).unwrap_or_default(),
        "parent_run_id": r.get::<_, Option<String>>(11).unwrap_or(None),
        "tool_calls": r.get::<_, i64>(12).unwrap_or(0),
    });
    // The cap of the autonomy setting that stopped the run (migration 0005; empty when none did). Read by name: the lists and the single run select it at different places.
    v["stopped_by"] = json!(r.get::<_, String>("stopped_by").unwrap_or_default());
    // What kind of task it was (migration 0007, card OA4-01; empty for a run from before).
    v["profile"] = json!(r.get::<_, String>("profile").unwrap_or_default());
    if !summary {
        v["tokens_in"] = json!(r.get::<_, i64>(13).unwrap_or(0));
        v["tokens_out"] = json!(r.get::<_, i64>(14).unwrap_or(0));
        v["plan"] = r.get::<_, Option<String>>(15).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        v["taint"] = r.get::<_, Option<String>>(16).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        // The stored sentence and the model's guess (columns 17 and 18, from migration 0004); `triage::add_to_run` reads and completes them.
        v["triage"] = r.get::<_, Option<String>>(17).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        v["diagnosis"] = r.get::<_, Option<String>>(18).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
    }
    v
}

fn steps_of(c: &Connection, id: &str) -> R<Vec<Value>> {
    let mut st = c
        .prepare(
            "SELECT seq, step, kind, tool, args_redacted, observation_digest, observation_head, ok, status, decision, approval, ms,
                    tokens_in, tokens_out, cache_read, planner_reply, idempotency_digest, ts, ttft_ms, decode_tps_x10
             FROM agent_steps WHERE run_id = ?1 ORDER BY seq",
        )
        .map_err(db)?;
    let rows = st
        .query_map([id], |r| {
            let args: String = r.get(4)?;
            Ok(json!({
                "seq": r.get::<_, i64>(0)?,
                "step": r.get::<_, i64>(1)?,
                "kind": r.get::<_, String>(2)?,
                "tool": r.get::<_, String>(3)?,
                "arguments": serde_json::from_str::<Value>(&args).unwrap_or(Value::String(args)),
                "observation_digest": r.get::<_, String>(5)?,
                "observation": r.get::<_, String>(6)?,
                "ok": r.get::<_, Option<i64>>(7)?.map(|n| n != 0),
                "status": r.get::<_, String>(8)?,
                "decision": r.get::<_, String>(9)?,
                "approval": r.get::<_, String>(10)?,
                "ms": r.get::<_, Option<i64>>(11)?,
                "tokens_in": r.get::<_, Option<i64>>(12)?,
                "tokens_out": r.get::<_, Option<i64>>(13)?,
                "cache_read": r.get::<_, Option<i64>>(14)?,
                "planner_reply": r.get::<_, String>(15)?,
                "idempotency_digest": r.get::<_, String>(16)?,
                "ts": r.get::<_, i64>(17)?,
                "ttft_ms": r.get::<_, Option<i64>>(18)?,
                "decode_tokens_per_second": r.get::<_, Option<i64>>(19)?.map(|v| v as f64 / 10.0),
            }))
        })
        .map_err(db)?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    fn run(id: &str, at: i64) -> NewRun {
        NewRun { id: id.into(), started_at: at, goal_redacted: format!("goal {id}"), caller: "api".into(), ..Default::default() }
    }

    fn tool_step(step: u32, tool: &str) -> NewStep {
        NewStep { step, kind: "tool".into(), tool: tool.into(), args_redacted: "{\"path\":\"a\"}".into(), ts: 1, ..Default::default() }
    }

    #[test]
    fn ending_a_run_keeps_the_time_its_marks_were_first_taken_on() {
        let s = store("end_marks");
        s.create_run(&run("m1", 10)).unwrap();
        s.set_marks("m1", true, false).unwrap();
        let first = s.marks_detail("m1").unwrap().unwrap();
        assert!(first.tainted && first.tainted_at.is_some() && first.private_at.is_none());
        s.end_run("m1", 99, &RunEnd { status: "completed".into(), taint_json: Some(r#"{"tainted":true,"private":true}"#.into()), ..Default::default() }).unwrap();
        let after = s.marks_detail("m1").unwrap().unwrap();
        assert!(after.tainted && after.private, "{after:?}");
        assert_eq!(after.tainted_at, first.tainted_at, "the time text from outside first arrived is not lost when the run ends");
        assert!(after.private_at.is_some(), "and the private mark that came last has its time too");
        // A run that ends with nothing to say about its marks keeps what it had.
        s.create_run(&run("m2", 20)).unwrap();
        s.set_marks("m2", false, true).unwrap();
        s.end_run("m2", 99, &RunEnd { status: "completed".into(), ..Default::default() }).unwrap();
        assert_eq!(s.marks("m2").unwrap(), Some((false, true)));
    }

    #[test]
    fn a_step_is_written_before_the_tool_runs_and_completed_after() {
        let s = store("wal");
        s.create_run(&run("r1", 1_000)).unwrap();
        let seq = s.add_step("r1", &tool_step(1, "fs.read")).unwrap();
        // The tool is "running": what the database holds now is what a crash would leave behind.
        let open = s.steps("r1").unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!((open[0]["status"].as_str(), open[0]["ok"].clone()), (Some("started"), Value::Null));
        s.finish_step("r1", seq, &StepOutcome { ok: true, status: "completed".into(), observation_head: "hello".into(), decision: "allowed".into(), ms: 12, ..Default::default() }).unwrap();
        let done = s.steps("r1").unwrap();
        assert_eq!((done[0]["status"].as_str(), done[0]["ok"].clone(), done[0]["ms"].clone()), (Some("completed"), json!(true), json!(12)));
        assert_eq!(done[0]["arguments"], json!({"path": "a"}), "arguments come back as JSON");
    }

    #[test]
    fn a_process_that_died_mid_run_leaves_it_interrupted_never_completed_or_failed() {
        let s = store("kill");
        s.create_run(&run("r1", 1_000)).unwrap();
        s.add_step("r1", &tool_step(1, "term.run")).unwrap(); // `started`, never finished: the process was killed here
        s.create_run(&run("r2", 2_000)).unwrap();
        s.end_run("r2", 2_500, &RunEnd { status: status::COMPLETED.into(), stopped_reason: "final".into(), rounds: 1, ..Default::default() }).unwrap();
        // The next start-up:
        assert_eq!(s.mark_interrupted(9_000).unwrap(), 1);
        let r1 = s.get("r1").unwrap().unwrap();
        assert_eq!((r1["status"].as_str(), r1["stopped_reason"].as_str(), r1["ended_at"].as_i64()), (Some("interrupted"), Some("process_ended"), Some(9_000)));
        assert_eq!(r1["steps"][0]["status"], "interrupted", "the step that was in flight is not assumed to have worked");
        assert_eq!(s.get("r2").unwrap().unwrap()["status"], "completed", "a run that ended is left alone");
        assert_eq!(s.mark_interrupted(9_500).unwrap(), 0, "a second start-up finds nothing to do");
    }

    #[test]
    fn ending_a_run_closes_what_was_left_open() {
        let s = store("end");
        s.create_run(&run("r1", 1)).unwrap();
        s.add_step("r1", &tool_step(1, "fs.read")).unwrap();
        s.end_run("r1", 5, &RunEnd { status: status::CANCELLED.into(), stopped_reason: "cancelled".into(), rounds: 1, final_answer_head: "Stopped.".into(), ..Default::default() }).unwrap();
        let r = s.get("r1").unwrap().unwrap();
        assert_eq!((r["status"].as_str(), r["steps"][0]["status"].as_str()), (Some("cancelled"), Some("interrupted")));
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_removes_a_run_that_is_still_going() {
        let s = store("prune");
        let day = 24 * 3600 * 1000;
        s.create_run(&run("old", 1)).unwrap();
        s.end_run("old", 2, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        s.add_step("old", &tool_step(1, "fs.read")).unwrap();
        s.create_run(&run("old_but_running", 1)).unwrap();
        for i in 0..5 {
            let id = format!("n{i}");
            s.create_run(&run(&id, 100 * day + i)).unwrap();
            s.end_run(&id, 100 * day + i + 1, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        }
        // By age: only the old finished run goes.
        assert_eq!(s.prune(101 * day, 30 * day, 100).unwrap(), 1);
        assert!(s.get("old").unwrap().is_none() && s.steps("old").unwrap().is_empty(), "its steps go with it");
        assert!(s.get("old_but_running").unwrap().is_some());
        // By count: keep the newest three finished runs.
        assert_eq!(s.prune(101 * day, 30 * day, 3).unwrap(), 2);
        let left: Vec<String> = s.list(50, 0, None).unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(left, vec!["n4", "n3", "n2", "old_but_running"], "{left:?}");
    }

    #[test]
    fn the_list_is_newest_first_can_be_filtered_and_counts_tool_calls() {
        let s = store("list");
        s.create_run(&run("a", 10)).unwrap();
        s.create_run(&run("b", 20)).unwrap();
        s.add_step("b", &tool_step(1, "fs.read")).unwrap();
        s.add_step("b", &NewStep { kind: "plan".into(), ..Default::default() }).unwrap();
        s.end_run("a", 11, &RunEnd { status: status::FAILED.into(), ..Default::default() }).unwrap();
        let all = s.list(10, 0, None).unwrap();
        assert_eq!(all.iter().map(|r| r["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["b", "a"]);
        assert_eq!(all[0]["tool_calls"], 1, "only tool steps count");
        assert_eq!(s.list(10, 0, Some("failed")).unwrap().len(), 1);
        assert_eq!(s.list(1, 1, None).unwrap()[0]["id"], "a", "paged");
        assert!(s.get("nope").unwrap().is_none());
    }

    #[test]
    fn a_database_made_before_the_timings_columns_is_upgraded_in_place_and_keeps_its_rows() {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_v1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // A database as migration 0001 made it, with a run and a step in it.
            let c = Connection::open(dir.join("agent_runs.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE agent_runs (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, goal_redacted TEXT NOT NULL DEFAULT '', caller TEXT NOT NULL DEFAULT '', route TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, model TEXT NOT NULL DEFAULT '', rounds INTEGER NOT NULL DEFAULT 0, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, stopped_reason TEXT NOT NULL DEFAULT '', final_answer_head TEXT NOT NULL DEFAULT '', parent_run_id TEXT, plan_json TEXT, taint_json TEXT);
                 CREATE TABLE agent_steps (run_id TEXT NOT NULL, step INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, tool TEXT NOT NULL DEFAULT '', args_redacted TEXT NOT NULL DEFAULT '', observation_digest TEXT NOT NULL DEFAULT '', observation_head TEXT NOT NULL DEFAULT '', ok INTEGER, status TEXT NOT NULL DEFAULT 'started', decision TEXT NOT NULL DEFAULT '', approval TEXT NOT NULL DEFAULT '', ms INTEGER, tokens_in INTEGER, tokens_out INTEGER, cache_read INTEGER, planner_reply TEXT NOT NULL DEFAULT '', idempotency_digest TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL, PRIMARY KEY (run_id, seq));
                 INSERT INTO agent_runs (id, started_at, status) VALUES ('old', 1, 'completed');
                 INSERT INTO agent_steps (run_id, step, seq, kind, tool, ts) VALUES ('old', 1, 1, 'tool', 'fs.read', 1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
        let old = s.get("old").unwrap().unwrap();
        assert_eq!(old["steps"][0]["tool"], "fs.read", "what was there is still there");
        assert_eq!(old["steps"][0]["ttft_ms"], Value::Null);
        let seq = s.add_step("old", &tool_step(2, "fs.read")).unwrap();
        s.finish_step("old", seq, &StepOutcome { ok: true, status: "completed".into(), ttft_ms: Some(310), decode_tps_x10: Some(38), ..Default::default() }).unwrap();
        let steps = s.steps("old").unwrap();
        assert_eq!((steps[1]["ttft_ms"].as_i64(), steps[1]["decode_tokens_per_second"].as_f64()), (Some(310), Some(3.8)));
        drop(s);
        assert!(AgentRunStore::open(dir.join("agent_runs.db")).is_ok(), "opening it again does not try the upgrade twice");
    }

    #[test]
    fn opening_an_existing_database_keeps_its_runs_and_the_schema_version() {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_reopen_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
            s.create_run(&run("keep", 1)).unwrap();
        }
        let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
        assert!(s.get("keep").unwrap().is_some());
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn a_fresh_run_record_is_at_the_newest_version_and_a_copy_is_made_before_an_old_one_is_upgraded() {
        let w = crate::test_support::TempWorld::new("agent_runs_version");
        let path = w.path("agent_runs.db");
        let newest = STEPS.last().unwrap().id as i64;
        {
            let s = AgentRunStore::open(&path).unwrap();
            let v: i64 = s.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, newest);
            assert_eq!(STEPS.iter().map(|s| s.id).collect::<Vec<_>>(), (1..=newest as u32).collect::<Vec<_>>(), "the steps count up from 1");
            s.create_run(&run("keep", 1)).unwrap();
        }
        // The same file, put back to version 2: the upgrade to the newest copies it first, keeps the run, and does not copy again when nothing is to be upgraded.
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("ALTER TABLE agent_runs DROP COLUMN profile; ALTER TABLE agent_runs DROP COLUMN stopped_by; ALTER TABLE agent_runs DROP COLUMN triage_json; ALTER TABLE agent_runs DROP COLUMN diagnosis_json; ALTER TABLE agent_runs DROP COLUMN goal_full; PRAGMA user_version = 2;").unwrap();
        }
        let s = AgentRunStore::open(&path).unwrap();
        assert!(s.get("keep").unwrap().is_some());
        assert!(w.path("agent_runs.db.bak-v2").is_file(), "a copy of the version-2 database was kept");
        let copy = Connection::open(w.path("agent_runs.db.bak-v2")).unwrap();
        assert_eq!(copy.query_row::<i64, _, _>("SELECT count(*) FROM agent_runs", [], |r| r.get(0)).unwrap(), 1);
        drop(s);
        let before = std::fs::metadata(w.path("agent_runs.db.bak-v2")).unwrap().modified().unwrap();
        drop(AgentRunStore::open(&path).unwrap());
        assert_eq!(std::fs::metadata(w.path("agent_runs.db.bak-v2")).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn an_upgrade_that_stops_between_its_two_statements_leaves_neither_column_nor_new_version() {
        // Regression: the steps ran in autocommit. Migration 0004 adds two columns; a stop after the first left that column added and the version at 3, and the next start failed
        // on "duplicate column name" for ever. Here the second `ALTER` cannot run (its column already exists), which is the same state of affairs.
        let w = crate::test_support::TempWorld::new("agent_runs_halfway");
        let path = w.path("agent_runs.db");
        drop(AgentRunStore::open(&path).unwrap());
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("ALTER TABLE agent_runs DROP COLUMN stopped_by; ALTER TABLE agent_runs DROP COLUMN triage_json; PRAGMA user_version = 3;").unwrap();
            // `diagnosis_json` is still there, so step 4 fails on its second statement.
        }
        let err = AgentRunStore::open(&path).err().expect("the step cannot run");
        assert!(err.contains("step 4") && err.contains("rolled back"), "{err}");
        let c = Connection::open(&path).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 3, "the version was not raised");
        let has_triage = c.prepare("SELECT triage_json FROM agent_runs").is_ok();
        assert!(!has_triage, "the first ALTER of the step was rolled back with the second");
    }

    #[test]
    fn a_run_record_from_a_newer_build_is_refused_and_left_byte_for_byte_as_it_was() {
        let w = crate::test_support::TempWorld::new("agent_runs_newer");
        let path = w.path("agent_runs.db");
        {
            let s = AgentRunStore::open(&path).unwrap();
            s.create_run(&run("r1", 1)).unwrap();
            s.conn().execute_batch("PRAGMA user_version = 99;").unwrap();
            // A rollback journal, so that closing the file does not fold a write-ahead log into it.
            let _: String = s.conn().query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let err = AgentRunStore::open(&path).err().expect("refused");
        assert!(err.contains("newer than this build") && err.contains("version 99"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_damaged_run_record_is_moved_aside_and_runs_are_recorded_again_in_a_new_one() {
        let w = crate::test_support::TempWorld::new("agent_runs_damaged");
        let path = w.write("agent_runs.db", "garbage, not a database ".repeat(300));
        let s = AgentRunStore::open(&path).unwrap();
        assert_eq!(s.count().unwrap(), 0);
        s.create_run(&run("fresh", 1)).unwrap();
        assert_eq!(s.count().unwrap(), 1);
        let aside: Vec<_> = crate::storage::sqlite_store::leftovers(&w.root);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("garbage"), "the old file is kept");
    }

    #[test]
    fn what_a_run_holds_is_kept_the_moment_it_changes_and_the_long_goal_is_kept_for_resuming() {
        let s = store("marks");
        s.create_run(&NewRun { goal_full: "read the page and write the long report ".repeat(30), ..run("r1", 1_000) }).unwrap();
        assert_eq!(s.marks("r1").unwrap(), None, "nothing is known until something is written");
        s.set_marks("r1", true, false).unwrap();
        assert_eq!(s.marks("r1").unwrap(), Some((true, false)));
        s.set_marks("r1", true, true).unwrap();
        assert_eq!(s.marks("r1").unwrap(), Some((true, true)));
        assert_eq!(s.marks("nope").unwrap(), None);
        assert!(s.goal_for_resume("r1").unwrap().unwrap().len() > 600, "the long goal, and not the short one that the API shows");
        s.create_run(&run("r2", 2_000)).unwrap();
        assert_eq!(s.goal_for_resume("r2").unwrap().as_deref(), Some("goal r2"), "a run that kept no long goal falls back to the short one");
        assert_eq!(s.goal_for_resume("nope").unwrap(), None);
        // The API's view of a run does not carry the long goal.
        assert!(!s.get("r1").unwrap().unwrap().to_string().contains("write the long report write the long report"));
    }

    #[test]
    fn the_calls_that_were_running_when_a_run_stopped_are_listed_and_the_finished_ones_are_not() {
        let s = store("unknown");
        s.create_run(&run("r1", 1_000)).unwrap();
        let done = s.add_step("r1", &tool_step(1, "fs.read")).unwrap();
        s.finish_step("r1", done, &StepOutcome { ok: true, status: "completed".into(), ..Default::default() }).unwrap();
        let mut write = tool_step(2, "fs.write");
        write.idempotency_digest = "abcdef0123456789".into();
        s.add_step("r1", &write).unwrap();
        s.add_step("r1", &NewStep { step: 2, kind: "plan".into(), ts: 2, ..Default::default() }).unwrap();
        s.mark_interrupted(5_000).unwrap();
        let unknown = s.unknown_steps("r1").unwrap();
        assert_eq!(unknown.len(), 1, "{unknown:?}");
        assert_eq!((unknown[0].tool.as_str(), unknown[0].digest.as_str(), unknown[0].step), ("fs.write", "abcdef0123456789", 2));
        assert!(s.unknown_steps("nope").unwrap().is_empty());
    }

    #[test]
    fn the_row_that_announces_a_call_is_written_with_a_full_sync_and_the_setting_is_put_back() {
        let s = store("sync");
        s.create_run(&NewRun { id: "r".into(), started_at: 1, ..Default::default() }).unwrap();
        let sync = |s: &AgentRunStore| -> i64 { s.conn().query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap() };
        assert_eq!(sync(&s), 1, "NORMAL");
        s.add_step("r", &NewStep { step: 1, kind: "tool".into(), tool: "fs.write".into(), ts: 2, ..Default::default() }).unwrap();
        assert_eq!(sync(&s), 1, "back to NORMAL after the announce");
        // A write that fails still puts it back.
        s.conn().execute_batch("DROP TABLE agent_steps").unwrap();
        assert!(s.add_step("r", &NewStep { step: 2, ..Default::default() }).is_err());
        assert_eq!(sync(&s), 1);
    }

    #[test]
    fn a_question_nobody_answered_expires_when_the_idle_time_is_over_and_all_of_them_at_start_up() {
        let s = store("expire");
        for (id, ended) in [("old", 1_000), ("new", 9_000)] {
            s.create_run(&NewRun { id: id.into(), started_at: ended - 500, ..Default::default() }).unwrap();
            s.end_run(id, ended, &RunEnd { status: status::WAITING_USER.into(), stopped_reason: "waiting_user".into(), ..Default::default() }).unwrap();
        }
        s.create_run(&NewRun { id: "going".into(), started_at: 1, ..Default::default() }).unwrap();
        let state = |s: &AgentRunStore, id: &str| s.get(id).unwrap().unwrap()["status"].as_str().unwrap().to_string();
        assert_eq!(s.expire_waiting(10_000, 20_000).unwrap(), 0, "within the idle time");
        assert_eq!(s.expire_waiting(10_000, 5_000).unwrap(), 1, "only the old one is past it");
        assert_eq!((state(&s, "old"), state(&s, "new"), state(&s, "going")), ("expired".to_string(), "waiting_user".to_string(), "running".to_string()));
        assert_eq!(s.get("old").unwrap().unwrap()["stopped_reason"], "question_expired");
        assert_eq!(s.expire_waiting(10_000, 0).unwrap(), 1, "at start-up nothing is waiting any more");
        assert_eq!(state(&s, "new"), "expired");
        assert_eq!(state(&s, "going"), "running", "a run that is going is not a question");
    }

    #[test]
    fn a_run_that_writes_nothing_for_a_while_is_stalled_if_it_is_alive_and_interrupted_if_it_is_not_and_one_step_makes_it_running_again() {
        let s = store("stalled");
        s.create_run(&run("alive", 1_000)).unwrap();
        s.create_run(&run("gone", 1_000)).unwrap();
        s.create_run(&run("busy", 1_000)).unwrap();
        s.add_step("busy", &NewStep { step: 1, kind: "plan".into(), ts: 299_000, ..Default::default() }).unwrap();
        s.add_step("gone", &tool_step(1, "term.run")).unwrap();
        let alive = |id: &str| id != "gone";
        // Four minutes after the start nothing is stalled yet.
        assert!(s.mark_stalled(241_000, 300_000, &alive).unwrap().is_empty());
        // Five minutes and a bit: the two that wrote nothing since their start (`busy` wrote a step a moment ago).
        let stalled = s.mark_stalled(302_000, 300_000, &alive).unwrap();
        assert_eq!(stalled, vec!["alive".to_string()]);
        let get = |id: &str| s.get(id).unwrap().unwrap();
        assert_eq!(get("alive")["status"], "stalled");
        assert_eq!(get("busy")["status"], "running");
        assert_eq!(get("gone")["status"], "interrupted", "a run nobody is working on can never go on by itself");
        assert_eq!(get("gone")["steps"][0]["status"], "interrupted", "and the call it had open did not finish");
        // A step is progress: the run was only slow.
        s.add_step("alive", &tool_step(1, "fs.read")).unwrap();
        assert_eq!(get("alive")["status"], "running");
        // A stalled run is not final: the start-up marks it interrupted, and pruning never takes it.
        s.mark_stalled(900_000, 300_000, &alive).unwrap();
        assert_eq!(get("alive")["status"], "stalled");
        assert_eq!(s.prune(10_000_000_000, 1, 0).unwrap(), 1, "busy and alive are still going, so only the one that was interrupted goes");
        assert_eq!(s.mark_interrupted(1_000_000).unwrap(), 2);
        assert_eq!(s.get("alive").unwrap().unwrap()["status"], "interrupted");
    }

    #[test]
    fn a_database_made_before_the_goal_column_is_upgraded_in_place() {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_v2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let c = Connection::open(dir.join("agent_runs.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE agent_runs (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, goal_redacted TEXT NOT NULL DEFAULT '', caller TEXT NOT NULL DEFAULT '', route TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, model TEXT NOT NULL DEFAULT '', rounds INTEGER NOT NULL DEFAULT 0, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, stopped_reason TEXT NOT NULL DEFAULT '', final_answer_head TEXT NOT NULL DEFAULT '', parent_run_id TEXT, plan_json TEXT, taint_json TEXT);
                 CREATE TABLE agent_steps (run_id TEXT NOT NULL, step INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, tool TEXT NOT NULL DEFAULT '', args_redacted TEXT NOT NULL DEFAULT '', observation_digest TEXT NOT NULL DEFAULT '', observation_head TEXT NOT NULL DEFAULT '', ok INTEGER, status TEXT NOT NULL DEFAULT 'started', decision TEXT NOT NULL DEFAULT '', approval TEXT NOT NULL DEFAULT '', ms INTEGER, tokens_in INTEGER, tokens_out INTEGER, cache_read INTEGER, planner_reply TEXT NOT NULL DEFAULT '', idempotency_digest TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL, ttft_ms INTEGER, decode_tps_x10 INTEGER, PRIMARY KEY (run_id, seq));
                 INSERT INTO agent_runs (id, started_at, status, goal_redacted) VALUES ('old', 1, 'interrupted', 'the old goal');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
        assert_eq!(s.goal_for_resume("old").unwrap().as_deref(), Some("the old goal"), "a run from before has its short goal to go on from");
        s.create_run(&NewRun { goal_full: "new".into(), ..run("new", 2) }).unwrap();
        assert_eq!(s.goal_for_resume("new").unwrap().as_deref(), Some("new"));
    }

    /// Card R7-18: the cap that stopped a run is kept in the run's record, shown in the list and in the run, and empty for every run no cap stopped.
    #[test]
    fn the_cap_that_stopped_a_run_is_kept_with_it_and_shown_in_the_list_and_in_the_run() {
        let s = store("stopped_by");
        s.create_run(&run("capped", 10)).unwrap();
        s.create_run(&run("free", 20)).unwrap();
        s.create_run(&run("going", 30)).unwrap();
        s.end_run("capped", 11, &RunEnd { status: status::FAILED.into(), stopped_reason: "final".into(), rounds: 3, stopped_by: "max_tool_calls".into(), ..Default::default() }).unwrap();
        s.end_run("free", 21, &RunEnd { status: status::COMPLETED.into(), stopped_reason: "final".into(), ..Default::default() }).unwrap();
        assert_eq!(s.get("capped").unwrap().unwrap()["stopped_by"], "max_tool_calls");
        assert_eq!((s.get("capped").unwrap().unwrap()["status"].as_str(), s.get("capped").unwrap().unwrap()["stopped_reason"].as_str()), (Some("failed"), Some("final")), "the reason the loop gave is not replaced");
        assert_eq!(s.get("free").unwrap().unwrap()["stopped_by"], "", "a run no cap stopped");
        assert_eq!(s.get("going").unwrap().unwrap()["stopped_by"], "", "a run that is going");
        let listed = s.list(10, 0, None).unwrap();
        let by = |id: &str| listed.iter().find(|r| r["id"] == id).unwrap()["stopped_by"].clone();
        assert_eq!((by("capped"), by("free"), by("going")), (json!("max_tool_calls"), json!(""), json!("")));
        // A run that stopped for another reason and can be gone on from is listed with the field too.
        s.create_run(&run("cut", 5)).unwrap();
        s.mark_interrupted(99).unwrap();
        assert!(s.list_resumable(10).unwrap().iter().any(|r| r["id"] == "cut" && r["stopped_by"] == ""));
    }

    /// Card R7-18: a database made before the column is upgraded in place and keeps its runs, whatever version it was at.
    #[test]
    fn a_database_made_before_the_stopped_by_column_is_upgraded_in_place_and_keeps_its_runs() {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_v4_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // A database as migrations 0001 to 0004 made it, with a run in it.
            let c = Connection::open(dir.join("agent_runs.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE agent_runs (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, goal_redacted TEXT NOT NULL DEFAULT '', caller TEXT NOT NULL DEFAULT '', route TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, model TEXT NOT NULL DEFAULT '', rounds INTEGER NOT NULL DEFAULT 0, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, stopped_reason TEXT NOT NULL DEFAULT '', final_answer_head TEXT NOT NULL DEFAULT '', parent_run_id TEXT, plan_json TEXT, taint_json TEXT, goal_full TEXT NOT NULL DEFAULT '', triage_json TEXT, diagnosis_json TEXT);
                 CREATE TABLE agent_steps (run_id TEXT NOT NULL, step INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, tool TEXT NOT NULL DEFAULT '', args_redacted TEXT NOT NULL DEFAULT '', observation_digest TEXT NOT NULL DEFAULT '', observation_head TEXT NOT NULL DEFAULT '', ok INTEGER, status TEXT NOT NULL DEFAULT 'started', decision TEXT NOT NULL DEFAULT '', approval TEXT NOT NULL DEFAULT '', ms INTEGER, tokens_in INTEGER, tokens_out INTEGER, cache_read INTEGER, planner_reply TEXT NOT NULL DEFAULT '', idempotency_digest TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL, ttft_ms INTEGER, decode_tps_x10 INTEGER, PRIMARY KEY (run_id, seq));
                 INSERT INTO agent_runs (id, started_at, status, goal_redacted, stopped_reason) VALUES ('old', 1, 'completed', 'the old goal', 'final');
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        let s = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
        let old = s.get("old").unwrap().unwrap();
        assert_eq!((old["goal"].as_str(), old["stopped_reason"].as_str(), old["stopped_by"].as_str()), (Some("the old goal"), Some("final"), Some("")), "what was there is still there, and no cap stopped it");
        s.create_run(&run("new", 2)).unwrap();
        s.end_run("new", 3, &RunEnd { status: status::FAILED.into(), stopped_by: "max_seconds".into(), ..Default::default() }).unwrap();
        assert_eq!(s.get("new").unwrap().unwrap()["stopped_by"], "max_seconds");
        drop(s);
        assert!(AgentRunStore::open(dir.join("agent_runs.db")).is_ok(), "opening it again does not try the upgrade twice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the events of a run (card OA10-01) ----------------------------------------------------------------------------------

    fn note(i: usize) -> Event {
        Event::Text { index: i as u32, text: format!("piece {i}") }
    }

    /// Card OA10-01 (UX-02): the numbers inside a run are 1, 2, 3 ... with no gap and no repeat, also when several threads write to the one run, and everything is there when the file is
    /// opened again; the writing goes on from the last number.
    #[test]
    fn events_are_gap_free_and_survive_reopen() {
        let w = crate::test_support::TempWorld::new("run_events_gap_free");
        let path = w.path("agent_runs.db");
        {
            let s = std::sync::Arc::new(AgentRunStore::open(&path).unwrap());
            s.create_run(&run("r1", 1)).unwrap();
            s.create_run(&run("r2", 2)).unwrap();
            let writers: Vec<_> = (0..4)
                .map(|t| {
                    let s = s.clone();
                    std::thread::spawn(move || {
                        for i in 0..50 {
                            s.append_event("r1", &note(t * 50 + i)).unwrap().expect("written");
                            s.append_event("r2", &note(i)).unwrap().expect("written");
                        }
                    })
                })
                .collect();
            for t in writers {
                t.join().unwrap();
            }
            let rows = s.events_after("r1", 0, 1_000).unwrap();
            assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), (1..=200).collect::<Vec<i64>>(), "no gap, no repeat, in order");
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            assert!(ids.windows(2).all(|p| p[0] < p[1]), "the number across runs only grows");
            assert_eq!(s.events_after("r2", 0, 1_000).unwrap().len(), 200, "another run has its own numbers");
            assert_eq!(s.last_event_seq("r1").unwrap(), 200);
        }
        let s = AgentRunStore::open(&path).unwrap();
        let rows = s.events_after("r1", 0, 1_000).unwrap();
        assert_eq!(rows.len(), 200);
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), (1..=200).collect::<Vec<i64>>());
        assert_eq!(s.append_event("r1", &note(999)).unwrap().unwrap().seq, 201, "the next number after a reopen");
        assert_eq!(s.last_event_id().unwrap(), 401);
    }

    #[test]
    fn after_cursor_returns_only_newer() {
        let s = store("run_events_after");
        s.create_run(&run("r1", 1)).unwrap();
        for i in 0..30 {
            s.append_event("r1", &note(i)).unwrap();
        }
        let seqs = |after: i64, limit: u32| s.events_after("r1", after, limit).unwrap().iter().map(|r| r.seq).collect::<Vec<_>>();
        assert_eq!(seqs(5, 10), (6..=15).collect::<Vec<i64>>(), "only newer, at most the limit");
        assert_eq!(seqs(0, 3), vec![1, 2, 3]);
        assert_eq!(seqs(29, 10), vec![30]);
        assert!(seqs(30, 10).is_empty() && seqs(500, 10).is_empty(), "a cursor at or past the end is an empty page, not an error");
        assert_eq!(seqs(-4, 2), vec![1, 2], "a negative cursor is the start");
        assert!(s.events_after("nope", 0, 10).unwrap().is_empty());
        // Across runs, by the global number.
        s.create_run(&run("r2", 2)).unwrap();
        s.append_event("r2", &note(0)).unwrap();
        let since = s.events_since(29, 10).unwrap();
        assert_eq!(since.iter().map(|r| (r.run_id.as_str(), r.seq)).collect::<Vec<_>>(), vec![("r1", 30), ("r2", 1)]);
    }

    #[test]
    fn an_event_keeps_its_two_parts_and_a_frame_of_a_stream_is_never_written() {
        let s = store("run_events_parts");
        s.create_run(&run("r1", 1)).unwrap();
        let started = s.append_event("r1", &Event::ToolStarted { call_id: 3, tool: "fs.read".into(), args_head: r#"{"path":"a.txt"}"#.into() }).unwrap().unwrap();
        assert_eq!((started.kind.as_str(), started.seq), ("tool.started", 1));
        let back = &s.events_after("r1", 0, 10).unwrap()[0];
        assert_eq!(back.data.get("tool").and_then(Value::as_str), Some("fs.read"));
        assert!(back.data.get("args_head").is_none(), "the arguments are not in the part everybody reads");
        assert_eq!(back.ui.get("args_head").and_then(Value::as_str), Some(r#"{"path":"a.txt"}"#));
        for frame in [Event::Heartbeat, Event::End { last: 1 }, Event::Connected { last: 1 }] {
            assert_eq!(s.append_event("r1", &frame).unwrap(), None, "{frame:?}");
        }
        assert_eq!(s.last_event_seq("r1").unwrap(), 1);
    }

    #[test]
    fn a_run_ends_once_in_its_log() {
        let s = store("run_events_once");
        s.create_run(&run("r1", 1)).unwrap();
        let finished = |status: &str| Event::RunFinished { status: status.into(), stopped_reason: "final".into(), answer_head: String::new(), why_it_stopped: None, files_changed: 0, ms: 5 };
        assert!(s.append_event("r1", &finished("completed")).unwrap().is_some());
        assert_eq!(s.append_event("r1", &finished("failed")).unwrap(), None, "a second end is not written");
        let rows = s.events_after("r1", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].data.get("status").and_then(Value::as_str), Some("completed"), "the first end stands");
    }

    #[test]
    fn pruning_a_run_removes_its_events_and_the_numbers_are_not_given_again() {
        let s = store("run_events_prune");
        let day = 24 * 3600 * 1000;
        s.create_run(&run("old", 1)).unwrap();
        s.end_run("old", 2, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        s.set_origin("old", "api").unwrap();
        for i in 0..5 {
            s.append_event("old", &note(i)).unwrap();
        }
        s.create_run(&run("going", 1)).unwrap();
        s.append_event("going", &note(0)).unwrap();
        let newest = s.last_event_id().unwrap();
        assert_eq!(s.prune(100 * day, 30 * day, 100).unwrap(), 1);
        assert!(s.events_after("old", 0, 10).unwrap().is_empty() && s.log_head("old").unwrap().is_none(), "the events go with the run");
        assert_eq!(s.events_after("going", 0, 10).unwrap().len(), 1, "a run that is going keeps its events");
        assert_eq!(s.last_event_id().unwrap(), newest, "the numbers already given stay given");
        assert!(s.append_event("going", &note(1)).unwrap().unwrap().id > newest, "a deleted row's number is not given to another event");
    }

    /// Review fix (narrowing lost after the 64-run ring): the tools a command allowed a run are kept with the run, replaced when they are said again, nothing for a run that was not narrowed,
    /// and they go with the run when it is pruned.
    #[test]
    fn the_tools_a_command_allowed_are_kept_with_the_run_and_go_with_it_when_it_is_pruned() {
        let s = store("run_allowed_tools");
        let day = 24 * 3600 * 1000;
        s.create_run(&run("old", 1)).unwrap();
        s.end_run("old", 2, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        s.create_run(&run("free", 1)).unwrap();
        assert!(s.allowed_tools_of("old").unwrap().is_empty(), "nothing kept before it is said");
        s.set_allowed_tools("old", &["fs.read".to_string(), "git.*".to_string()]).unwrap();
        assert_eq!(s.allowed_tools_of("old").unwrap(), vec!["fs.read".to_string(), "git.*".to_string()]);
        s.set_allowed_tools("old", &["fs.read".to_string()]).unwrap();
        assert_eq!(s.allowed_tools_of("old").unwrap(), vec!["fs.read".to_string()], "said again, replaced");
        assert!(s.allowed_tools_of("free").unwrap().is_empty() && s.allowed_tools_of("nobody").unwrap().is_empty());
        // The origin of a run is another row and does not touch it.
        s.set_origin("old", "api").unwrap();
        assert_eq!(s.allowed_tools_of("old").unwrap(), vec!["fs.read".to_string()]);
        assert_eq!(s.prune(100 * day, 30 * day, 100).unwrap(), 1);
        assert!(s.allowed_tools_of("old").unwrap().is_empty(), "the list goes with the run");
    }

    /// Card OA11-04 (security review): a run narrowed by two lists (a command's and an agent's) goes on after a restart narrowed by both, since a call has to be in every list; one list is kept
    /// in the format the earlier versions read, and a row that cannot be read is a list that allows nothing and not "no list" (which would give the run every tool).
    #[test]
    fn a_run_narrowed_by_two_lists_keeps_both_and_a_row_that_cannot_be_read_allows_nothing() {
        let s = store("run_allowed_lists");
        s.create_run(&run("two", 1)).unwrap();
        let (command, agent) = (vec!["fs.read".to_string(), "git.*".to_string()], vec!["fs.read".to_string(), "fs.grep".to_string()]);
        s.set_allowed_lists("two", &[command.clone(), agent.clone()]).unwrap();
        assert_eq!(s.allowed_lists_of("two").unwrap(), vec![command.clone(), agent.clone()], "both lists, in order");
        assert_eq!(s.allowed_tools_of("two").unwrap(), command, "the old reader gets the first");
        // One list is the format of before (a plain array of names), so a store that an earlier version wrote or reads is the same.
        s.set_allowed_lists("one", &[Vec::new(), agent.clone()]).unwrap();
        let text: String = s.conn().query_row("SELECT tools FROM run_allowed_tools WHERE run_id = 'one'", [], |r| r.get(0)).unwrap();
        assert_eq!(text, r#"["fs.read","fs.grep"]"#);
        assert_eq!(s.allowed_lists_of("one").unwrap(), vec![agent.clone()]);
        s.set_allowed_tools("old", &command).unwrap();
        assert_eq!(s.allowed_lists_of("old").unwrap(), vec![command.clone()], "a list kept by set_allowed_tools is one list");
        // Nothing is kept for no list, and nothing is read for a run that has none.
        s.set_allowed_lists("none", &[Vec::new()]).unwrap();
        assert!(s.allowed_lists_of("none").unwrap().is_empty() && s.allowed_lists_of("nobody").unwrap().is_empty());
        // A row that is neither shape is no reason to give the run every tool.
        s.conn().execute("INSERT OR REPLACE INTO run_allowed_tools (run_id, tools) VALUES ('broken', 'not json')", []).unwrap();
        let broken = s.allowed_lists_of("broken").unwrap();
        assert_eq!(broken.len(), 1, "{broken:?}");
        assert!(!broken[0].iter().any(|p| p == "*" || p == "fs.read"), "{broken:?}");
        s.conn().execute("INSERT OR REPLACE INTO run_allowed_tools (run_id, tools) VALUES ('empty', '[]')", []).unwrap();
        assert!(s.allowed_lists_of("empty").unwrap().is_empty(), "the column's own default is no list");
    }

    /// Card OA10-01: a run that Aqua's restart (or the watchdog) ended has a `run.finished` in its log, so a reader is told it is over.
    #[test]
    fn the_start_up_sweep_and_the_watchdog_end_the_log_of_a_run_they_interrupt() {
        let s = store("run_events_sweep");
        s.create_run(&run("cut", 1_000)).unwrap();
        s.append_event("cut", &Event::Status { state: crate::agent_runs::events::RunState::Running, position: None, reason: None }).unwrap();
        s.create_run(&run("fine", 1_000)).unwrap();
        s.end_run("fine", 1_500, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        assert!(!s.log_head("cut").unwrap().unwrap().finished, "going: not finished");
        assert_eq!(s.mark_interrupted(9_000).unwrap(), 1);
        let rows = s.events_after("cut", 0, 10).unwrap();
        assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["status", "run.finished"]);
        assert_eq!((rows[1].data["status"].as_str(), rows[1].data["stopped_reason"].as_str(), rows[1].data["ms"].as_i64()), (Some("interrupted"), Some("process_ended"), Some(8_000)));
        assert!(s.log_head("cut").unwrap().unwrap().finished);
        assert!(s.events_after("fine", 0, 10).unwrap().is_empty(), "a run that ended on its own is not given an end by the sweep");
        // The watchdog: a silent run this process is not working on.
        s.create_run(&run("gone", 2_000)).unwrap();
        let stalled = s.mark_stalled(2_000 + 6 * 60_000, 5 * 60_000, &|_| false).unwrap();
        assert!(stalled.is_empty());
        let rows = s.events_after("gone", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].kind.as_str(), rows[0].data["stopped_reason"].as_str()), ("run.finished", Some("no_longer_running")));
    }

    /// What a reader is told before it reads: who asked, and whether the log is complete.
    #[test]
    fn the_head_of_a_log_says_who_asked_and_when_nothing_more_will_come() {
        let s = store("run_events_head");
        assert_eq!(s.log_head("nope").unwrap(), None);
        s.create_run(&run("a", 1)).unwrap();
        let head = s.log_head("a").unwrap().unwrap();
        assert_eq!((head.origin.as_str(), head.status.as_str(), head.finished), ("", "running", false), "going, with no event yet: not finished");
        s.set_origin("a", "api").unwrap();
        s.set_origin("a", "ui").unwrap();
        assert_eq!(s.log_head("a").unwrap().unwrap().origin, "ui", "the last word stands");
        // Ended before the log existed (or with the log off): finished with nothing to read, so a stream does not wait for an end that will not come.
        s.create_run(&run("old", 1)).unwrap();
        s.end_run("old", 2, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        assert!(s.log_head("old").unwrap().unwrap().finished);
        // Ended, with a log that has no end yet: the end is still being written, so not finished.
        s.create_run(&run("closing", 1)).unwrap();
        s.append_event("closing", &note(0)).unwrap();
        s.end_run("closing", 2, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        assert!(!s.log_head("closing").unwrap().unwrap().finished);
        s.append_event("closing", &Event::RunFinished { status: "completed".into(), stopped_reason: "final".into(), answer_head: String::new(), why_it_stopped: None, files_changed: 0, ms: 1 }).unwrap();
        assert!(s.log_head("closing").unwrap().unwrap().finished);
    }

    /// A database made before the events existed (version 5) is upgraded in place and keeps its runs; the new tables are empty.
    #[test]
    fn a_database_from_before_the_events_is_upgraded_in_place_and_keeps_its_runs() {
        let w = crate::test_support::TempWorld::new("run_events_from_v5");
        let path = w.path("agent_runs.db");
        {
            let s = AgentRunStore::open(&path).unwrap();
            s.create_run(&run("kept", 1)).unwrap();
            s.conn().execute_batch("DROP TABLE run_events; DROP TABLE run_meta; ALTER TABLE agent_runs DROP COLUMN profile; PRAGMA user_version = 5;").unwrap();
        }
        let s = AgentRunStore::open(&path).unwrap();
        assert!(s.get("kept").unwrap().is_some());
        assert!(s.events_after("kept", 0, 10).unwrap().is_empty());
        assert!(s.append_event("kept", &note(0)).unwrap().is_some());
        assert!(w.path("agent_runs.db.bak-v5").is_file(), "a copy of the version-5 database was kept before the upgrade");
    }

    /// Card OA4-01: what kind of task a run was is kept in its record. A database from before the column (version 6) is upgraded in place and keeps its runs, which have none; a run that
    /// is given one shows it in the record, in the list and in what `profile_of` answers (an empty one is none); the profile written last is the one that stays.
    #[test]
    fn a_run_record_keeps_what_kind_of_task_the_run_was_and_an_older_database_is_upgraded_in_place() {
        let w = crate::test_support::TempWorld::new("run_profile_from_v6");
        let path = w.path("agent_runs.db");
        {
            let s = AgentRunStore::open(&path).unwrap();
            s.create_run(&run("old", 1)).unwrap();
            s.conn().execute_batch("ALTER TABLE agent_runs DROP COLUMN profile; PRAGMA user_version = 6;").unwrap();
        }
        let s = AgentRunStore::open(&path).unwrap();
        assert!(w.path("agent_runs.db.bak-v6").is_file(), "a copy of the version-6 database was kept before the upgrade");
        assert_eq!(s.get("old").unwrap().unwrap()["profile"], "", "a run from before has none");
        assert_eq!(s.profile_of("old"), None);
        assert_eq!(s.profile_of("nobody"), None);
        s.create_run(&run("new", 2)).unwrap();
        assert_eq!(s.profile_of("new"), None, "none until it is written");
        s.set_profile("new", "desktop").unwrap();
        assert_eq!(s.profile_of("new").as_deref(), Some("desktop"));
        // A run that began as something else and changed code is written again.
        s.set_profile("new", "coding").unwrap();
        assert_eq!(s.profile_of("new").as_deref(), Some("coding"));
        assert_eq!(s.get("new").unwrap().unwrap()["profile"], "coding");
        let listed = s.list(10, 0, None).unwrap();
        let by = |id: &str| listed.iter().find(|r| r["id"] == id).unwrap()["profile"].clone();
        assert_eq!((by("new"), by("old")), (json!("coding"), json!("")));
        // A run that is unknown is no error.
        assert!(s.set_profile("nobody", "web").is_ok() && s.profile_of("nobody").is_none());
    }

    /// Card OA4-04 (WF-06): the coding record of a run is kept with it (migration 0009): written again as the run goes on, shown in the record of the run and not in the list, and removed with the
    /// run. A database from before (version 8) is upgraded in place and keeps its runs, which have none.
    #[test]
    fn a_coding_record_is_kept_replaced_shown_with_the_run_and_removed_with_it() {
        let w = crate::test_support::TempWorld::new("coding_record_from_v8");
        let path = w.path("agent_runs.db");
        {
            let s = AgentRunStore::open(&path).unwrap();
            s.create_run(&run("old", 1)).unwrap();
            s.conn().execute_batch("DROP TABLE coding_records; PRAGMA user_version = 8;").unwrap();
        }
        let s = AgentRunStore::open(&path).unwrap();
        assert!(w.path("agent_runs.db.bak-v8").is_file(), "a copy of the version-8 database was kept before the upgrade");
        assert!(s.get("old").unwrap().unwrap()["record"].is_null(), "a run from before has none");
        assert!(s.coding_record_of("old").is_none() && s.coding_record_of("nobody").is_none());
        s.create_run(&run("new", 2)).unwrap();
        s.set_coding_record("new", r#"{"next":"run the check again","edits_since_check":true}"#, 10).unwrap();
        s.set_coding_record("new", r#"{"next":"give the final answer with the result of the check","edits_since_check":false}"#, 20).unwrap();
        let shown = s.get("new").unwrap().unwrap();
        assert_eq!(shown["record"]["next"], "give the final answer with the result of the check", "the one written last stays");
        assert_eq!(shown["record"]["edits_since_check"], false);
        assert_eq!(s.coding_record_of("new").unwrap()["next"], "give the final answer with the result of the check");
        assert!(s.list(10, 0, None).unwrap().iter().all(|r| r.get("record").is_none()), "the list is a summary");
        // A record that is not JSON is none, not an error.
        s.conn().execute("UPDATE coding_records SET record = 'not json' WHERE run_id = 'new'", []).unwrap();
        assert!(s.get("new").unwrap().unwrap()["record"].is_null());
        // It goes when its run is pruned.
        s.set_coding_record("new", r#"{"next":"x"}"#, 30).unwrap();
        s.end_run("new", 40, &RunEnd { status: status::COMPLETED.into(), ..Default::default() }).unwrap();
        let day = 24 * 3600 * 1000;
        assert_eq!(s.prune(100 * day, 30 * day, 100).unwrap(), 1, "only the run that ended and is old goes");
        assert!(s.get("new").unwrap().is_none());
        let rows: i64 = s.conn().query_row("SELECT count(*) FROM coding_records WHERE run_id = 'new'", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0, "its record went with it");
        // A call with nobody to write for is no error.
        crate::agent_runs::note_coding_record(r#"{"next":"x"}"#);
    }
}
