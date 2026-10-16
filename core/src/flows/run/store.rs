//! Where runs live: the same `data/flows.db` as the flows themselves.
//!
//! Every write of a step's state goes to the database at once, so a crash loses at most the step that
//! was in flight, and that step is marked `interrupted` (never assumed to have failed or succeeded).
//! Every query is scoped by `owner_id`.

use super::model::*;
use crate::flows::model::{Flow, FlowDef};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

/// How long a key for the start of a run is remembered.
pub const KEY_MEMORY_SECS: i64 = 7 * 24 * 3600;

/// What a start says when its key was used before for another request (the routes answer it as an unprocessable request).
pub const KEY_MISMATCH: &str = "this key was used for a different request: use a new key for a new request";

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

/// The run tables. Also called by the flow store, so deleting a flow can remove its runs.
pub fn init_schema(c: &Connection) -> R<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS flow_runs (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             owner_id TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             flow_version INTEGER NOT NULL,
             name TEXT NOT NULL,
             status TEXT NOT NULL,
             trigger_kind TEXT NOT NULL,
             dry_run INTEGER NOT NULL DEFAULT 0,
             vars_json TEXT NOT NULL DEFAULT '{}',
             payload_json TEXT NOT NULL DEFAULT 'null',
             def_json TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             started_at INTEGER NOT NULL DEFAULT 0,
             finished_at INTEGER NOT NULL DEFAULT 0,
             error TEXT NOT NULL DEFAULT '',
             from_run INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_flow_runs_flow ON flow_runs(owner_id, flow_id, id);
         CREATE TABLE IF NOT EXISTS flow_node_runs (
             run_id INTEGER NOT NULL,
             node_id TEXT NOT NULL,
             status TEXT NOT NULL,
             attempts INTEGER NOT NULL DEFAULT 0,
             started_at INTEGER NOT NULL DEFAULT 0,
             finished_at INTEGER NOT NULL DEFAULT 0,
             outputs_json TEXT NOT NULL DEFAULT '{}',
             taken_json TEXT NOT NULL DEFAULT '[]',
             error TEXT NOT NULL DEFAULT '',
             wait_json TEXT,
             gate_approved INTEGER NOT NULL DEFAULT 0,
             note TEXT NOT NULL DEFAULT '',
             loop_json TEXT,
             PRIMARY KEY (run_id, node_id)
         );
         CREATE TABLE IF NOT EXISTS flow_run_keys (
             owner_id TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             idem_key TEXT NOT NULL,
             fingerprint TEXT NOT NULL DEFAULT '',
             run_id INTEGER NOT NULL,
             created_at INTEGER NOT NULL,
             PRIMARY KEY (owner_id, flow_id, idem_key)
         );
         CREATE TABLE IF NOT EXISTS flow_run_events (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             run_id INTEGER NOT NULL,
             at INTEGER NOT NULL,
             node_id TEXT,
             level TEXT NOT NULL,
             kind TEXT NOT NULL,
             message TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_flow_run_events ON flow_run_events(run_id, id);
         CREATE TABLE IF NOT EXISTS flow_effects (
             idem_key TEXT PRIMARY KEY,
             run_id INTEGER NOT NULL,
             node_id TEXT NOT NULL,
             status TEXT NOT NULL,
             result_json TEXT NOT NULL DEFAULT 'null',
             at INTEGER NOT NULL
         );",
    )
    .map_err(db)?;
    // The place of a For each step in its list (card R7-12), added to a database that already has step records.
    let have: Vec<String> = c.prepare("PRAGMA table_info(flow_node_runs)").map_err(db)?.query_map([], |r| r.get::<_, String>(1)).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db)?;
    if !have.iter().any(|h| h == "loop_json") {
        c.execute("ALTER TABLE flow_node_runs ADD COLUMN loop_json TEXT", []).map_err(db)?;
    }
    super::recovery_store::init(c)
}

pub struct RunStore {
    path: PathBuf,
}

