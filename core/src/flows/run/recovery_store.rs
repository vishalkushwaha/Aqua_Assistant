//! What Aqua did to recover from a failure, whether it worked, and what it has learned from that.
//!
//! Two tables in the flows database:
//! - `flow_recoveries`: each fix that was applied to a flow (which version it came from and produced, so it
//!   can be undone), and how it turned out.
//! - `flow_recovery_kb`: for each kind of fix and kind of failure, how often it helped. A fix is *trusted*
//!   only after it has worked several times and keeps a high success rate; it is *retired* when it stops working.

use super::store::RunStore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

pub fn init(c: &Connection) -> R<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS flow_recoveries (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             owner_id TEXT NOT NULL,
             flow_id INTEGER NOT NULL,
             run_id INTEGER NOT NULL,
             node_id TEXT NOT NULL,
             class TEXT NOT NULL,
             fix_key TEXT NOT NULL,
             title TEXT NOT NULL,
             kind TEXT NOT NULL,
             version_before INTEGER NOT NULL DEFAULT 0,
             version_after INTEGER NOT NULL DEFAULT 0,
             status TEXT NOT NULL,
             outcome TEXT NOT NULL DEFAULT 'pending',
             created_at INTEGER NOT NULL,
             decided_at INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_flow_recoveries ON flow_recoveries(owner_id, flow_id, id);
         CREATE TABLE IF NOT EXISTS flow_recovery_kb (
             fix_key TEXT NOT NULL,
             class TEXT NOT NULL,
             successes INTEGER NOT NULL DEFAULT 0,
             failures INTEGER NOT NULL DEFAULT 0,
             updated_at INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (fix_key, class)
         );",
    )
    .map_err(db)
}

/// Successes needed before a fix may be applied without asking, and the rate it must keep.
pub const TRUST_SUCCESSES: i64 = 3;
pub const TRUST_CONFIDENCE: f64 = 0.75;
/// Below this rate, after at least two failures, a fix is no longer offered.
pub const RETIRE_CONFIDENCE: f64 = 0.4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Knowledge {
    pub fix_key: String,
    pub class: String,
    pub successes: i64,
    pub failures: i64,
    /// A smoothed success rate: (successes + 1) / (attempts + 2), so one lucky run is not certainty.
    pub confidence: f64,
    /// candidate (still learning) | trusted | retired
    pub state: &'static str,
}

impl Knowledge {
    pub fn new(fix_key: &str, class: &str, successes: i64, failures: i64) -> Self {
        let confidence = (successes as f64 + 1.0) / ((successes + failures) as f64 + 2.0);
        let state = if failures >= 2 && confidence < RETIRE_CONFIDENCE {
            "retired"
        } else if successes >= TRUST_SUCCESSES && confidence >= TRUST_CONFIDENCE {
            "trusted"
        } else {
            "candidate"
        };
        Self { fix_key: fix_key.into(), class: class.into(), successes, failures, confidence, state }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recovery {
    pub id: i64,
    pub flow_id: i64,
    pub run_id: i64,
    pub node_id: String,
    pub class: String,
    pub fix_key: String,
    pub title: String,
    /// temporary | permanent
    pub kind: String,
    pub version_before: i64,
    pub version_after: i64,
    /// applied | rolled_back
    pub status: String,
    /// pending | worked | did_not_help
    pub outcome: String,
    pub created_at: i64,
}

const COLS: &str = "id, flow_id, run_id, node_id, class, fix_key, title, kind, version_before, version_after, status, outcome, created_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Recovery> {
    Ok(Recovery { id: r.get(0)?, flow_id: r.get(1)?, run_id: r.get(2)?, node_id: r.get(3)?, class: r.get(4)?, fix_key: r.get(5)?, title: r.get(6)?, kind: r.get(7)?, version_before: r.get(8)?, version_after: r.get(9)?, status: r.get(10)?, outcome: r.get(11)?, created_at: r.get(12)? })
}

impl RunStore {
    pub fn kb_get(&self, fix_key: &str, class: &str) -> R<Knowledge> {
        let c = self.conn()?;
        let (s, f): (i64, i64) = c
            .query_row("SELECT successes, failures FROM flow_recovery_kb WHERE fix_key=?1 AND class=?2", params![fix_key, class], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(db)?
            .unwrap_or((0, 0));
        Ok(Knowledge::new(fix_key, class, s, f))
    }

    pub fn kb_all(&self) -> R<Vec<Knowledge>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT fix_key, class, successes, failures FROM flow_recovery_kb ORDER BY fix_key, class").map_err(db)?;
        let rows = stmt.query_map([], |r| Ok(Knowledge::new(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?, r.get(2)?, r.get(3)?))).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db);
        rows
    }

