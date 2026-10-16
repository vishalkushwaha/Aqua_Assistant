//! Where the record of a coding session is kept (`coding_sessions` of `data/agent_runs.db`; card OA10-04, task CX-08 of the OpenCode audit; the record itself is
//! `modules::coding_session_record`).
//!
//! One row a session: the project it is about (its folder, lower-cased: [`project_key`](crate::modules::coding_session_record::project_key)), when it began and when it was last written,
//! whether it is `open` or `closed`, and the record as JSON. The rules of the lifetime, which are the owner's decision D-14 as the proposal wrote it:
//!
//! * **a session found by its project ends after four hours without a run** ([`IDLE_PROJECT_MS`]): a goal in the same project after that opens a new one (the old one stays for listing
//!   until it is thirty days old). A session the client **named** (`session_id`) is the client's to end, and its record is used for twenty-four hours after its last run ([`IDLE_NAMED_MS`]);
//! * **thirty days and fifty sessions** ([`RETENTION_MS`], [`MAX_SESSIONS`]): older or beyond the newest fifty are deleted, at start-up and whenever a session is opened;
//! * **forgetting is real**: a deleted record is overwritten on disk (`PRAGMA secure_delete`, then the log of the database is written into the file), as the person's facts are;
//! * **closed, not deleted, by a new topic**: "new topic", "sleep" and the other things that clear the conversation close the open sessions (the record stays for the person to list or
//!   delete); the privacy route that clears the conversation also deletes them.
//!
//! A write reads the row, lets the caller change the record, and writes it back while the connection is held, so two runs of one project that end together join their changes (the
//! caller's function does the join) instead of one overwriting the other.

use super::store::{AgentRunStore, R};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn db(e: rusqlite::Error) -> String {
    format!("agent run store: {e}")
}

/// A session found by its project ends after this long without a run: four hours.
pub const IDLE_PROJECT_MS: i64 = 4 * 3600 * 1000;
/// A session the client named is used for this long after its last run: twenty-four hours.
pub const IDLE_NAMED_MS: i64 = 24 * 3600 * 1000;
/// Records older than this are deleted: thirty days.
pub const RETENTION_MS: i64 = 30 * 24 * 3600 * 1000;
/// The most sessions kept.
pub const MAX_SESSIONS: i64 = 50;

/// A row of `coding_sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub project_key: String,
    pub started_at: i64,
    pub updated_at: i64,
    /// `open` or `closed`.
    pub status: String,
    /// The record as JSON (`modules::coding_session_record::SessionRecord`).
    pub record: String,
    /// A summary of it was stored as experience (card OA10-04, CX-19): at most once.
    pub summarised: bool,
}

impl SessionRow {
    pub fn is_open(&self) -> bool {
        self.status == "open"
    }

    /// Is the record to be used by a run at `now`: the session is open and was written within `idle_ms`.
    pub fn usable(&self, now: i64, idle_ms: i64) -> bool {
        self.is_open() && now - self.updated_at <= idle_ms
    }

    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "project": self.project_key,
            "status": self.status,
            "started_at": self.started_at,
            "updated_at": self.updated_at,
            "record": serde_json::from_str::<Value>(&self.record).unwrap_or(Value::Null),
        })
    }
}

const COLUMNS: &str = "id, project_key, started_at, updated_at, status, record, summarised";

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow { id: r.get(0)?, project_key: r.get(1)?, started_at: r.get(2)?, updated_at: r.get(3)?, status: r.get(4)?, record: r.get(5)?, summarised: r.get::<_, i64>(6)? != 0 })
}

fn get_in(c: &Connection, id: &str) -> Option<SessionRow> {
    c.query_row(&format!("SELECT {COLUMNS} FROM coding_sessions WHERE id = ?1"), [id], row_of).optional().ok().flatten()
}

impl AgentRunStore {
    /// One session, as written; `None` when there is none.
    pub fn coding_session(&self, id: &str) -> Option<SessionRow> {
        get_in(&self.conn(), id)
    }

