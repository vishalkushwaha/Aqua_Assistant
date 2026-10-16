//! Sessions over runs, and the state a run goes on from (card OA10-04; tasks UX-10, AG-15 and AG-06 of the OpenCode audit).
//!
//! A **session** is the runs that belong together: a run is in its own session unless it goes on from another run (it then has that run's session: after a question, a resume, a
//! continuation, an approved plan) or its request named a `session_id`. A run from before the tables existed has no row, and its session is its own id, so the list needs no back-fill.
//!
//! * `title`: the one the person gave (`PATCH /api/v1/sessions/{id}`), else the first sentence of the first goal, at most 60 characters. Never made by the model: a title that took a call
//!   of the model would compete with the person's own turn.
//! * search: `run_search` is a full-text index of the words of what each run was asked and what it answered (both already masked in the record), and nothing else: never a tool's arguments
//!   or result. It is filled when a search is made, from the runs that ended and are not in it yet, so a run costs nothing to be searchable until somebody searches.
//! * a **fork** is a new, empty session that says which run it was made from; the first run started in it is told, as data, what that run was asked and answered.
//! * deleting a session removes every row of its runs (steps, events, questions, records, index entries) with the database set to overwrite what it deletes, as `forget` does for the
//!   person's facts.
//!
//! This file also keeps what a run needs to be gone on from and is not in the steps (`run_state`, migration 0011): the checklist of `todo.write` and the lines the person added while it ran
//! (AG-15: a run that is resumed had lost both), and the plan a plan run wrote with what the person did about it (AG-06). All of it is model or person text that is **data**: nothing here
//! widens a permission.

use super::store::{delete_run_rows, run_json, status, AgentRunStore, R};
use base64::Engine;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;

fn db(e: rusqlite::Error) -> String {
    format!("agent run store: {e}")
}

/// The longest session id a client may name.
pub const MAX_ID_CHARS: usize = 64;
/// The title made from a goal.
pub const TITLE_CHARS: usize = 60;
/// The longest title a person may give.
pub const MAX_TITLE_CHARS: usize = 120;
/// What a checklist may take up as JSON.
const CHECKLIST_BYTES: usize = 2_048;
/// The most lines the person's additions to a run keep, and the longest (the same as `steer`'s).
const STEERED_LINES: usize = 5;
const STEERED_CHARS: usize = 500;
/// A plan that nobody approved is gone after this long.
pub const PLAN_EXPIRES_MS: i64 = 24 * 3600 * 1000;
/// The most characters of a plan.
pub const PLAN_CHARS: usize = 1_500;

/// Letters, digits and `_ . : -`, 1 to 64 of them: what a client may name a session.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_CHARS && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// A new id for a session that nobody named.
pub fn new_session_id() -> String {
    format!("ses_{}", crate::agent_runs::scope::new_run_id().trim_start_matches("run_"))
}

