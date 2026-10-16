//! Where workflows and their run history live: `data/workflows.db` (SQLite, WAL).
//!
//! Every query is scoped by `owner_id`. The scheduler is the one caller that looks across owners
//! (`due`), and it runs each workflow as its own owner.

use super::model::*;
use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("workflow store: {e}")
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The next time a trigger is due after `from` (unix seconds). Only timed triggers have one.
pub fn next_run_for(trigger: &Trigger, from: i64) -> Option<i64> {
    match trigger {
        Trigger::Schedule { cron } => super::schedule::CronExpr::parse(cron).ok()?.next_after(from),
        Trigger::Interval { every_secs } => Some(from + *every_secs as i64),
        _ => None,
    }
}

/// What claiming a run did.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// A run row was created and is now running.
    Started(i64),
    /// No run was made; a `skipped` row records why.
    Skipped { run_id: i64, reason: String },
}

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`).
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS workflows (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 owner_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '',
                 enabled INTEGER NOT NULL DEFAULT 0,
                 trigger_json TEXT NOT NULL,
                 steps_json TEXT NOT NULL,
                 catch_up INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 next_run_at INTEGER,
                 last_run_at INTEGER NOT NULL DEFAULT 0,
                 last_status TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_workflows_owner ON workflows(owner_id);
             CREATE INDEX IF NOT EXISTS idx_workflows_due ON workflows(enabled, next_run_at);
             CREATE TABLE IF NOT EXISTS workflow_runs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 workflow_id INTEGER NOT NULL,
                 owner_id TEXT NOT NULL,
                 trigger_kind TEXT NOT NULL,
                 status TEXT NOT NULL,
                 started_at INTEGER NOT NULL,
                 finished_at INTEGER NOT NULL DEFAULT 0,
                 error TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_runs_workflow ON workflow_runs(owner_id, workflow_id, id);
             CREATE TABLE IF NOT EXISTS workflow_run_steps (
                 run_id INTEGER NOT NULL,
                 idx INTEGER NOT NULL,
                 step_id TEXT NOT NULL,
                 name TEXT NOT NULL DEFAULT '',
                 status TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 started_at INTEGER NOT NULL DEFAULT 0,
                 finished_at INTEGER NOT NULL DEFAULT 0,
                 output TEXT NOT NULL DEFAULT '',
                 error TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (run_id, idx)
             );",
}];

pub struct WorkflowStore {
    path: PathBuf,
}

const WORKFLOW_COLS: &str = "id, owner_id, name, description, enabled, trigger_json, steps_json, catch_up, created_at, updated_at, next_run_at, last_run_at, last_status";
const RUN_COLS: &str = "id, workflow_id, owner_id, trigger_kind, status, started_at, finished_at, error";

impl WorkflowStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let store = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = store.init() {
            tracing::warn!("{e}");
        }
        store
    }

    fn opts() -> Opts {
        Opts::standard().busy(10_000)
    }

    fn conn(&self) -> R<Connection> {
        sqlite_store::connect_checked(&self.path, &Self::opts(), sqlite_store::newest(STEPS)).map_err(|e| format!("workflow store: {e}"))
    }

    /// The folder, the check (a damaged file is moved aside and a new one made), the write-ahead log and the schema version. A file made before versions existed holds these
    /// tables already: the step does nothing to it and the version is stamped.
    fn init(&self) -> R<()> {
        sqlite_store::open_migrated(&self.path, &Self::opts(), STEPS).map(|_| ()).map_err(|e| format!("workflow store: {e}"))
    }

    fn workflow_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workflow> {
        let trigger_json: String = row.get(5)?;
        let steps_json: String = row.get(6)?;
        let bad = |what: &str, e: serde_json::Error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, format!("{what}: {e}").into());
        Ok(Workflow {
            id: row.get(0)?,
            owner_id: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            enabled: row.get::<_, i64>(4)? != 0,
            trigger: serde_json::from_str(&trigger_json).map_err(|e| bad("trigger", e))?,
            steps: serde_json::from_str(&steps_json).map_err(|e| bad("steps", e))?,
            catch_up: row.get::<_, i64>(7)? != 0,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            next_run_at: row.get(10)?,
            last_run_at: row.get(11)?,
            last_status: row.get(12)?,
        })
    }

    // ---- workflows ------------------------------------------------------------------------

    pub fn create(&self, owner: &str, input: &WorkflowInput, now: i64) -> R<Workflow> {
        input.validate()?;
        let next = if input.enabled { next_run_for(&input.trigger, now) } else { None };
        let c = self.conn()?;
        c.execute(
            "INSERT INTO workflows (owner_id, name, description, enabled, trigger_json, steps_json, catch_up, created_at, updated_at, next_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
            params![owner, input.name.trim(), input.description, input.enabled as i64, to_json(&input.trigger)?, to_json(&input.steps)?, input.catch_up as i64, now, next],
        )
        .map_err(db)?;
        let id = c.last_insert_rowid();
        self.get(owner, id)?.ok_or_else(|| "workflow store: the new workflow was not found".to_string())
    }

    pub fn update(&self, owner: &str, id: i64, input: &WorkflowInput, now: i64) -> R<Option<Workflow>> {
        input.validate()?;
        let next = if input.enabled { next_run_for(&input.trigger, now) } else { None };
        let c = self.conn()?;
        let n = c
            .execute(
                "UPDATE workflows SET name=?1, description=?2, enabled=?3, trigger_json=?4, steps_json=?5, catch_up=?6, updated_at=?7, next_run_at=?8
                 WHERE id=?9 AND owner_id=?10",
                params![input.name.trim(), input.description, input.enabled as i64, to_json(&input.trigger)?, to_json(&input.steps)?, input.catch_up as i64, now, next, id, owner],
            )
            .map_err(db)?;
        if n == 0 {
            return Ok(None);
        }
        self.get(owner, id)
    }

    pub fn get(&self, owner: &str, id: i64) -> R<Option<Workflow>> {
        let c = self.conn()?;
        c.query_row(&format!("SELECT {WORKFLOW_COLS} FROM workflows WHERE id=?1 AND owner_id=?2"), params![id, owner], Self::workflow_from).optional().map_err(db)
    }

    pub fn list(&self, owner: &str) -> R<Vec<Workflow>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {WORKFLOW_COLS} FROM workflows WHERE owner_id=?1 ORDER BY id")).map_err(db)?;
        let rows = stmt.query_map(params![owner], Self::workflow_from).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn delete(&self, owner: &str, id: i64) -> R<bool> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(db)?;
        let n = tx.execute("DELETE FROM workflows WHERE id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        if n > 0 {
            tx.execute("DELETE FROM workflow_run_steps WHERE run_id IN (SELECT id FROM workflow_runs WHERE workflow_id=?1 AND owner_id=?2)", params![id, owner]).map_err(db)?;
            tx.execute("DELETE FROM workflow_runs WHERE workflow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(n > 0)
    }

    pub fn set_enabled(&self, owner: &str, id: i64, enabled: bool, now: i64) -> R<Option<Workflow>> {
        let Some(w) = self.get(owner, id)? else { return Ok(None) };
        let next = if enabled { next_run_for(&w.trigger, now) } else { None };
        self.conn()?.execute("UPDATE workflows SET enabled=?1, next_run_at=?2, updated_at=?3 WHERE id=?4 AND owner_id=?5", params![enabled as i64, next, now, id, owner]).map_err(db)?;
        self.get(owner, id)
    }

    /// Enabled workflows whose scheduled time has come, across all owners (the scheduler's view).
    pub fn due(&self, now: i64) -> R<Vec<Workflow>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {WORKFLOW_COLS} FROM workflows WHERE enabled=1 AND next_run_at IS NOT NULL AND next_run_at<=?1 ORDER BY next_run_at")).map_err(db)?;
        let rows = stmt.query_map(params![now], Self::workflow_from).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// Enabled workflows that run when Aqua starts.
    pub fn startup_workflows(&self) -> R<Vec<Workflow>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {WORKFLOW_COLS} FROM workflows WHERE enabled=1 ORDER BY id")).map_err(db)?;
        let rows = stmt.query_map([], Self::workflow_from).map_err(db)?;
        Ok(rows.collect::<Result<Vec<_>, _>>().map_err(db)?.into_iter().filter(|w| w.trigger == Trigger::Startup).collect())
    }

    // ---- runs -----------------------------------------------------------------------------

    /// Start a run, or record why not, in one transaction so two callers cannot both start one.
    /// `scheduled` runs that find the previous run still going are skipped (and the schedule moves
    /// on); a manual run is refused with an error.
    pub fn claim(&self, w: &Workflow, trigger_kind: &str, scheduled: bool, now: i64) -> R<Claim> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let running: i64 = tx.query_row("SELECT COUNT(*) FROM workflow_runs WHERE workflow_id=?1 AND owner_id=?2 AND status='running'", params![w.id, w.owner_id], |r| r.get(0)).map_err(db)?;
        if scheduled {
            let next = next_run_for(&w.trigger, now);
            tx.execute("UPDATE workflows SET next_run_at=?1 WHERE id=?2 AND owner_id=?3", params![next, w.id, w.owner_id]).map_err(db)?;
        }
        if running > 0 {
            if !scheduled {
                return Err("this workflow is already running".into());
            }
            let reason = "not run: the previous run was still going".to_string();
            tx.execute("INSERT INTO workflow_runs (workflow_id, owner_id, trigger_kind, status, started_at, finished_at, error) VALUES (?1, ?2, ?3, 'skipped', ?4, ?4, ?5)", params![w.id, w.owner_id, trigger_kind, now, reason]).map_err(db)?;
            let run_id = tx.last_insert_rowid();
            tx.commit().map_err(db)?;
            return Ok(Claim::Skipped { run_id, reason });
        }
        tx.execute("INSERT INTO workflow_runs (workflow_id, owner_id, trigger_kind, status, started_at) VALUES (?1, ?2, ?3, 'running', ?4)", params![w.id, w.owner_id, trigger_kind, now]).map_err(db)?;
        let run_id = tx.last_insert_rowid();
        for (i, s) in w.steps.iter().enumerate() {
            tx.execute("INSERT INTO workflow_run_steps (run_id, idx, step_id, name, status) VALUES (?1, ?2, ?3, ?4, 'pending')", params![run_id, i as i64, s.id, s.name]).map_err(db)?;
        }
        tx.execute("UPDATE workflows SET last_run_at=?1, last_status='running' WHERE id=?2 AND owner_id=?3", params![now, w.id, w.owner_id]).map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(Claim::Started(run_id))
    }

    /// A scheduled run that was missed while Aqua was closed: recorded as skipped and the schedule moves on.
    pub fn record_missed(&self, w: &Workflow, now: i64, due_at: i64) -> R<i64> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let reason = format!("missed: it was due {} minutes ago while Aqua was not running", ((now - due_at) / 60).max(1));
        tx.execute("INSERT INTO workflow_runs (workflow_id, owner_id, trigger_kind, status, started_at, finished_at, error) VALUES (?1, ?2, ?3, 'skipped', ?4, ?4, ?5)", params![w.id, w.owner_id, w.trigger.kind(), now, reason]).map_err(db)?;
        let run_id = tx.last_insert_rowid();
        tx.execute("UPDATE workflows SET next_run_at=?1, last_status='skipped' WHERE id=?2 AND owner_id=?3", params![next_run_for(&w.trigger, now), w.id, w.owner_id]).map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(run_id)
    }

    pub fn step_started(&self, run_id: i64, idx: u32, now: i64) -> R<()> {
        self.conn()?.execute("UPDATE workflow_run_steps SET status='running', started_at=?1, attempts=attempts WHERE run_id=?2 AND idx=?3", params![now, run_id, idx]).map_err(db)?;
        Ok(())
    }

    pub fn step_attempt(&self, run_id: i64, idx: u32, attempts: u32) -> R<()> {
        self.conn()?.execute("UPDATE workflow_run_steps SET attempts=?1 WHERE run_id=?2 AND idx=?3", params![attempts, run_id, idx]).map_err(db)?;
        Ok(())
    }

    pub fn step_finished(&self, run_id: i64, idx: u32, status: &str, output: &str, error: &str, now: i64) -> R<()> {
        self.conn()?.execute("UPDATE workflow_run_steps SET status=?1, output=?2, error=?3, finished_at=?4 WHERE run_id=?5 AND idx=?6", params![status, output, error, now, run_id, idx]).map_err(db)?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: i64, status: RunStatus, error: &str, now: i64) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE workflow_runs SET status=?1, error=?2, finished_at=?3 WHERE id=?4", params![status.as_str(), error, now, run_id]).map_err(db)?;
        c.execute("UPDATE workflows SET last_status=?1 WHERE id=(SELECT workflow_id FROM workflow_runs WHERE id=?2)", params![status.as_str(), run_id]).map_err(db)?;
        Ok(())
    }

    fn run_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
        Ok(Run {
            id: row.get(0)?,
            workflow_id: row.get(1)?,
            owner_id: row.get(2)?,
            trigger_kind: row.get(3)?,
            status: RunStatus::parse(&row.get::<_, String>(4)?),
            started_at: row.get(5)?,
            finished_at: row.get(6)?,
            error: row.get(7)?,
            steps: Vec::new(),
        })
    }

    fn steps_of(c: &Connection, run_id: i64) -> R<Vec<StepRun>> {
        let mut stmt = c.prepare("SELECT idx, step_id, name, status, attempts, started_at, finished_at, output, error FROM workflow_run_steps WHERE run_id=?1 ORDER BY idx").map_err(db)?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(StepRun { idx: r.get(0)?, step_id: r.get(1)?, name: r.get(2)?, status: r.get(3)?, attempts: r.get(4)?, started_at: r.get(5)?, finished_at: r.get(6)?, output: r.get(7)?, error: r.get(8)? })
            })
            .map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    /// The most recent runs of a workflow, newest first, without their step logs.
    pub fn list_runs(&self, owner: &str, workflow_id: i64, limit: u32) -> R<Vec<Run>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE owner_id=?1 AND workflow_id=?2 ORDER BY id DESC LIMIT ?3")).map_err(db)?;
        let rows = stmt.query_map(params![owner, workflow_id, limit.clamp(1, 200)], Self::run_from).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn get_run(&self, owner: &str, run_id: i64) -> R<Option<Run>> {
        let c = self.conn()?;
        let run = c.query_row(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id=?1 AND owner_id=?2"), params![run_id, owner], Self::run_from).optional().map_err(db)?;
        match run {
            Some(mut r) => {
                r.steps = Self::steps_of(&c, r.id)?;
                Ok(Some(r))
            }
            None => Ok(None),
        }
    }

    /// Runs left "running" by a crash or a shutdown become `interrupted`. Returns how many.
    pub fn recover_interrupted(&self, now: i64) -> R<usize> {
        let c = self.conn()?;
        c.execute("UPDATE workflow_run_steps SET status='failed', error='Aqua stopped while this step was running', finished_at=?1 WHERE status='running'", params![now]).map_err(db)?;
        c.execute("UPDATE workflow_run_steps SET status='skipped' WHERE status='pending' AND run_id IN (SELECT id FROM workflow_runs WHERE status='running')", []).map_err(db)?;
        let n = c.execute("UPDATE workflow_runs SET status='interrupted', error='Aqua stopped while this run was going', finished_at=?1 WHERE status='running'", params![now]).map_err(db)?;
        c.execute("UPDATE workflows SET last_status='interrupted' WHERE last_status='running'", []).map_err(db)?;
        Ok(n)
    }

    /// Keep the history bounded: the newest `keep` runs of each workflow, and nothing older than `max_age_secs`.
    pub fn prune(&self, keep: u32, max_age_secs: i64, now: i64) -> R<usize> {
        let c = self.conn()?;
        let doomed = "SELECT id FROM workflow_runs WHERE status<>'running' AND (finished_at<?1 OR id NOT IN (
                         SELECT id FROM workflow_runs r2 WHERE r2.workflow_id=workflow_runs.workflow_id AND r2.owner_id=workflow_runs.owner_id ORDER BY id DESC LIMIT ?2))";
        c.execute(&format!("DELETE FROM workflow_run_steps WHERE run_id IN ({doomed})"), params![now - max_age_secs, keep]).map_err(db)?;
        c.execute(&format!("DELETE FROM workflow_runs WHERE id IN ({doomed})"), params![now - max_age_secs, keep]).map_err(db)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> R<String> {
    serde_json::to_string(v).map_err(|e| format!("workflow store: {e}"))
}