    /// The open session of a project that was written within `idle_ms`, or a new one with the id `new_id`. Sessions of the project that are open and have gone idle are closed here, so
    /// that there is at most one open session per project. Old sessions are pruned.
    pub fn open_project_session(&self, project_key: &str, new_id: &str, now: i64, idle_ms: i64) -> R<SessionRow> {
        let c = self.conn();
        prune_in(&c, now)?;
        c.execute("UPDATE coding_sessions SET status = 'closed' WHERE project_key = ?1 AND status = 'open' AND updated_at < ?2", params![project_key, now - idle_ms]).map_err(db)?;
        let found: Option<SessionRow> =
            c.query_row(&format!("SELECT {COLUMNS} FROM coding_sessions WHERE project_key = ?1 AND status = 'open' ORDER BY updated_at DESC LIMIT 1"), [project_key], row_of).optional().map_err(db)?;
        if let Some(row) = found {
            return Ok(row);
        }
        c.execute("INSERT INTO coding_sessions (id, project_key, started_at, updated_at, status, record) VALUES (?1, ?2, ?3, ?3, 'open', '{}')", params![new_id, project_key, now]).map_err(db)?;
        // (Fifty are kept, the one just made among them.)
        prune_in(&c, now)?;
        get_in(&c, new_id).ok_or_else(|| "agent run store: the session just made is not there".to_string())
    }

    /// The session a client named: the row of that id, made (open) when there is none and opened again when it was closed.
    pub fn named_session(&self, id: &str, project_key: &str, now: i64) -> R<SessionRow> {
        let c = self.conn();
        prune_in(&c, now)?;
        if get_in(&c, id).is_none() {
            c.execute("INSERT INTO coding_sessions (id, project_key, started_at, updated_at, status, record) VALUES (?1, ?2, ?3, ?3, 'open', '{}')", params![id, project_key, now]).map_err(db)?;
        } else {
            c.execute("UPDATE coding_sessions SET status = 'open' WHERE id = ?1", [id]).map_err(db)?;
        }
        prune_in(&c, now)?;
        get_in(&c, id).ok_or_else(|| "agent run store: the session just made is not there".to_string())
    }

    /// Changes the record of a session: `change` is given the record as it is now (JSON) and gives the record to write. Written while the connection is held, so changes of two runs do
    /// not overwrite each other when `change` joins what it is given with what it has. `false` when there is no such session.
    pub fn update_coding_session(&self, id: &str, now: i64, change: &mut dyn FnMut(&str) -> String) -> R<bool> {
        let c = self.conn();
        let Some(row) = get_in(&c, id) else { return Ok(false) };
        let record = change(&row.record);
        let n = c.execute("UPDATE coding_sessions SET record = ?2, updated_at = ?3 WHERE id = ?1", params![id, record, now]).map_err(db)?;
        Ok(n == 1)
    }

    /// Closes a session: its record stays (to list, to delete), and no run is told it.
    pub fn close_coding_session(&self, id: &str) -> R<bool> {
        Ok(self.conn().execute("UPDATE coding_sessions SET status = 'closed' WHERE id = ?1 AND status = 'open'", [id]).map_err(db)? == 1)
    }

    /// Closes every open session ("new topic", "sleep"). How many were open.
    pub fn close_open_coding_sessions(&self) -> R<usize> {
        self.conn().execute("UPDATE coding_sessions SET status = 'closed' WHERE status = 'open'", []).map_err(db)
    }

    /// Deletes a session's record for good (see the module). `false` when there is none.
    pub fn forget_coding_session(&self, id: &str) -> R<bool> {
        let c = self.conn();
        let n = with_secure_delete(&c, |c| c.execute("DELETE FROM coding_sessions WHERE id = ?1", [id]).map_err(db))?;
        Ok(n == 1)
    }

    /// Deletes the records of the open sessions for good (the person cleared what Aqua remembers). How many went.
    pub fn forget_open_coding_sessions(&self) -> R<usize> {
        let c = self.conn();
        with_secure_delete(&c, |c| c.execute("DELETE FROM coding_sessions WHERE status = 'open'", []).map_err(db))
    }

    /// The sessions, newest first, of one project when `project_key` is given.
    pub fn list_coding_sessions(&self, project_key: Option<&str>, limit: u32) -> R<Vec<SessionRow>> {
        let c = self.conn();
        let mut st = c
            .prepare(&format!("SELECT {COLUMNS} FROM coding_sessions WHERE (?1 IS NULL OR project_key = ?1) ORDER BY updated_at DESC, id DESC LIMIT ?2"))
            .map_err(db)?;
        let rows = st.query_map(params![project_key, limit.clamp(1, 200)], row_of).map_err(db)?.filter_map(Result::ok).collect();
        Ok(rows)
    }

    /// Deletes what is older than thirty days or beyond the newest fifty. How many.
    pub fn prune_coding_sessions(&self, now: i64) -> R<usize> {
        prune_in(&self.conn(), now)
    }