fn masked_line(text: &str, max: usize) -> String {
    let masked = aqua_tool_host::output::Redactor::new().redact(text);
    let one: String = masked.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    cut(&one, max)
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A title as a person may give it: one line, secrets masked, at most [`MAX_TITLE_CHARS`] characters.
pub fn clean_title(text: &str) -> String {
    masked_line(text, MAX_TITLE_CHARS)
}

/// The title a goal makes: its first sentence (a dot, question mark or exclamation mark followed by a space ends it; the dot of a file name does not), at most 60 characters. The sentence
/// the API puts in front of a goal that names a folder ("Work in the folder ...") is not part of it.
pub fn title_of(goal: &str) -> String {
    let one: String = goal.split_whitespace().collect::<Vec<_>>().join(" ");
    let one = match one.strip_prefix("Work in the folder ") {
        Some(rest) => rest.split_once(". ").map_or(one.as_str(), |(_, after)| after).to_string(),
        None => one,
    };
    let chars: Vec<(usize, char)> = one.char_indices().collect();
    let mut end = one.len();
    for (i, (pos, c)) in chars.iter().enumerate() {
        if matches!(c, '.' | '?' | '!') && chars.get(i + 1).is_none_or(|(_, next)| next.is_whitespace()) {
            end = *pos;
            break;
        }
    }
    cut(one[..end].trim(), TITLE_CHARS)
}

// ---- which session a run is in ---------------------------------------------------------------------------------------------------------

impl AgentRunStore {
    /// Puts a run in a session (and says which folder it worked in, when that is known). Written again when a run moves to another session (a run that joins the open session of its
    /// project) or learns its folder; a folder that is not given leaves the one that was written.
    pub fn set_run_session(&self, run_id: &str, session_id: &str, directory: &str) -> R<()> {
        self.conn()
            .execute(
                "INSERT INTO run_sessions (run_id, session_id, directory) VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET session_id = excluded.session_id, directory = CASE WHEN excluded.directory != '' THEN excluded.directory ELSE run_sessions.directory END",
                params![run_id, session_id, directory],
            )
            .map_err(db)?;
        Ok(())
    }

    /// The session a run was put in; `None` for a run that has no row (a run from before: its session is its own id).
    pub fn session_of(&self, run_id: &str) -> Option<String> {
        self.conn().query_row("SELECT session_id FROM run_sessions WHERE run_id = ?1", [run_id], |r| r.get(0)).optional().ok().flatten()
    }

    /// The session of a run as a reader sees it: the one it was put in, else its own id.
    pub fn session_id_of(&self, run_id: &str) -> String {
        self.session_of(run_id).unwrap_or_else(|| run_id.to_string())
    }

    /// Is there a session called `id`: a run in it, or a fork that was made with that id?
    pub fn session_exists(&self, id: &str) -> bool {
        let c = self.conn();
        session_exists_in(&c, id)
    }

    /// Does every run of the session belong to the API token (it asked for it), and has the session a run? What the API token may read of the sessions (`GET /api/v1/sessions`): its own.
    pub fn session_is_api_own(&self, id: &str) -> bool {
        let c = self.conn();
        let (total, foreign): (i64, i64) = c
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(CASE WHEN COALESCE(m.origin, '') = 'api' THEN 0 ELSE 1 END), 0)
                 FROM agent_runs r LEFT JOIN run_sessions rs ON rs.run_id = r.id LEFT JOIN run_meta m ON m.run_id = r.id
                 WHERE COALESCE(rs.session_id, r.id) = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 1));
        total > 0 && foreign == 0
    }

    /// The runs of a session, oldest first, as the list of runs shows them.
    pub fn session_runs(&self, id: &str) -> R<Vec<Value>> {
        let c = self.conn();
        runs_of_session(&c, id)
    }

    /// The sessions, newest first. `q` is looked for in what the runs were asked and what they answered (not in what their tools returned); `directory` keeps the sessions of one project
    /// folder; `api_own_only` keeps only the sessions whose every run the API token asked for. At most `limit` (200); the second part is the cursor of the next page, when there is one.
    pub fn list_sessions(&self, q: &SessionQuery<'_>) -> R<(Vec<Value>, Option<String>)> {
        let limit = q.limit.clamp(1, 200) as usize;
        let matching: Option<HashSet<String>> = if fts_query(q.q).is_none() {
            None
        } else {
            self.index_runs()?;
            let ids = self.search_runs(q.q, 1_000)?;
            Some(self.sessions_of_runs(&ids)?)
        };
        let (cursor_at, cursor_id) = match decode_cursor(q.cursor) {
            Some((at, id)) => (Some(at), id),
            None => (None, String::new()),
        };
        let wanted_dir = q.directory.trim().to_lowercase();
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT COALESCE(rs.session_id, r.id) AS sid, MIN(r.started_at), MAX(r.started_at), COUNT(*),
                        MAX(CASE WHEN r.status IN ('running', 'stalled') THEN 1 ELSE 0 END), COALESCE(MAX(rs.directory), ''),
                        SUM(CASE WHEN COALESCE(m.origin, '') = 'api' THEN 0 ELSE 1 END), MAX(CASE WHEN r.status = 'imported' THEN 1 ELSE 0 END)
                 FROM agent_runs r LEFT JOIN run_sessions rs ON rs.run_id = r.id LEFT JOIN run_meta m ON m.run_id = r.id
                 GROUP BY COALESCE(rs.session_id, r.id)
                 HAVING (?1 = 0 OR SUM(CASE WHEN COALESCE(m.origin, '') = 'api' THEN 0 ELSE 1 END) = 0)
                    AND (?2 IS NULL OR MAX(r.started_at) < ?2 OR (MAX(r.started_at) = ?2 AND COALESCE(rs.session_id, r.id) < ?3))
                    AND (?4 = '' OR lower(COALESCE(MAX(rs.directory), '')) = ?4)
                 ORDER BY MAX(r.started_at) DESC, COALESCE(rs.session_id, r.id) DESC",
            )
            .map_err(db)?;
        let rows = st
            .query_map(params![q.api_own_only as i64, cursor_at, cursor_id, wanted_dir], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)? != 0, r.get::<_, String>(5)?, r.get::<_, i64>(7)? != 0))
            })
            .map_err(db)?;
        let mut kept: Vec<(String, i64, i64, i64, bool, String, bool)> = Vec::new();
        for row in rows.filter_map(Result::ok) {
            if matching.as_ref().is_some_and(|m| !m.contains(&row.0)) {
                continue;
            }
            kept.push(row);
            if kept.len() > limit {
                break;
            }
        }
        let next = if kept.len() > limit {
            kept.truncate(limit);
            kept.last().map(|(sid, _, last, ..)| encode_cursor(*last, sid))
        } else {
            None
        };
        let sessions = kept
            .into_iter()
            .map(|(sid, first, last, runs, going, directory, imported)| {
                json!({
                    "id": sid,
                    "title": title_in(&c, &sid),
                    "directory": directory,
                    "runs": runs,
                    "started_at": first,
                    "updated_at": last,
                    "going": going,
                    "imported": imported,
                    "forked_from": fork_in(&c, &sid).map(|(session, run, _)| json!({ "session_id": session, "run_id": run })),
                })
            })
            .collect();
        Ok((sessions, next))
    }

    /// One session: its title, its folder, its runs (as the list shows them) and what it was forked from. `None` when there is no such session.
    pub fn get_session(&self, id: &str) -> R<Option<Value>> {
        let c = self.conn();
        let runs = runs_of_session(&c, id)?;
        let fork = fork_in(&c, id);
        if runs.is_empty() && fork.is_none() {
            return Ok(None);
        }
        let directory: String = c.query_row("SELECT COALESCE(MAX(directory), '') FROM run_sessions WHERE session_id = ?1", [id], |r| r.get(0)).unwrap_or_default();
        let started = runs.first().and_then(|r| r["started_at"].as_i64());
        let updated = runs.last().and_then(|r| r["started_at"].as_i64());
        Ok(Some(json!({
            "id": id,
            "title": title_in(&c, id),
            "directory": directory,
            "runs": runs,
            "started_at": started,
            "updated_at": updated,
            "imported": runs.iter().any(|r| r["status"] == status::IMPORTED),
            "forked_from": fork.map(|(session, run, at)| json!({ "session_id": session, "run_id": run, "at": at })),
        })))
    }

    /// Gives a session a title (an empty one takes the title away: the first goal's sentence is the title again). `false` when there is no such session.
    pub fn rename_session(&self, id: &str, title: &str, now: i64) -> R<bool> {
        let c = self.conn();
        if !session_exists_in(&c, id) {
            return Ok(false);
        }
        if title.trim().is_empty() {
            c.execute("DELETE FROM session_titles WHERE session_id = ?1", [id]).map_err(db)?;
        } else {
            c.execute(
                "INSERT INTO session_titles (session_id, title, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(session_id) DO UPDATE SET title = excluded.title, updated_at = excluded.updated_at",
                params![id, title, now],
            )
            .map_err(db)?;
        }
        Ok(true)
    }

    /// Deletes a session for good: every run of it with its steps, events, questions, records and index entries, its title, its fork mark and the record of the coding session that has its
    /// id. The database overwrites what it deletes while this goes on (as the person's facts are forgotten), and the log of the database is written into the file at the end.
    pub fn delete_session(&self, id: &str) -> R<DeleteOutcome> {
        let c = self.conn();
        let ids: Vec<String> = {
            let mut st = c
                .prepare("SELECT r.id, r.status FROM agent_runs r LEFT JOIN run_sessions rs ON rs.run_id = r.id WHERE COALESCE(rs.session_id, r.id) = ?1")
                .map_err(db)?;
            let rows: Vec<(String, String)> = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(db)?.filter_map(Result::ok).collect();
            if rows.iter().any(|(_, state)| matches!(state.as_str(), status::RUNNING | status::STALLED)) {
                return Ok(DeleteOutcome::Going);
            }
            rows.into_iter().map(|(run, _)| run).collect()
        };
        let had_fork = fork_in(&c, id).is_some();
        let had_record: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM coding_sessions WHERE id = ?1)", [id], |r| r.get(0)).unwrap_or(false);
        if ids.is_empty() && !had_fork && !had_record {
            return Ok(DeleteOutcome::NotFound);
        }
        let was: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap_or(0);
        let _ = c.execute_batch("PRAGMA secure_delete = ON;");
        let done = (|| -> R<()> {
            for run in &ids {
                delete_run_rows(&c, run)?;
            }
            c.execute("DELETE FROM session_titles WHERE session_id = ?1", [id]).map_err(db)?;
            c.execute("DELETE FROM session_forks WHERE session_id = ?1", [id]).map_err(db)?;
            c.execute("DELETE FROM coding_sessions WHERE id = ?1", [id]).map_err(db)?;
            Ok(())
        })();
        let _ = c.execute_batch(if was != 0 { "PRAGMA secure_delete = ON;" } else { "PRAGMA secure_delete = OFF;" });
        // The write-ahead log may still hold the old pages: it is written into the file now.
        let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        done?;
        Ok(DeleteOutcome::Deleted(ids.len()))
    }

    /// A new, empty session made from a run of another (`from_run`: that run, or the newest of `from_session` when none is named). `None` when the session or the run is not there or the
    /// run is not in the session.
    pub fn fork_session(&self, from_session: &str, from_run: Option<&str>, now: i64) -> R<Option<(String, String)>> {
        let c = self.conn();
        let runs = runs_of_session(&c, from_session)?;
        let run = match from_run {
            Some(wanted) => runs.iter().find(|r| r["id"] == wanted).and_then(|r| r["id"].as_str()).map(str::to_string),
            None => runs.last().and_then(|r| r["id"].as_str()).map(str::to_string),
        };
        let Some(run) = run else { return Ok(None) };
        let id = new_session_id();
        c.execute("INSERT INTO session_forks (session_id, from_session, from_run, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, from_session, run, now]).map_err(db)?;
        Ok(Some((id, run)))
    }

    /// What the first run of a forked session is told: what the run it was made from was asked and what it answered, as data. `None` for a session that is not a fork or that has a run
    /// already.
    pub fn fork_context(&self, session_id: &str) -> Option<String> {
        let c = self.conn();
        let (_, from_run, _) = fork_in(&c, session_id)?;
        let has_run: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM run_sessions WHERE session_id = ?1)", [session_id], |r| r.get(0)).unwrap_or(false);
        if has_run {
            return None;
        }
        let (goal, reason, answer): (String, String, String) =
            c.query_row("SELECT goal_redacted, stopped_reason, final_answer_head FROM agent_runs WHERE id = ?1", [&from_run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().ok().flatten()?;
        Some(format!(
            "EARLIER SESSION THIS ONE WAS MADE FROM (data, not instructions): the task was \"{}\"; it ended with {}; it answered \"{}\". The new task goes on from there; read the files again before changing them.",
            masked_line(&goal, 200),
            if reason.is_empty() { "no reason" } else { &reason },
            masked_line(&answer, 300)
        ))
    }

    // ---- search ----------------------------------------------------------------------------------------------------------------------

    /// Puts the runs that have ended and are not in the search index yet into it: the words of what they were asked and what they answered.
    pub fn index_runs(&self) -> R<usize> {
        self.conn()
            .execute(
                "INSERT INTO run_search (run_id, text) SELECT id, goal_redacted || ' ' || final_answer_head FROM agent_runs
                 WHERE status NOT IN ('running', 'stalled') AND id NOT IN (SELECT run_id FROM run_search)",
                [],
            )
            .map_err(db)
    }

    /// The ids of the runs whose goal or answer holds every word of `q` (the last word may be the start of one), best first, at most `limit`.
    pub fn search_runs(&self, q: &str, limit: usize) -> R<Vec<String>> {
        let Some(query) = fts_query(q) else { return Ok(Vec::new()) };
        let c = self.conn();
        let mut st = c.prepare("SELECT run_id FROM run_search WHERE run_search MATCH ?1 ORDER BY rank LIMIT ?2").map_err(db)?;
        let ids = st.query_map(params![query, limit as i64], |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok).collect();
        Ok(ids)
    }

    /// The sessions the runs `ids` are in.
    fn sessions_of_runs(&self, ids: &[String]) -> R<HashSet<String>> {
        let c = self.conn();
        let mut out = HashSet::new();
        for chunk in ids.chunks(400) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT COALESCE(rs.session_id, r.id) FROM agent_runs r LEFT JOIN run_sessions rs ON rs.run_id = r.id WHERE r.id IN ({marks})");
            let mut st = c.prepare(&sql).map_err(db)?;
            out.extend(st.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0)).map_err(db)?.filter_map(Result::ok));
        }
        Ok(out)
    }

    // ---- what a run goes on from -------------------------------------------------------------------------------------------------------

    /// Keeps the checklist of the run (`(text, status)` items; JSON of at most 2 KB, the later items are left out when it is longer). An empty list clears it.
    pub fn set_checklist(&self, run_id: &str, items: &[(String, String)]) -> R<()> {
        let mut kept: Vec<Value> = Vec::new();
        for (text, state) in items {
            kept.push(json!({ "text": masked_line(text, 80), "status": state }));
            if serde_json::to_string(&kept).map_or(0, |s| s.len()) > CHECKLIST_BYTES {
                kept.pop();
                break;
            }
        }
        let text = if kept.is_empty() { String::new() } else { serde_json::to_string(&kept).unwrap_or_default() };
        self.conn()
            .execute("INSERT INTO run_state (run_id, checklist) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET checklist = excluded.checklist", params![run_id, text])
            .map_err(db)?;
        Ok(())
    }

    /// The checklist the run last kept: `(text, status)`, in order. Empty when it has none.
    pub fn checklist_of(&self, run_id: &str) -> Vec<(String, String)> {
        let text: Option<String> = self.conn().query_row("SELECT checklist FROM run_state WHERE run_id = ?1", [run_id], |r| r.get(0)).optional().ok().flatten();
        checklist_from(text.as_deref().unwrap_or(""))
    }

    /// Keeps the lines the person added to the run while it went (at most five, 500 characters each, masked).
    pub fn set_steered(&self, run_id: &str, lines: &[String]) -> R<()> {
        let kept: Vec<String> = lines.iter().map(|l| masked_line(l, STEERED_CHARS)).filter(|l| !l.is_empty()).take(STEERED_LINES).collect();
        let text = if kept.is_empty() { String::new() } else { serde_json::to_string(&kept).unwrap_or_default() };
        self.conn().execute("INSERT INTO run_state (run_id, steered) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET steered = excluded.steered", params![run_id, text]).map_err(db)?;
        Ok(())
    }

    /// The lines the person added to the run, in the order they came.
    pub fn steered_of(&self, run_id: &str) -> Vec<String> {
        let text: Option<String> = self.conn().query_row("SELECT steered FROM run_state WHERE run_id = ?1", [run_id], |r| r.get(0)).optional().ok().flatten();
        steered_from(text.as_deref().unwrap_or(""))
    }

    /// The plan a plan run wrote is ready for the person to approve (the plan as text: at most 1,500 characters, masked, the lines kept).
    pub fn set_plan_ready(&self, run_id: &str, plan: &str, now: i64) -> R<()> {
        let masked = aqua_tool_host::output::Redactor::new().redact(plan);
        let text: String = masked.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").chars().take(PLAN_CHARS).collect();
        self.conn()
            .execute(
                "INSERT INTO run_state (run_id, plan_state, plan_text, plan_at) VALUES (?1, 'ready', ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET plan_state = 'ready', plan_text = excluded.plan_text, plan_at = excluded.plan_at, approved_at = NULL, approved_by = ''",
                params![run_id, text, now],
            )
            .map_err(db)?;
        Ok(())
    }

    /// What became of the plan of a run: `None` for a run that wrote none.
    pub fn plan_state_of(&self, run_id: &str) -> Option<PlanState> {
        let c = self.conn();
        plan_in(&c, run_id)
    }

    /// Moves the plan of a run from one state to another (`ready` to `approved` or `revised`), once: `false` when it was not in `from` (a second approval, an expired or missing plan).
    pub fn move_plan(&self, run_id: &str, from: &str, to: &str, now: i64, by: &str) -> R<bool> {
        let n = self
            .conn()
            .execute("UPDATE run_state SET plan_state = ?3, approved_at = ?4, approved_by = ?5 WHERE run_id = ?1 AND plan_state = ?2", params![run_id, from, to, now, by])
            .map_err(db)?;
        Ok(n == 1)
    }

    /// The runs that ended with a plan the person has not answered, newest first: what the app offers to approve. Plans older than a day are not among them.
    pub fn plans_waiting(&self, now: i64, limit: u32) -> R<Vec<Value>> {
        let c = self.conn();
        let mut st = c
            .prepare(
                "SELECT s.run_id, s.plan_text, s.plan_at, r.goal_redacted FROM run_state s JOIN agent_runs r ON r.id = s.run_id
                 WHERE s.plan_state = 'ready' AND s.plan_at > ?1 ORDER BY s.plan_at DESC LIMIT ?2",
            )
            .map_err(db)?;
        let rows = st
            .query_map(params![now - PLAN_EXPIRES_MS, limit.min(100)], |r| {
                Ok(json!({ "run_id": r.get::<_, String>(0)?, "plan": r.get::<_, String>(1)?, "at": r.get::<_, i64>(2)?, "goal": r.get::<_, String>(3)? }))
            })
            .map_err(db)?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }
}

