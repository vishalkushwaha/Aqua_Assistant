//! Schedules, what happened each time one came due, and the scheduler's own heartbeat: `data/flows.db`.
//!
//! The heartbeat is how a missed run is explained. Every tick writes the time; when the next tick finds
//! the last one long ago, that stretch is recorded as a gap ("Aqua was not running from 09:58 to 10:22"),
//! and a missed schedule can then say exactly why.

use super::model::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

fn json<T: serde::Serialize>(v: &T) -> R<String> {
    serde_json::to_string(v).map_err(|e| format!("flow store: {e}"))
}

/// A tick is expected about this often; a longer silence is a gap.
pub const GAP_AFTER_SECS: i64 = 60;

pub fn init_schema(c: &Connection) -> R<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS flow_schedules (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             owner_id TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             name TEXT NOT NULL,
             when_json TEXT NOT NULL,
             timezone TEXT NOT NULL DEFAULT '',
             inputs_json TEXT NOT NULL DEFAULT '{}',
             misfire TEXT NOT NULL DEFAULT 'ask',
             notify_json TEXT NOT NULL DEFAULT '{}',
             enabled INTEGER NOT NULL DEFAULT 1,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             next_run_at INTEGER,
             last_run_at INTEGER NOT NULL DEFAULT 0,
             last_run_id INTEGER,
             last_status TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_flow_schedules_owner ON flow_schedules(owner_id, flow_id);
         CREATE INDEX IF NOT EXISTS idx_flow_schedules_due ON flow_schedules(enabled, next_run_at);
         CREATE TABLE IF NOT EXISTS flow_schedule_occurrences (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             schedule_id INTEGER NOT NULL,
             owner_id TEXT NOT NULL,
             due_at INTEGER NOT NULL,
             decided_at INTEGER NOT NULL,
             outcome TEXT NOT NULL,
             run_id INTEGER,
             reason TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_occurrences ON flow_schedule_occurrences(owner_id, schedule_id, id);
         CREATE TABLE IF NOT EXISTS flow_clock (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             heartbeat_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS flow_clock_gaps (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             start_at INTEGER NOT NULL,
             end_at INTEGER NOT NULL
         );",
    )
    .map_err(db)?;
    // Campaign columns, added to a database that already has schedules.
    let have: Vec<String> = c.prepare("PRAGMA table_info(flow_schedules)").map_err(db)?.query_map([], |r| r.get::<_, String>(1)).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db)?;
    for (name, ddl) in [("ends_at", "INTEGER"), ("end_on", "TEXT"), ("max_runs", "INTEGER"), ("series_json", "TEXT NOT NULL DEFAULT '[]'"), ("runs_started", "INTEGER NOT NULL DEFAULT 0")] {
        if !have.iter().any(|h| h == name) {
            c.execute(&format!("ALTER TABLE flow_schedules ADD COLUMN {name} {ddl}"), []).map_err(db)?;
        }
    }
    Ok(())
}

pub struct SchedStore {
    path: PathBuf,
}

const COLS: &str = "id, owner_id, flow_id, name, when_json, timezone, inputs_json, misfire, notify_json, enabled, created_at, updated_at, next_run_at, last_run_at, last_run_id, last_status, ends_at, end_on, max_runs, series_json, runs_started";

fn parse_misfire(s: &str) -> Misfire {
    match s {
        "run_now" => Misfire::RunNow,
        "skip" => Misfire::Skip,
        "next_run" => Misfire::NextRun,
        _ => Misfire::Ask,
    }
}

fn misfire_str(m: Misfire) -> &'static str {
    match m {
        Misfire::RunNow => "run_now",
        Misfire::Skip => "skip",
        Misfire::Ask => "ask",
        Misfire::NextRun => "next_run",
    }
}