fn parse_map(s: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(s).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

const RUN_COLS: &str = "id, owner_id, flow_id, flow_version, name, status, trigger_kind, dry_run, vars_json, payload_json, created_at, started_at, finished_at, error, from_run";

impl RunStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let s = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = s.conn().and_then(|c| init_schema(&c)) {
            tracing::warn!("{e}");
        }
        s
    }

    pub(super) fn conn(&self) -> R<Connection> {
        crate::flows::store::conn_at(&self.path)
    }

    fn run_from(r: &rusqlite::Row<'_>, def: FlowDef) -> rusqlite::Result<Run> {
        Ok(Run {
            id: r.get(0)?,
            owner_id: r.get(1)?,
            flow_id: r.get(2)?,
            flow_version: r.get(3)?,
            name: r.get(4)?,
            status: RunStatus::parse(&r.get::<_, String>(5)?),
            trigger_kind: r.get(6)?,
            dry_run: r.get::<_, i64>(7)? != 0,
            vars: parse_map(&r.get::<_, String>(8)?),
            payload: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
            created_at: r.get(10)?,
            started_at: r.get(11)?,
            finished_at: r.get(12)?,
            error: r.get(13)?,
            from_run: r.get(14)?,
            def,
        })
    }

    // ---- runs ---------------------------------------------------------------------------------

    /// A new run of `flow` with a step-run row for every step, all pending.
    #[allow(clippy::too_many_arguments)]
    pub fn create_run(&self, owner: &str, flow: &Flow, vars: Map<String, Value>, payload: Value, trigger_kind: &str, dry_run: bool, from_run: Option<i64>, at: i64) -> R<Run> {
        self.create_run_keyed(owner, flow, vars, payload, trigger_kind, dry_run, from_run, at, None).map(|(run, _)| run)
    }

    /// The run that a key started before, when it is still there: `Ok(None)` for a key not seen (or whose run has been cleared away), an error when it was used for another request
    /// (`fingerprint` says what the request is).
    pub fn run_for_key(&self, owner: &str, flow_id: i64, key: &str, fingerprint: &str) -> R<Option<Run>> {
        let c = self.conn()?;
        let Some((run_id, first)) = Self::key_seen(&c, owner, flow_id, key)? else { return Ok(None) };
        let Some(run) = self.get_run(owner, run_id)? else { return Ok(None) };
        if first != fingerprint {
            return Err(KEY_MISMATCH.into());
        }
        Ok(Some(run))
    }

    fn key_seen(c: &Connection, owner: &str, flow_id: i64, key: &str) -> R<Option<(i64, String)>> {
        c.query_row("SELECT run_id, fingerprint FROM flow_run_keys WHERE owner_id=?1 AND flow_id=?2 AND idem_key=?3", params![owner, flow_id, key], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db)
    }

    /// [`create_run`] for a start that has a key `(key, fingerprint)`: a key that started a run before (and the same request) gives that run back, with `true`, and creates nothing.
    /// The look and the write are one step under the write lock, so two starts with one key cannot both make a run.
    #[allow(clippy::too_many_arguments)]
    pub fn create_run_keyed(&self, owner: &str, flow: &Flow, vars: Map<String, Value>, payload: Value, trigger_kind: &str, dry_run: bool, from_run: Option<i64>, at: i64, key: Option<(&str, &str)>) -> R<(Run, bool)> {
        let mut c = self.conn()?;
        let tx = if key.is_some() { c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) } else { c.transaction() }.map_err(db)?;
        if let Some((k, fingerprint)) = key {
            if let Some((run_id, first)) = Self::key_seen(&tx, owner, flow.id, k)? {
                let still_there = tx.query_row("SELECT COUNT(*) FROM flow_runs WHERE id=?1 AND owner_id=?2", params![run_id, owner], |r| r.get::<_, i64>(0)).map_err(db)? > 0;
                if still_there {
                    drop(tx);
                    if first != fingerprint {
                        return Err(KEY_MISMATCH.into());
                    }
                    let run = self.get_run(owner, run_id)?.ok_or_else(|| "flow store: the run of this key was not found".to_string())?;
                    return Ok((run, true));
                }
                // The run was cleared away (kept runs are limited): the key starts a new one.
                tx.execute("DELETE FROM flow_run_keys WHERE owner_id=?1 AND flow_id=?2 AND idem_key=?3", params![owner, flow.id, k]).map_err(db)?;
            }
            tx.execute("DELETE FROM flow_run_keys WHERE created_at<?1", params![at - KEY_MEMORY_SECS]).map_err(db)?;
        }
        let def_json = serde_json::to_string(&flow.def).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO flow_runs (owner_id, flow_id, flow_version, name, status, trigger_kind, dry_run, vars_json, payload_json, def_json, created_at, from_run) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![owner, flow.id, flow.version, flow.def.name, trigger_kind, dry_run as i64, Value::Object(vars).to_string(), payload.to_string(), def_json, at, from_run],
        )
        .map_err(db)?;
        let id = tx.last_insert_rowid();
        for n in &flow.def.nodes {
            tx.execute("INSERT INTO flow_node_runs (run_id, node_id, status) VALUES (?1, ?2, 'pending')", params![id, n.id]).map_err(db)?;
        }
        if let Some((k, fingerprint)) = key {
            tx.execute("INSERT INTO flow_run_keys (owner_id, flow_id, idem_key, fingerprint, run_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![owner, flow.id, k, fingerprint, id, at]).map_err(db)?;
        }
        tx.commit().map_err(db)?;
        let run = self.get_run(owner, id)?.ok_or_else(|| "flow store: the new run was not found".to_string())?;
        Ok((run, false))
    }

    pub fn get_run(&self, owner: &str, id: i64) -> R<Option<Run>> {
        let c = self.conn()?;
        let def_json: Option<String> = c.query_row("SELECT def_json FROM flow_runs WHERE id=?1 AND owner_id=?2", params![id, owner], |r| r.get(0)).optional().map_err(db)?;
        let Some(def_json) = def_json else { return Ok(None) };
        let def: FlowDef = serde_json::from_str(&def_json).map_err(|e| format!("flow store: a stored run could not be read: {e}"))?;
        c.query_row(&format!("SELECT {RUN_COLS} FROM flow_runs WHERE id=?1 AND owner_id=?2"), params![id, owner], |r| Self::run_from(r, def.clone())).optional().map_err(db)
    }

    /// The newest runs, without the flow snapshot (which can be large).
    pub fn list_runs(&self, owner: &str, flow_id: Option<i64>, limit: u32) -> R<Vec<Run>> {
        let c = self.conn()?;
        let limit = limit.clamp(1, 200);
        let map = |r: &rusqlite::Row<'_>| Self::run_from(r, FlowDef::default());
        match flow_id {
            Some(f) => c.prepare(&format!("SELECT {RUN_COLS} FROM flow_runs WHERE owner_id=?1 AND flow_id=?2 ORDER BY id DESC LIMIT ?3")).map_err(db)?.query_map(params![owner, f, limit], map).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
            None => c.prepare(&format!("SELECT {RUN_COLS} FROM flow_runs WHERE owner_id=?1 ORDER BY id DESC LIMIT ?2")).map_err(db)?.query_map(params![owner, limit], map).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db),
        }
    }

    /// Runs marked running, with when anything last happened in each (its last event, else its start).
    pub fn running_runs(&self) -> R<Vec<(i64, String, i64, String, i64)>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT r.id, r.owner_id, r.flow_id, r.name, COALESCE((SELECT MAX(at) FROM flow_run_events e WHERE e.run_id = r.id), r.started_at, r.created_at) FROM flow_runs r WHERE r.status = 'running'").map_err(db)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db);
        rows
    }

    pub fn set_status(&self, id: i64, status: RunStatus, error: &str, at: i64) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE flow_runs SET status=?1, error=?2, started_at=CASE WHEN started_at=0 AND ?1<>'queued' THEN ?3 ELSE started_at END, finished_at=CASE WHEN ?4 THEN ?3 ELSE 0 END WHERE id=?5", params![status.as_str(), error, at, status.is_finished(), id]).map_err(db)?;
        Ok(())
    }

    pub fn set_vars(&self, id: i64, vars: &Map<String, Value>) -> R<()> {
        self.conn()?.execute("UPDATE flow_runs SET vars_json=?1 WHERE id=?2", params![Value::Object(vars.clone()).to_string(), id]).map_err(db)?;
        Ok(())
    }

    pub fn has_success_for_version(&self, owner: &str, flow_id: i64, version: i64) -> R<bool> {
        let n: i64 = self.conn()?.query_row("SELECT COUNT(*) FROM flow_runs WHERE owner_id=?1 AND flow_id=?2 AND flow_version=?3 AND status='succeeded' AND dry_run=0", params![owner, flow_id, version], |r| r.get(0)).map_err(db)?;
        Ok(n > 0)
    }

    // ---- steps --------------------------------------------------------------------------------

    fn node_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRun> {
        let wait: Option<String> = r.get(9)?;
        Ok(NodeRun {
            run_id: r.get(0)?,
            node_id: r.get(1)?,
            status: NodeStatus::parse(&r.get::<_, String>(2)?),
            attempts: r.get(3)?,
            started_at: r.get(4)?,
            finished_at: r.get(5)?,
            outputs: parse_map(&r.get::<_, String>(6)?),
            taken: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
            error: r.get(8)?,
            wait: wait.and_then(|w| serde_json::from_str(&w).ok()),
            gate_approved: r.get::<_, i64>(10)? != 0,
            note: r.get(11)?,
            looping: r.get::<_, Option<String>>(12)?.and_then(|j| serde_json::from_str(&j).ok()),
        })
    }

    const NODE_COLS: &'static str = "run_id, node_id, status, attempts, started_at, finished_at, outputs_json, taken_json, error, wait_json, gate_approved, note, loop_json";

    pub fn node_runs(&self, run_id: i64) -> R<Vec<NodeRun>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {} FROM flow_node_runs WHERE run_id=?1 ORDER BY rowid", Self::NODE_COLS)).map_err(db)?;
        let rows = stmt.query_map(params![run_id], Self::node_from).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn node_run(&self, run_id: i64, node_id: &str) -> R<Option<NodeRun>> {
        self.conn()?.query_row(&format!("SELECT {} FROM flow_node_runs WHERE run_id=?1 AND node_id=?2", Self::NODE_COLS), params![run_id, node_id], Self::node_from).optional().map_err(db)
    }

    pub fn save_node(&self, n: &NodeRun) -> R<()> {
        let wait = n.wait.as_ref().map(|w| serde_json::to_string(w).unwrap_or_default());
        let looping = n.looping.as_ref().map(|l| serde_json::to_string(l).unwrap_or_default());
        self.conn()?
            .execute(
                "UPDATE flow_node_runs SET status=?1, attempts=?2, started_at=?3, finished_at=?4, outputs_json=?5, taken_json=?6, error=?7, wait_json=?8, gate_approved=?9, note=?10, loop_json=?13 WHERE run_id=?11 AND node_id=?12",
                params![n.status.as_str(), n.attempts, n.started_at, n.finished_at, Value::Object(n.outputs.clone()).to_string(), serde_json::to_string(&n.taken).unwrap_or_default(), n.error, wait, n.gate_approved as i64, n.note, n.run_id, n.node_id, looping],
            )
            .map_err(db)?;
        Ok(())
    }

    // ---- events -------------------------------------------------------------------------------

    pub fn add_event(&self, run_id: i64, node: Option<&str>, level: Level, kind: &str, message: &str, at: i64) -> R<i64> {
        let c = self.conn()?;
        let message: String = message.chars().take(2000).collect();
        c.execute("INSERT INTO flow_run_events (run_id, at, node_id, level, kind, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![run_id, at, node, level.as_str(), kind, message]).map_err(db)?;
        Ok(c.last_insert_rowid())
    }

    /// Events after `after` (an event id; 0 for all), oldest first. Empty if the run is not this owner's.
    pub fn events(&self, owner: &str, run_id: i64, after: i64, limit: u32) -> R<Vec<RunEvent>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT e.id, e.run_id, e.at, e.node_id, e.level, e.kind, e.message FROM flow_run_events e JOIN flow_runs r ON r.id=e.run_id WHERE r.owner_id=?1 AND e.run_id=?2 AND e.id>?3 ORDER BY e.id LIMIT ?4")
            .map_err(db)?;
        let rows = stmt.query_map(params![owner, run_id, after, limit.clamp(1, 1000)], |r| Ok(RunEvent { id: r.get(0)?, run_id: r.get(1)?, at: r.get(2)?, node_id: r.get(3)?, level: r.get(4)?, kind: r.get(5)?, message: r.get(6)? })).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    // ---- the effects ledger: has this outside action already happened? -----------------------------

    pub fn effect_get(&self, key: &str) -> R<Option<(String, Value)>> {
        let row: Option<(String, String)> = self.conn()?.query_row("SELECT status, result_json FROM flow_effects WHERE idem_key=?1", params![key], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db)?;
        Ok(row.map(|(s, j)| (s, serde_json::from_str(&j).unwrap_or(Value::Null))))
    }

    /// Record that an outside action is about to be tried. Fails if one is already recorded.
    pub fn effect_begin(&self, key: &str, run_id: i64, node_id: &str, at: i64) -> R<bool> {
        let n = self.conn()?.execute("INSERT OR IGNORE INTO flow_effects (idem_key, run_id, node_id, status, at) VALUES (?1, ?2, ?3, 'started', ?4)", params![key, run_id, node_id, at]).map_err(db)?;
        Ok(n > 0)
    }

    pub fn effect_done(&self, key: &str, result: &Value, at: i64) -> R<()> {
        self.conn()?.execute("UPDATE flow_effects SET status='done', result_json=?1, at=?2 WHERE idem_key=?3", params![result.to_string(), at, key]).map_err(db)?;
        Ok(())
    }

    pub fn effect_clear(&self, key: &str) -> R<()> {
        self.conn()?.execute("DELETE FROM flow_effects WHERE idem_key=?1", params![key]).map_err(db)?;
        Ok(())
    }

    // ---- waiting, recovery, the inbox ---------------------------------------------------------------------

    /// Runs waiting only on a time that has come: (owner, run id).
    pub fn due_waits(&self, now: i64) -> R<Vec<(String, i64)>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT r.owner_id, r.id, n.wait_json FROM flow_runs r JOIN flow_node_runs n ON n.run_id=r.id WHERE r.status='waiting' AND n.status='waiting' AND n.wait_json LIKE '%\"until\"%'")
            .map_err(db)?;
        let rows: Vec<(String, i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(db)?.collect::<Result<_, _>>().map_err(db)?;
        let mut out: Vec<(String, i64)> = Vec::new();
        for (owner, id, w) in rows {
            if let Ok(WaitRequest::Until { ts }) = serde_json::from_str::<WaitRequest>(&w) {
                if ts <= now && !out.iter().any(|(_, i)| *i == id) {
                    out.push((owner, id));
                }
            }
        }
        Ok(out)
    }

    /// Runs and steps that were going when Aqua stopped become `interrupted`. Returns how many runs.
    pub fn recover_interrupted(&self, at: i64) -> R<Vec<i64>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT id FROM flow_runs WHERE status IN ('running','queued')").map_err(db)?;
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0)).map_err(db)?.collect::<Result<_, _>>().map_err(db)?;
        for id in &ids {
            c.execute("UPDATE flow_node_runs SET status='interrupted', error='Aqua stopped while this step was running' WHERE run_id=?1 AND status='running'", params![id]).map_err(db)?;
            c.execute("UPDATE flow_runs SET status='interrupted', error='Aqua stopped while this run was going' WHERE id=?1", params![id]).map_err(db)?;
            let _ = self.add_event(*id, None, Level::Warn, "interrupted", "Aqua stopped while this run was going; it can be resumed", at);
        }
        Ok(ids)
    }

    /// Everything waiting for a person, across runs: approvals, questions, failures to decide on.
    pub fn inbox(&self, owner: &str) -> R<Vec<InboxItem>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT r.id, r.flow_id, r.name, r.def_json, n.node_id, n.wait_json, n.started_at, n.finished_at FROM flow_runs r JOIN flow_node_runs n ON n.run_id=r.id WHERE r.owner_id=?1 AND r.status='waiting' AND n.status IN ('waiting','needs_action') ORDER BY r.id DESC, n.rowid")
            .map_err(db)?;
        let rows: Vec<(i64, i64, String, String, String, Option<String>, i64, i64)> = stmt.query_map(params![owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).map_err(db)?.collect::<Result<_, _>>().map_err(db)?;
        let mut out = Vec::new();
        for (run_id, flow_id, flow_name, def_json, node_id, wait, started, finished) in rows {
            let Some(wait) = wait.and_then(|w| serde_json::from_str::<WaitRequest>(&w).ok()) else { continue };
            if matches!(wait, WaitRequest::Until { .. }) {
                continue;
            }
            let node_name = serde_json::from_str::<FlowDef>(&def_json).ok().and_then(|d| d.node(&node_id).map(|n| n.name.clone())).unwrap_or_else(|| node_id.clone());
            out.push(InboxItem { run_id, flow_id, flow_name, node_id, node_name, since: finished.max(started), wait });
        }
        Ok(out)
    }

    /// Keep the newest `keep` finished runs of each flow, and nothing finished before `before`. Runs that are going, waiting,
    /// paused or interrupted are never touched: someone may still need them.
    pub fn prune(&self, keep: u32, before: i64) -> R<usize> {
        let c = self.conn()?;
        let doomed = "SELECT id FROM flow_runs WHERE status IN ('succeeded','failed','cancelled') AND (finished_at<?1 OR id NOT IN (SELECT id FROM flow_runs r2 WHERE r2.flow_id=flow_runs.flow_id AND r2.owner_id=flow_runs.owner_id ORDER BY id DESC LIMIT ?2))";
        c.execute(&format!("DELETE FROM flow_node_runs WHERE run_id IN ({doomed})"), params![before, keep]).map_err(db)?;
        c.execute(&format!("DELETE FROM flow_run_events WHERE run_id IN ({doomed})"), params![before, keep]).map_err(db)?;
        // The ledger lines of a run are only of use while the run can still be resumed.
        c.execute(&format!("DELETE FROM flow_effects WHERE run_id IN ({doomed})"), params![before, keep]).map_err(db)?;
        // A key whose run is cleared away would only point at nothing.
        c.execute(&format!("DELETE FROM flow_run_keys WHERE run_id IN ({doomed})"), params![before, keep]).map_err(db)?;
        c.execute(&format!("DELETE FROM flow_runs WHERE id IN ({doomed})"), params![before, keep]).map_err(db)
    }
}