/// What `list_sessions` is asked.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionQuery<'a> {
    pub q: &'a str,
    pub directory: &'a str,
    pub limit: u32,
    pub cursor: Option<&'a str>,
    pub api_own_only: bool,
}

/// What deleting a session came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    NotFound,
    /// A run of it is still going: it has to end first.
    Going,
    /// How many runs went with it.
    Deleted(usize),
}

/// What became of the plan of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanState {
    /// `ready` (waits for the person), `approved` or `revised`.
    pub state: String,
    pub text: String,
    pub at: i64,
    pub approved_at: Option<i64>,
    pub approved_by: String,
}

impl PlanState {
    /// Has a plan that waits for the person gone stale (a day old)?
    pub fn expired(&self, now: i64) -> bool {
        self.state == "ready" && now - self.at > PLAN_EXPIRES_MS
    }

    pub fn view(&self, now: i64) -> Value {
        json!({
            "state": if self.expired(now) { "expired" } else { self.state.as_str() },
            "plan": self.text,
            "at": self.at,
            "expires_at": self.at + PLAN_EXPIRES_MS,
            "approved_at": self.approved_at,
            "approved_by": self.approved_by,
        })
    }
}

// ---- shared reads ----------------------------------------------------------------------------------------------------------------------

fn session_exists_in(c: &Connection, id: &str) -> bool {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_runs r LEFT JOIN run_sessions rs ON rs.run_id = r.id WHERE COALESCE(rs.session_id, r.id) = ?1) OR EXISTS(SELECT 1 FROM session_forks WHERE session_id = ?1)",
        [id],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