impl SchedStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let s = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = s.conn().and_then(|c| init_schema(&c)) {
            tracing::warn!("{e}");
        }
        s
    }

    fn conn(&self) -> R<Connection> {
        crate::flows::store::conn_at(&self.path)
    }

    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
        let bad = |what: &str| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, what.to_string().into());
        let when: When = serde_json::from_str(&r.get::<_, String>(4)?).map_err(|_| bad("when"))?;
        let inputs: Map<String, Value> = serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default();
        let notify: NotifyPrefs = serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default();
        Ok(Schedule {
            id: r.get(0)?,
            owner_id: r.get(1)?,
            flow_id: r.get(2)?,
            name: r.get(3)?,
            when,
            timezone: r.get(5)?,
            inputs,
            misfire: parse_misfire(&r.get::<_, String>(7)?),
            notify,
            enabled: r.get::<_, i64>(9)? != 0,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
            next_run_at: r.get(12)?,
            last_run_at: r.get(13)?,
            last_run_id: r.get(14)?,
            last_status: r.get(15)?,
            ends_at: r.get(16)?,
            campaign: Campaign {
                end_on: r.get(17)?,
                max_runs: r.get::<_, Option<i64>>(18)?.map(|m| m as u32),
                series: serde_json::from_str::<Vec<Map<String, Value>>>(&r.get::<_, String>(19)?).unwrap_or_default(),
            },
            runs_started: r.get::<_, i64>(20)? as u32,
        })
    }

    // ---- schedules ----------------------------------------------------------------------------

    pub fn create(&self, owner: &str, flow_id: i64, input: &ScheduleInput, now: i64) -> R<Schedule> {
        let (zone, next) = input.validate(now)?;
        let ends_at = input.ends_at(&zone)?;
        let c = self.conn()?;
        c.execute(
            "INSERT INTO flow_schedules (owner_id, flow_id, name, when_json, timezone, inputs_json, misfire, notify_json, enabled, created_at, updated_at, next_run_at, ends_at, end_on, max_runs, series_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![owner, flow_id, input.name.trim(), json(&input.when)?, input.timezone.trim(), Value::Object(input.inputs.clone()).to_string(), misfire_str(input.misfire), json(&input.notify)?, input.enabled as i64, now, if input.enabled { next } else { None }, ends_at, input.campaign.end_on.as_deref().map(str::trim).filter(|d| !d.is_empty()), input.campaign.max_runs.map(|m| m as i64), serde_json::to_string(&input.campaign.series).map_err(|e| e.to_string())?],
        )
        .map_err(db)?;
        let id = c.last_insert_rowid();
        self.get(owner, id)?.ok_or_else(|| "flow store: the new schedule was not found".to_string())
    }

    pub fn update(&self, owner: &str, id: i64, input: &ScheduleInput, now: i64) -> R<Option<Schedule>> {
        let (zone, next) = input.validate(now)?;
        let ends_at = input.ends_at(&zone)?;
        let n = self
            .conn()?
            .execute(
                "UPDATE flow_schedules SET name=?1, when_json=?2, timezone=?3, inputs_json=?4, misfire=?5, notify_json=?6, enabled=?7, updated_at=?8, next_run_at=?9, ends_at=?12, end_on=?13, max_runs=?14, series_json=?15 WHERE id=?10 AND owner_id=?11",
                params![input.name.trim(), json(&input.when)?, input.timezone.trim(), Value::Object(input.inputs.clone()).to_string(), misfire_str(input.misfire), json(&input.notify)?, input.enabled as i64, now, if input.enabled { next } else { None }, id, owner, ends_at, input.campaign.end_on.as_deref().map(str::trim).filter(|d| !d.is_empty()), input.campaign.max_runs.map(|m| m as i64), serde_json::to_string(&input.campaign.series).map_err(|e| e.to_string())?],
            )
            .map_err(db)?;
        if n == 0 {
            return Ok(None);
        }
        self.get(owner, id)
    }

    pub fn get(&self, owner: &str, id: i64) -> R<Option<Schedule>> {
        self.conn()?.query_row(&format!("SELECT {COLS} FROM flow_schedules WHERE id=?1 AND owner_id=?2"), params![id, owner], Self::from_row).optional().map_err(db)
    }

    pub fn list(&self, owner: &str, flow_id: Option<i64>) -> R<Vec<Schedule>> {
        let c = self.conn()?;
        match flow_id {
            Some(f) => c.prepare(&format!("SELECT {COLS} FROM flow_schedules WHERE owner_id=?1 AND flow_id=?2 ORDER BY id")).map_err(db)?.query_map(params![owner, f], Self::from_row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
            None => c.prepare(&format!("SELECT {COLS} FROM flow_schedules WHERE owner_id=?1 ORDER BY id")).map_err(db)?.query_map(params![owner], Self::from_row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
        }
    }

    pub fn delete(&self, owner: &str, id: i64) -> R<bool> {
        let c = self.conn()?;
        let n = c.execute("DELETE FROM flow_schedules WHERE id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        if n > 0 {
            c.execute("DELETE FROM flow_schedule_occurrences WHERE schedule_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        }
        Ok(n > 0)
    }

    pub fn delete_for_flow(&self, owner: &str, flow_id: i64) -> R<usize> {
        let c = self.conn()?;
        c.execute("DELETE FROM flow_schedule_occurrences WHERE owner_id=?1 AND schedule_id IN (SELECT id FROM flow_schedules WHERE owner_id=?1 AND flow_id=?2)", params![owner, flow_id]).map_err(db)?;
        c.execute("DELETE FROM flow_schedules WHERE owner_id=?1 AND flow_id=?2", params![owner, flow_id]).map_err(db)
    }

    /// Switch on or off. Switching on works out the next time from `now`, so nothing stale fires.
    pub fn set_enabled(&self, owner: &str, id: i64, enabled: bool, now: i64) -> R<Option<Schedule>> {
        let Some(s) = self.get(owner, id)? else { return Ok(None) };
        let next = if enabled {
            match &s.when {
                When::Cron { .. } => {
                    if s.campaign_over(now) {
                        return Err("this campaign has made all its runs (or its last day has passed), so it cannot be switched on again; create a new one".into());
                    }
                    s.next_within_campaign(now, false)
                }
                When::Once { when } => {
                    let ts = s.zone()?.to_timestamp(chrono::NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M").map_err(|e| e.to_string())?);
                    ts.filter(|t| *t > now)
                }
            }
        } else {
            None
        };
        if enabled && next.is_none() {
            return Err("a one-time schedule whose time has passed cannot be switched on again; create a new one".into());
        }
        self.conn()?.execute("UPDATE flow_schedules SET enabled=?1, next_run_at=?2, updated_at=?3 WHERE id=?4 AND owner_id=?5", params![enabled as i64, next, now, id, owner]).map_err(db)?;
        self.get(owner, id)
    }

    /// Enabled schedules whose time has come, across owners (the scheduler's view).
    pub fn due(&self, now: i64) -> R<Vec<Schedule>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {COLS} FROM flow_schedules WHERE enabled=1 AND next_run_at IS NOT NULL AND next_run_at<=?1 ORDER BY next_run_at")).map_err(db)?;
        let rows = stmt.query_map(params![now], Self::from_row).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// After a time has been dealt with: when it is next due, and how the last one went.
    pub fn advance(&self, id: i64, next: Option<i64>, ran_at: i64, run_id: Option<i64>, status: &str) -> R<()> {
        self.conn()?
            .execute(
                "UPDATE flow_schedules SET next_run_at=?1, last_run_at=CASE WHEN ?2>0 THEN ?2 ELSE last_run_at END, last_run_id=COALESCE(?3, last_run_id), last_status=?4, enabled=CASE WHEN ?1 IS NULL THEN 0 ELSE enabled END WHERE id=?5",
                params![next, ran_at, run_id, status, id],
            )
            .map_err(db)?;
        Ok(())
    }

    /// A run was started for this schedule: it counts towards a campaign's number of runs.
    pub fn note_started(&self, id: i64) -> R<()> {
        self.conn()?.execute("UPDATE flow_schedules SET runs_started = runs_started + 1 WHERE id=?1", params![id]).map_err(db)?;
        Ok(())
    }

    pub fn set_last_status(&self, id: i64, run_id: i64, status: &str) -> R<()> {
        self.conn()?.execute("UPDATE flow_schedules SET last_run_id=?1, last_status=?2 WHERE id=?3", params![run_id, status, id]).map_err(db)?;
        Ok(())
    }

    // ---- occurrences ---------------------------------------------------------------------------------

    pub fn add_occurrence(&self, owner: &str, schedule_id: i64, due_at: i64, decided_at: i64, outcome: &str, run_id: Option<i64>, reason: &str) -> R<i64> {
        let c = self.conn()?;
        c.execute("INSERT INTO flow_schedule_occurrences (schedule_id, owner_id, due_at, decided_at, outcome, run_id, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![schedule_id, owner, due_at, decided_at, outcome, run_id, reason]).map_err(db)?;
        Ok(c.last_insert_rowid())
    }

    fn occ_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Occurrence> {
        Ok(Occurrence { id: r.get(0)?, schedule_id: r.get(1)?, due_at: r.get(2)?, decided_at: r.get(3)?, outcome: r.get(4)?, run_id: r.get(5)?, reason: r.get(6)? })
    }

    pub fn occurrences(&self, owner: &str, schedule_id: i64, limit: u32) -> R<Vec<Occurrence>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT id, schedule_id, due_at, decided_at, outcome, run_id, reason FROM flow_schedule_occurrences WHERE owner_id=?1 AND schedule_id=?2 ORDER BY id DESC LIMIT ?3").map_err(db)?;
        let rows = stmt.query_map(params![owner, schedule_id, limit.clamp(1, 200)], Self::occ_from).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn occurrence(&self, owner: &str, id: i64) -> R<Option<Occurrence>> {
        self.conn()?.query_row("SELECT id, schedule_id, due_at, decided_at, outcome, run_id, reason FROM flow_schedule_occurrences WHERE id=?1 AND owner_id=?2", params![id, owner], Self::occ_from).optional().map_err(db)
    }

    /// Times that were missed and are waiting for the person to decide.
    pub fn pending_missed(&self, owner: &str) -> R<Vec<(Occurrence, String)>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT o.id, o.schedule_id, o.due_at, o.decided_at, o.outcome, o.run_id, o.reason, s.name FROM flow_schedule_occurrences o JOIN flow_schedules s ON s.id=o.schedule_id WHERE o.owner_id=?1 AND o.outcome='missed_pending' ORDER BY o.id")
            .map_err(db)?;
        let rows = stmt.query_map(params![owner], |r| Ok((Self::occ_from(r)?, r.get::<_, String>(7)?))).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn resolve_occurrence(&self, id: i64, outcome: &str, run_id: Option<i64>, reason: &str, at: i64) -> R<()> {
        self.conn()?.execute("UPDATE flow_schedule_occurrences SET outcome=?1, run_id=?2, reason=?3, decided_at=?4 WHERE id=?5", params![outcome, run_id, reason, at, id]).map_err(db)?;
        Ok(())
    }

    /// The newest run a schedule started that is still going (queued, running, waiting, paused). An interrupted run (its
    /// driver died) is not counted: it is waiting for a person to resume it, and until then it must not hold back every
    /// later time of its schedule. Resuming it makes it running again, and then it counts.
    pub fn active_run_of(&self, owner: &str, schedule_id: i64) -> R<Option<(i64, String)>> {
        // The id is followed by a comma or the closing brace, whatever order the keys were written in.
        let (mid, end) = (format!("%\"schedule_id\":{schedule_id},%"), format!("%\"schedule_id\":{schedule_id}}}"));
        self.conn()?
            .query_row("SELECT id, status FROM flow_runs WHERE owner_id=?1 AND trigger_kind='schedule' AND (payload_json LIKE ?2 OR payload_json LIKE ?3) AND status IN ('queued','running','waiting','paused') ORDER BY id DESC LIMIT 1", params![owner, mid, end], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(db)
    }

    // ---- the heartbeat -----------------------------------------------------------------------------------

    /// Record that the scheduler is alive. Returns the gap since the last beat if it was longer than a tick
    /// should be, as (from, to); the gap is remembered so a missed run can be explained later.
    pub fn heartbeat(&self, now: i64) -> R<Option<(i64, i64)>> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let prev: Option<i64> = tx.query_row("SELECT heartbeat_at FROM flow_clock WHERE id=1", [], |r| r.get(0)).optional().map_err(db)?;
        tx.execute("INSERT INTO flow_clock (id, heartbeat_at) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET heartbeat_at=?1", params![now]).map_err(db)?;
        let gap = prev.filter(|p| now - p > GAP_AFTER_SECS).map(|p| (p, now));
        if let Some((a, b)) = gap {
            tx.execute("INSERT INTO flow_clock_gaps (start_at, end_at) VALUES (?1, ?2)", params![a, b]).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(gap)
    }

    /// Forget the record of times that were decided long ago (all but the newest `keep` of each schedule, and anything decided before
    /// `before`) and the stretches when Aqua was not running that ended before it. A missed time still waiting for the person's
    /// answer is never dropped.
    pub fn prune(&self, keep: u32, before: i64) -> R<usize> {
        let c = self.conn()?;
        let occurrences = c
            .execute(
                "DELETE FROM flow_schedule_occurrences WHERE outcome<>'missed_pending' AND (decided_at<?1 OR id NOT IN (SELECT id FROM flow_schedule_occurrences o2 WHERE o2.schedule_id=flow_schedule_occurrences.schedule_id ORDER BY id DESC LIMIT ?2))",
                params![before, keep],
            )
            .map_err(db)?;
        let gaps = c.execute("DELETE FROM flow_clock_gaps WHERE end_at<?1", params![before]).map_err(db)?;
        Ok(occurrences + gaps)
    }

    /// The stretch during which Aqua was not running that covers a moment.
    pub fn gap_covering(&self, ts: i64) -> R<Option<(i64, i64)>> {
        self.conn()?.query_row("SELECT start_at, end_at FROM flow_clock_gaps WHERE start_at<=?1 AND end_at>=?1 ORDER BY id DESC LIMIT 1", params![ts], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db)
    }
}