    /// The closed sessions that have not been summarised and have been idle for `idle_ms` (card OA10-04, CX-19): the ones whose summary may be written now, oldest first.
    pub fn sessions_to_summarise(&self, now: i64, idle_ms: i64, limit: u32) -> R<Vec<SessionRow>> {
        let c = self.conn();
        let mut st = c
            .prepare(&format!("SELECT {COLUMNS} FROM coding_sessions WHERE status = 'closed' AND summarised = 0 AND updated_at <= ?1 AND updated_at > ?2 ORDER BY updated_at LIMIT ?3"))
            .map_err(db)?;
        let rows = st.query_map(params![now - idle_ms, now - IDLE_NAMED_MS, limit.clamp(1, 20)], row_of).map_err(db)?.filter_map(Result::ok).collect();
        Ok(rows)
    }

    /// A session has its summary: it is not summarised again.
    pub fn mark_summarised(&self, id: &str) -> R<bool> {
        Ok(self.conn().execute("UPDATE coding_sessions SET summarised = 1 WHERE id = ?1 AND summarised = 0", [id]).map_err(db)? == 1)
    }
}

/// Runs `f` with the database set to overwrite what it deletes, and puts the setting back; then the log of the database is written into the file (a deleted record is not left in it).
fn with_secure_delete<T>(c: &Connection, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
    let was: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap_or(0);
    let _ = c.execute_batch("PRAGMA secure_delete = ON;");
    let done = f(c);
    let _ = c.execute_batch(if was != 0 { "PRAGMA secure_delete = ON;" } else { "PRAGMA secure_delete = OFF;" });
    let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    done
}