fn runs_of_session(c: &Connection, id: &str) -> R<Vec<Value>> {
    let mut st = c
        .prepare(
            "SELECT agent_runs.id, started_at, ended_at, goal_redacted, caller, route, status, model, rounds, stopped_reason, final_answer_head, parent_run_id,
                    (SELECT COUNT(*) FROM agent_steps s WHERE s.run_id = agent_runs.id AND s.kind = 'tool'), stopped_by, profile
             FROM agent_runs LEFT JOIN run_sessions rs ON rs.run_id = agent_runs.id
             WHERE COALESCE(rs.session_id, agent_runs.id) = ?1 ORDER BY started_at, agent_runs.id",
        )
        .map_err(db)?;
    let rows = st.query_map([id], |r| Ok(run_json(r, true))).map_err(db)?.filter_map(Result::ok).collect();
    Ok(rows)
}

/// The title of a session: the one it was given, else the sentence its first goal starts with.
fn title_in(c: &Connection, id: &str) -> String {
    if let Some(given) = c.query_row("SELECT title FROM session_titles WHERE session_id = ?1", [id], |r| r.get::<_, String>(0)).optional().ok().flatten() {
        return given;
    }
    let goal: Option<String> = c
        .query_row(
            "SELECT agent_runs.goal_redacted FROM agent_runs LEFT JOIN run_sessions rs ON rs.run_id = agent_runs.id
             WHERE COALESCE(rs.session_id, agent_runs.id) = ?1 ORDER BY started_at, agent_runs.id LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    match goal {
        Some(g) => title_of(&g),
        // A fork that has no run yet is named for what it was made from.
        None => fork_in(c, id).map(|(from, _, _)| format!("Fork of {}", title_in(c, &from))).unwrap_or_default(),
    }
}

/// (the session it was made from, the run, when), for a session that is a fork.
fn fork_in(c: &Connection, id: &str) -> Option<(String, String, i64)> {
    c.query_row("SELECT from_session, from_run, created_at FROM session_forks WHERE session_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().ok().flatten()
}

fn plan_in(c: &Connection, run_id: &str) -> Option<PlanState> {
    c.query_row("SELECT plan_state, plan_text, plan_at, approved_at, approved_by FROM run_state WHERE run_id = ?1 AND plan_state != ''", [run_id], |r| {
        Ok(PlanState { state: r.get(0)?, text: r.get(1)?, at: r.get::<_, Option<i64>>(2)?.unwrap_or(0), approved_at: r.get(3)?, approved_by: r.get(4)? })
    })
    .optional()
    .ok()
    .flatten()
}

fn checklist_from(text: &str) -> Vec<(String, String)> {
    serde_json::from_str::<Vec<Value>>(text)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| Some((v["text"].as_str()?.to_string(), v["status"].as_str().unwrap_or("pending").to_string())))
        .collect()
}

fn steered_from(text: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(text).unwrap_or_default()
}

/// The rows that belong to one run in the tables of migration 0011 (the run itself and the older tables are [`delete_run_rows`]'s).
pub(super) fn forget_run_rows(c: &Connection, id: &str) -> R<()> {
    c.execute("DELETE FROM run_sessions WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM run_state WHERE run_id = ?1", [id]).map_err(db)?;
    c.execute("DELETE FROM run_search WHERE run_id = ?1", [id]).map_err(db)?;
    Ok(())
}

/// What a session or title or fork mark has left when the runs it was about are gone (pruned by age): removed, so that nothing of a forgotten run stays. A fork that never had a run is
/// kept for as long as runs are (30 days).
pub(super) fn forget_orphans(c: &Connection, now: i64, max_age_ms: i64) -> R<()> {
    c.execute("DELETE FROM session_titles WHERE session_id NOT IN (SELECT session_id FROM run_sessions) AND session_id NOT IN (SELECT id FROM agent_runs)", []).map_err(db)?;
    c.execute("DELETE FROM session_forks WHERE session_id NOT IN (SELECT session_id FROM run_sessions) AND created_at < ?1", [now - max_age_ms]).map_err(db)?;
    Ok(())
}

/// Adds to the run JSON what the sessions and the state tables know of it (see `AgentRunStore::get`).
pub(super) fn decorate(c: &Connection, id: &str, run: &mut Value) {
    let session: Option<String> = c.query_row("SELECT session_id FROM run_sessions WHERE run_id = ?1", [id], |r| r.get(0)).optional().ok().flatten();
    run["session_id"] = json!(session.unwrap_or_else(|| id.to_string()));
    let state: Option<(String, String)> = c.query_row("SELECT checklist, steered FROM run_state WHERE run_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten();
    let (checklist, steered) = state.unwrap_or_default();
    run["checklist"] = Value::Array(checklist_from(&checklist).into_iter().map(|(text, state)| json!({ "text": text, "status": state })).collect());
    run["steered"] = json!(steered_from(&steered));
    run["plan_approval"] = plan_in(c, id).map(|p| p.view(super::now_ms())).unwrap_or(Value::Null);
    let goes_on: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM agent_runs WHERE parent_run_id = ?1)", [id], |r| r.get(0)).unwrap_or(false);
    run["continuable"] = json!(!goes_on && super::continuation::continuable(run["status"].as_str().unwrap_or(""), run["stopped_reason"].as_str().unwrap_or("")));
}

// ---- search words and cursors ----------------------------------------------------------------------------------------------------------

/// The query of the full-text index for what a person typed: every word (letters and digits only, lower case, at most eight) must be there, and the last may be the start of one. `None`
/// when nothing searchable was typed. Nothing the person types can be a part of the index's own syntax.
pub fn fts_query(q: &str) -> Option<String> {
    let words: Vec<String> = q.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).take(8).collect();
    let last = words.len().checked_sub(1)?;
    Some(words.iter().enumerate().map(|(i, w)| if i == last { format!("{w}*") } else { w.clone() }).collect::<Vec<_>>().join(" "))
}