    /// Learn from one outcome.
    pub fn kb_record(&self, fix_key: &str, class: &str, worked: bool, at: i64) -> R<Knowledge> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO flow_recovery_kb (fix_key, class, successes, failures, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(fix_key, class) DO UPDATE SET successes = successes + ?3, failures = failures + ?4, updated_at = ?5",
            params![fix_key, class, worked as i64, !worked as i64, at],
        )
        .map_err(db)?;
        drop(c);
        self.kb_get(fix_key, class)
    }

    /// Forget what was learned about one fix (or all of them): it starts again as a candidate. Returns how many
    /// records were removed. The fixes that were applied to flows, and their history, are not touched.
    pub fn kb_reset(&self, fix_key: Option<&str>) -> R<usize> {
        let c = self.conn()?;
        match fix_key {
            Some(k) => c.execute("DELETE FROM flow_recovery_kb WHERE fix_key=?1", params![k]),
            None => c.execute("DELETE FROM flow_recovery_kb", []),
        }
        .map_err(db)
    }

    /// May this fix be used without asking? Only after it has proven itself.
    pub fn kb_trusted(&self, fix_key: &str, class: &str) -> bool {
        self.kb_get(fix_key, class).map(|k| k.state == "trusted").unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn recovery_add(&self, owner: &str, flow_id: i64, run_id: i64, node_id: &str, class: &str, fix_key: &str, title: &str, kind: &str, before: i64, after: i64, at: i64) -> R<i64> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO flow_recoveries (owner_id, flow_id, run_id, node_id, class, fix_key, title, kind, version_before, version_after, status, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'applied',?11)",
            params![owner, flow_id, run_id, node_id, class, fix_key, title, kind, before, after, at],
        )
        .map_err(db)?;
        Ok(c.last_insert_rowid())
    }

    pub fn recovery_get(&self, owner: &str, id: i64) -> R<Option<Recovery>> {
        let c = self.conn()?;
        c.query_row(&format!("SELECT {COLS} FROM flow_recoveries WHERE id=?1 AND owner_id=?2"), params![id, owner], row).optional().map_err(db)
    }

    pub fn recoveries(&self, owner: &str, flow_id: i64, limit: u32) -> R<Vec<Recovery>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {COLS} FROM flow_recoveries WHERE owner_id=?1 AND flow_id=?2 ORDER BY id DESC LIMIT ?3")).map_err(db)?;
        let rows = stmt.query_map(params![owner, flow_id, limit.clamp(1, 200)], row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db);
        rows
    }

    /// Fixes applied to this flow whose result is not known yet.
    pub fn recoveries_pending(&self, owner: &str, flow_id: i64) -> R<Vec<Recovery>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {COLS} FROM flow_recoveries WHERE owner_id=?1 AND flow_id=?2 AND status='applied' AND outcome='pending' ORDER BY id")).map_err(db)?;
        let rows = stmt.query_map(params![owner, flow_id], row).map_err(db)?.collect::<Result<Vec<_>, _>>().map_err(db);
        rows
    }

    pub fn recovery_outcome(&self, id: i64, outcome: &str, at: i64) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE flow_recoveries SET outcome=?1, decided_at=?2 WHERE id=?3", params![outcome, at, id]).map_err(db)?;
        Ok(())
    }

    pub fn recovery_status(&self, id: i64, status: &str, at: i64) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE flow_recoveries SET status=?1, decided_at=?2 WHERE id=?3", params![status, at, id]).map_err(db)?;
        Ok(())
    }
}