fn prune_in(c: &Connection, now: i64) -> R<usize> {
    with_secure_delete(c, |c| {
        let old = c.execute("DELETE FROM coding_sessions WHERE updated_at < ?1", [now - RETENTION_MS]).map_err(db)?;
        let extra = c
            .execute("DELETE FROM coding_sessions WHERE id IN (SELECT id FROM coding_sessions ORDER BY updated_at DESC, id DESC LIMIT -1 OFFSET ?1)", [MAX_SESSIONS])
            .map_err(db)?;
        Ok(old + extra)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_coding_sessions_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    const H: i64 = 3600 * 1000;
    const T0: i64 = 1_900_000_000_000;

    #[test]
    fn a_project_has_one_open_session_that_ends_after_four_idle_hours() {
        let s = store("idle");
        let first = s.open_project_session("c:/work/aqua", "ses_1", T0, IDLE_PROJECT_MS).unwrap();
        assert_eq!((first.id.as_str(), first.status.as_str(), first.record.as_str()), ("ses_1", "open", "{}"));
        // A second goal in the same project, an hour later, is in the same session; another project is not.
        s.update_coding_session("ses_1", T0 + H, &mut |_| r#"{"objective":"a"}"#.to_string()).unwrap();
        assert_eq!(s.open_project_session("c:/work/aqua", "ses_2", T0 + 2 * H, IDLE_PROJECT_MS).unwrap().id, "ses_1");
        assert_eq!(s.open_project_session("c:/work/other", "ses_3", T0 + 2 * H, IDLE_PROJECT_MS).unwrap().id, "ses_3");
        // Four hours after the last run it is over: the next goal opens a new one, and the old one stays (closed) to be listed.
        let next = s.open_project_session("c:/work/aqua", "ses_4", T0 + H + IDLE_PROJECT_MS + 1, IDLE_PROJECT_MS).unwrap();
        assert_eq!(next.id, "ses_4");
        let old = s.coding_session("ses_1").unwrap();
        assert!(!old.is_open() && old.record.contains("\"a\""), "{old:?}");
        assert!(!old.usable(T0 + H + 1, IDLE_PROJECT_MS), "a closed session is not told to a run");
        assert!(s.coding_session("ses_4").unwrap().usable(T0 + H + IDLE_PROJECT_MS + 2, IDLE_PROJECT_MS));
    }

    #[test]
    fn a_session_the_client_named_is_made_once_and_opened_again_after_a_close() {
        let s = store("named");
        let a = s.named_session("mytask-1", "c:/work/aqua", T0).unwrap();
        assert_eq!((a.id.as_str(), a.project_key.as_str()), ("mytask-1", "c:/work/aqua"));
        s.update_coding_session("mytask-1", T0 + 1, &mut |_| r#"{"objective":"x"}"#.to_string()).unwrap();
        assert!(s.close_coding_session("mytask-1").unwrap() && !s.close_coding_session("mytask-1").unwrap(), "closed once");
        let again = s.named_session("mytask-1", "c:/work/aqua", T0 + 2).unwrap();
        assert!(again.is_open() && again.record.contains("\"x\""), "the record is still there");
        // (It was last written at T0 + 1.)
        assert!(again.usable(T0 + 1 + IDLE_NAMED_MS, IDLE_NAMED_MS) && !again.usable(T0 + 2 + IDLE_NAMED_MS, IDLE_NAMED_MS), "twenty-four hours");
        assert!(!s.update_coding_session("nobody", T0, &mut |r| r.to_string()).unwrap());
    }

    #[test]
    fn a_change_is_made_from_the_record_as_it_is_now_so_two_writers_join() {
        let s = store("join");
        s.open_project_session("p", "ses_1", T0, IDLE_PROJECT_MS).unwrap();
        s.update_coding_session("ses_1", T0 + 1, &mut |now| format!("{now}|a")).unwrap();
        s.update_coding_session("ses_1", T0 + 2, &mut |now| format!("{now}|b")).unwrap();
        assert_eq!(s.coding_session("ses_1").unwrap().record, "{}|a|b");
        assert_eq!(s.coding_session("ses_1").unwrap().updated_at, T0 + 2);
    }

    #[test]
    fn retention_is_thirty_days_and_fifty_sessions_and_forgetting_is_for_good() {
        let s = store("retention");
        for i in 0..60 {
            s.open_project_session(&format!("p{i}"), &format!("ses_{i:02}"), T0 + i, IDLE_PROJECT_MS).unwrap();
        }
        // Opening one pruned: at most fifty are kept, the newest.
        assert_eq!(s.list_coding_sessions(None, 200).unwrap().len(), MAX_SESSIONS as usize);
        assert!(s.coding_session("ses_00").is_none() && s.coding_session("ses_59").is_some());
        // Thirty days after, everything that was not written since is gone.
        assert!(s.prune_coding_sessions(T0 + RETENTION_MS + 1_000).unwrap() >= 50);
        assert!(s.list_coding_sessions(None, 200).unwrap().is_empty());
        // Forgetting: the record is not in the file afterwards (not even in the log of the database).
        s.open_project_session("p", "ses_x", T0, IDLE_PROJECT_MS).unwrap();
        s.update_coding_session("ses_x", T0 + 1, &mut |_| r#"{"objective":"zanzibar-unique-text"}"#.to_string()).unwrap();
        assert!(s.forget_coding_session("ses_x").unwrap() && !s.forget_coding_session("ses_x").unwrap());
        let bytes: Vec<u8> = std::fs::read(s.path()).unwrap().into_iter().chain(std::fs::read(s.path().with_extension("db-wal")).unwrap_or_default()).collect();
        assert!(!String::from_utf8_lossy(&bytes).contains("zanzibar-unique-text"), "the record is overwritten");
        // The privacy route: the open ones go, the closed ones are the person's to list and delete.
        s.open_project_session("q", "ses_q", T0, IDLE_PROJECT_MS).unwrap();
        s.open_project_session("r", "ses_r", T0, IDLE_PROJECT_MS).unwrap();
        s.close_coding_session("ses_r").unwrap();
        assert_eq!(s.forget_open_coding_sessions().unwrap(), 1);
        assert!(s.coding_session("ses_q").is_none() && s.coding_session("ses_r").is_some());
    }

    #[test]
    fn new_topic_closes_every_open_session_and_a_closed_one_is_offered_for_a_summary_once() {
        let s = store("close");
        s.open_project_session("p", "ses_1", T0, IDLE_PROJECT_MS).unwrap();
        s.open_project_session("q", "ses_2", T0, IDLE_PROJECT_MS).unwrap();
        assert_eq!(s.close_open_coding_sessions().unwrap(), 2);
        assert_eq!(s.close_open_coding_sessions().unwrap(), 0);
        // Idle for ten minutes and not yet summarised: offered; too young, or too old, or done already: not.
        assert_eq!(s.sessions_to_summarise(T0 + 11 * 60_000, 10 * 60_000, 10).unwrap().len(), 2);
        assert!(s.sessions_to_summarise(T0 + 5 * 60_000, 10 * 60_000, 10).unwrap().is_empty(), "not idle long enough");
        assert!(s.sessions_to_summarise(T0 + IDLE_NAMED_MS + 1, 10 * 60_000, 10).unwrap().is_empty(), "a job that waited a day is not run");
        assert!(s.mark_summarised("ses_1").unwrap() && !s.mark_summarised("ses_1").unwrap());
        assert_eq!(s.sessions_to_summarise(T0 + 11 * 60_000, 10 * 60_000, 10).unwrap().len(), 1);
    }
}