fn encode_cursor(at: i64, id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{at}|{id}"))
}

fn decode_cursor(cursor: Option<&str>) -> Option<(i64, String)> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor?.trim()).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (at, id) = text.split_once('|')?;
    Some((at.parse().ok()?, id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::super::store::{NewRun, RunEnd};
    use super::*;

    fn store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_agent_sessions_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    /// A run that ended, asked `goal`, answered `answer`, in session `session` (none: its own).
    fn ended(s: &AgentRunStore, id: &str, at: i64, goal: &str, answer: &str, session: Option<&str>) {
        s.create_run(&NewRun { id: id.into(), started_at: at, goal_redacted: goal.into(), goal_full: goal.into(), caller: "api".into(), route: "agent".into(), parent_run_id: None }).unwrap();
        if let Some(session) = session {
            s.set_run_session(id, session, "").unwrap();
        }
        s.end_run(id, at + 5, &RunEnd { status: status::COMPLETED.into(), stopped_reason: "final".into(), final_answer_head: answer.into(), ..Default::default() }).unwrap();
    }

    #[test]
    fn a_title_is_the_first_sentence_of_the_first_goal_and_a_file_name_does_not_end_it() {
        assert_eq!(title_of("Fix the failing test in src/lib.rs. Then run cargo test."), "Fix the failing test in src/lib.rs");
        assert_eq!(title_of("  add   a unit test\nfor the parser?  and more"), "add a unit test for the parser");
        assert_eq!(title_of("Work in the folder C:\\proj. Rename the struct and update the callers"), "Rename the struct and update the callers");
        let long = title_of(&"word ".repeat(40));
        assert!(long.chars().count() <= TITLE_CHARS && long.ends_with('…'), "{long}");
        assert_eq!(title_of(""), "");
        assert!(valid_session_id("ses_1.a:b-c") && !valid_session_id("") && !valid_session_id("a b") && !valid_session_id(&"x".repeat(65)) && !valid_session_id("../x"));
    }

    #[test]
    fn a_run_without_a_row_is_its_own_session_and_a_run_that_joins_one_is_listed_with_it() {
        let s = store("group");
        ended(&s, "run_old", 100, "Tidy the notes folder", "Done.", None);
        ended(&s, "run_a", 200, "Add a test for parse. Then more", "Added one.", Some("ses_x"));
        ended(&s, "run_b", 300, "Now run the whole suite", "All pass.", Some("ses_x"));
        let (list, next) = s.list_sessions(&SessionQuery { limit: 10, ..Default::default() }).unwrap();
        assert!(next.is_none());
        let ids: Vec<&str> = list.iter().map(|v| v["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["ses_x", "run_old"], "newest first, and a run from before is its own session");
        assert_eq!((list[0]["runs"].as_i64(), list[0]["title"].as_str()), (Some(2), Some("Add a test for parse")), "the title is the first goal's sentence");
        assert_eq!(s.session_id_of("run_old"), "run_old");
        assert_eq!(s.session_id_of("run_b"), "ses_x");
        let one = s.get_session("ses_x").unwrap().unwrap();
        assert_eq!(one["runs"].as_array().unwrap().len(), 2);
        assert!(s.get_session("nobody").unwrap().is_none());
        // A title the person gave replaces the made one, and an empty one gives it back.
        assert!(s.rename_session("ses_x", "Parser tests", 1).unwrap());
        assert_eq!(s.get_session("ses_x").unwrap().unwrap()["title"], "Parser tests");
        assert!(s.rename_session("ses_x", "  ", 2).unwrap());
        assert_eq!(s.get_session("ses_x").unwrap().unwrap()["title"], "Add a test for parse");
        assert!(!s.rename_session("nobody", "x", 3).unwrap());
    }

    #[test]
    fn the_list_is_paged_by_a_cursor_that_does_not_skip_or_repeat() {
        let s = store("pages");
        for i in 0..5 {
            ended(&s, &format!("run_{i}"), 100 + i, &format!("task number {i}"), "ok", None);
        }
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..5 {
            let (page, next) = s.list_sessions(&SessionQuery { limit: 2, cursor: cursor.as_deref(), ..Default::default() }).unwrap();
            seen.extend(page.iter().map(|v| v["id"].as_str().unwrap().to_string()));
            match next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        assert_eq!(seen, vec!["run_4", "run_3", "run_2", "run_1", "run_0"]);
        assert!(decode_cursor(Some("not a cursor!")).is_none(), "a cursor that is not one starts from the top");
    }

    #[test]
    fn search_finds_the_words_of_a_goal_or_an_answer_and_never_what_a_tool_returned() {
        let s = store("search");
        ended(&s, "run_1", 100, "Fix the quoting bug in the csv parser", "I changed parse_line.", Some("ses_csv"));
        ended(&s, "run_2", 200, "Write the changelog", "Wrote CHANGELOG.md.", None);
        // A tool result that holds a word nobody typed in a goal or an answer: the index does not have it.
        let seq = s.add_step("run_1", &super::super::store::NewStep { step: 1, kind: "tool".into(), tool: "fs.read".into(), ts: 1, ..Default::default() }).unwrap();
        s.finish_step("run_1", seq, &super::super::store::StepOutcome { ok: true, status: "completed".into(), observation_head: "zeppelin zeppelin".into(), ..Default::default() }).unwrap();
        let find = |q: &str| -> Vec<String> { s.list_sessions(&SessionQuery { q, limit: 10, ..Default::default() }).unwrap().0.iter().map(|v| v["id"].as_str().unwrap().to_string()).collect() };
        assert_eq!(find("quoting csv"), vec!["ses_csv"], "every word must be there");
        assert_eq!(find("parse_li"), vec!["ses_csv"], "the last word may be the start of one, and the answer counts");
        assert_eq!(find("changelog"), vec!["run_2"]);
        assert!(find("zeppelin").is_empty(), "what a tool returned is not searched");
        assert!(find("AND OR NOT \"(").len() <= 1, "what is typed is never the index's own syntax");
        assert_eq!(fts_query("  "), None);
        assert_eq!(fts_query("Fix: the BUG-2 now").as_deref(), Some("fix the bug 2 now*"));
    }

    #[test]
    fn deleting_a_session_takes_every_row_of_its_runs_and_its_search_hits_and_refuses_one_that_is_going() {
        let s = store("delete");
        ended(&s, "run_1", 100, "Fix the zebra bug", "fixed zebra", Some("ses_z"));
        ended(&s, "run_2", 200, "Check the zebra fix", "checked", Some("ses_z"));
        ended(&s, "run_keep", 300, "Something else", "ok", None);
        s.set_checklist("run_1", &[("read it".into(), "done".into())]).unwrap();
        s.set_coding_record("run_1", "{\"next\":\"x\"}", 1).unwrap();
        s.rename_session("ses_z", "Zebra", 1).unwrap();
        let hits = |q: &str| s.search_runs(q, 10).unwrap_or_default().len();
        s.index_runs().unwrap();
        assert_eq!(hits("zebra"), 2);
        // A run that is going holds its session.
        s.create_run(&NewRun { id: "run_live".into(), started_at: 400, ..Default::default() }).unwrap();
        s.set_run_session("run_live", "ses_z", "").unwrap();
        assert_eq!(s.delete_session("ses_z").unwrap(), DeleteOutcome::Going);
        s.end_run("run_live", 410, &RunEnd { status: status::CANCELLED.into(), ..Default::default() }).unwrap();
        assert_eq!(s.delete_session("ses_z").unwrap(), DeleteOutcome::Deleted(3));
        assert!(s.get("run_1").unwrap().is_none() && s.get_session("ses_z").unwrap().is_none());
        assert_eq!(hits("zebra"), 0, "the search hits went with the runs");
        for table in ["agent_steps", "run_events", "run_sessions", "run_state", "coding_records", "session_titles"] {
            let key = if table == "session_titles" { "session_id" } else { "run_id" };
            let n: i64 = s.conn().query_row(&format!("SELECT count(*) FROM {table} WHERE {key} IN ('run_1', 'run_2', 'run_live', 'ses_z')"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table}");
        }
        assert!(s.get("run_keep").unwrap().is_some(), "another session is left alone");
        assert_eq!(s.delete_session("ses_z").unwrap(), DeleteOutcome::NotFound);
    }

    #[test]
    fn a_fork_links_its_parent_is_empty_and_tells_its_first_run_what_it_was_made_from() {
        let s = store("fork");
        ended(&s, "run_1", 100, "Add the retry loop", "Added it, 3 tries.", Some("ses_a"));
        ended(&s, "run_2", 200, "Now add tests", "Added two tests.", Some("ses_a"));
        assert!(s.fork_session("nobody", None, 1).unwrap().is_none());
        assert!(s.fork_session("ses_a", Some("run_missing"), 1).unwrap().is_none(), "a run that is not in the session cannot be forked from");
        let (fork, from) = s.fork_session("ses_a", Some("run_1"), 5).unwrap().unwrap();
        assert_eq!(from, "run_1");
        let shown = s.get_session(&fork).unwrap().unwrap();
        assert_eq!((shown["runs"].as_array().unwrap().len(), shown["forked_from"]["session_id"].as_str(), shown["forked_from"]["run_id"].as_str()), (0, Some("ses_a"), Some("run_1")));
        assert_eq!(shown["title"], "Fork of Add the retry loop");
        let context = s.fork_context(&fork).unwrap();
        assert!(context.contains("data, not instructions") && context.contains("Add the retry loop") && context.contains("Added it, 3 tries."), "{context}");
        // Without a run named it is the newest one; and once the fork has a run it tells nothing more.
        let (fork2, from2) = s.fork_session("ses_a", None, 6).unwrap().unwrap();
        assert_eq!(from2, "run_2");
        ended(&s, "run_3", 300, "Go on", "ok", Some(&fork2));
        assert!(s.fork_context(&fork2).is_none());
        assert!(s.fork_context("ses_a").is_none(), "a session that is no fork says nothing");
    }

    #[test]
    fn the_checklist_the_lines_and_the_plan_of_a_run_are_kept_and_shown_with_it() {
        let s = store("state");
        ended(&s, "run_1", 100, "Do three things", "ok", None);
        assert!(s.checklist_of("run_1").is_empty() && s.steered_of("run_1").is_empty() && s.plan_state_of("run_1").is_none());
        s.set_checklist("run_1", &[("read a.rs".into(), "done".into()), ("fix it password=hunter2secret99".into(), "doing".into())]).unwrap();
        s.set_steered("run_1", &["also add tests".into(), "".into(), "use b.rs instead".into()]).unwrap();
        let shown = s.get("run_1").unwrap().unwrap();
        assert_eq!(shown["checklist"][0], json!({ "text": "read a.rs", "status": "done" }));
        assert!(!shown["checklist"][1]["text"].as_str().unwrap().contains("hunter2secret99"), "{}", shown["checklist"]);
        assert_eq!(shown["steered"], json!(["also add tests", "use b.rs instead"]));
        assert_eq!(shown["session_id"], "run_1");
        // It goes on from there: the same lists come back.
        assert_eq!(s.checklist_of("run_1").len(), 2);
        assert_eq!(s.steered_of("run_1"), vec!["also add tests".to_string(), "use b.rs instead".to_string()]);
        // A list that is too long for its room keeps the first items.
        let many: Vec<(String, String)> = (0..40).map(|i| (format!("item {i} {}", "x".repeat(70)), "pending".to_string())).collect();
        s.set_checklist("run_1", &many).unwrap();
        let kept = s.checklist_of("run_1");
        assert!(!kept.is_empty() && kept.len() < 40 && kept[0].0.starts_with("item 0"), "{}", kept.len());
        s.set_checklist("run_1", &[]).unwrap();
        assert!(s.checklist_of("run_1").is_empty());
        // A plan: ready, approved once, and a second approval finds nothing to approve.
        s.set_plan_ready("run_1", "1) read a.rs\n2) fix it token=abcdef123456789012", 1_000).unwrap();
        let plan = s.plan_state_of("run_1").unwrap();
        assert_eq!((plan.state.as_str(), plan.expired(1_000 + PLAN_EXPIRES_MS), plan.expired(1_001 + PLAN_EXPIRES_MS)), ("ready", false, true));
        assert!(!plan.text.contains("abcdef123456789012"), "{}", plan.text);
        assert_eq!(s.plans_waiting(2_000, 10).unwrap().len(), 1);
        assert!(s.move_plan("run_1", "ready", "approved", 2_000, "ui").unwrap());
        assert!(!s.move_plan("run_1", "ready", "approved", 2_001, "ui").unwrap(), "a plan is approved once");
        let approved = s.plan_state_of("run_1").unwrap();
        assert_eq!((approved.state.as_str(), approved.approved_by.as_str(), approved.approved_at), ("approved", "ui", Some(2_000)));
        assert!(s.plans_waiting(2_000, 10).unwrap().is_empty());
        assert_eq!(s.get("run_1").unwrap().unwrap()["plan_approval"]["state"], "approved");
    }

    #[test]
    fn a_run_that_is_pruned_takes_its_session_rows_with_it() {
        let s = store("prune");
        ended(&s, "run_1", 100, "old task", "ok", Some("ses_old"));
        s.set_checklist("run_1", &[("a".into(), "done".into())]).unwrap();
        s.index_runs().unwrap();
        s.rename_session("ses_old", "Old", 1).unwrap();
        let day = 24 * 3600 * 1000;
        assert_eq!(s.prune(100 * day, 30 * day, 100).unwrap(), 1);
        for (table, key) in [("run_sessions", "run_id"), ("run_state", "run_id"), ("run_search", "run_id"), ("session_titles", "session_id")] {
            let n: i64 = s.conn().query_row(&format!("SELECT count(*) FROM {table} WHERE {key} IN ('run_1', 'ses_old')"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }
}
