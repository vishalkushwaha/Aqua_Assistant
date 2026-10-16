//! The self-repair store: `data/self_repair.db` (SQLite, WAL), its own file with its own migrations.
//!
//! Two tables. `gaps`: things Aqua could not do and why, one row per gap with a count that grows when it happens again.
//! `proposals`: typed changes that would close a gap, each waiting for a person, with what was done and how to undo it.
//! Every text goes through the secret scrubber before it is stored, and nothing here is ever run or sent to a model.

use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const GAP_STATUSES: [&str; 4] = ["new", "proposed", "fixed", "dismissed"];
pub const PROPOSAL_STATUSES: [&str; 6] = ["pending", "approved", "rejected", "applied", "undone", "failed"];

/// Gaps kept when the table is pruned (dismissed and fixed ones go first), and the most it can ever hold.
pub const KEEP_GAPS: i64 = 500;
const MAX_GAPS: i64 = 1000;

const SUMMARY_CHARS: usize = 240;
const DETAIL_CHARS: usize = 800;
const ASKED_CHARS: usize = 300;
/// The longest text inside a finding (the answer of a research is the longest one).
const FINDING_CHARS: usize = 1_600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairError {
    /// No such gap or proposal.
    NotFound(String),
    /// The request makes no sense now (a second apply, a target that moved): the reason is for the person.
    Refused(String),
    /// The database failed.
    Store(String),
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairError::NotFound(m) | RepairError::Refused(m) | RepairError::Store(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RepairError {}

impl From<rusqlite::Error> for RepairError {
    fn from(e: rusqlite::Error) -> Self {
        RepairError::Store(format!("self repair store: {e}"))
    }
}

pub type R<T> = Result<T, RepairError>;

/// What kind of thing Aqua could not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    PlatformNotBuilt,
    FolderNotAllowed,
    ToolUnavailable,
    CredentialMissing,
    StepNotRunnable,
    /// Aqua does not know how to do what was asked (card R7-08): the only kind that research looks up.
    HowTo,
    /// A saved flow keeps failing (card R7-19, noticed by the watch).
    FlowUnhealthy,
    /// A schedule's last time failed, did not start or was missed (card R7-19, noticed by the watch).
    ScheduleFailed,
    Other,
}

impl GapKind {
    pub const ALL: [GapKind; 9] = [
        GapKind::PlatformNotBuilt,
        GapKind::FolderNotAllowed,
        GapKind::ToolUnavailable,
        GapKind::CredentialMissing,
        GapKind::StepNotRunnable,
        GapKind::HowTo,
        GapKind::FlowUnhealthy,
        GapKind::ScheduleFailed,
        GapKind::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            GapKind::PlatformNotBuilt => "platform_not_built",
            GapKind::FolderNotAllowed => "folder_not_allowed",
            GapKind::ToolUnavailable => "tool_unavailable",
            GapKind::CredentialMissing => "credential_missing",
            GapKind::StepNotRunnable => "step_not_runnable",
            GapKind::HowTo => "how_to",
            GapKind::FlowUnhealthy => "flow_unhealthy",
            GapKind::ScheduleFailed => "schedule_failed",
            GapKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<GapKind> {
        GapKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// A gap as it is recorded. `key` says what makes two reports the same gap (with the kind): the same one again is one row whose `count` grows.
#[derive(Debug, Clone)]
pub struct NewGap {
    pub kind: GapKind,
    pub key: String,
    pub summary: String,
    pub detail: String,
    /// What the person asked for (their own words), when there was a request.
    pub asked: String,
    /// Where it was noticed: `flow_run`, `agent_run`, ...
    pub source: String,
    pub run_id: String,
    pub flow_id: Option<i64>,
    pub node_id: String,
    /// Typed facts a proposal is made from (a platform, a folder). Never text to follow.
    pub data: Value,
}

impl NewGap {
    pub fn new(kind: GapKind, key: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { kind, key: key.into(), summary: summary.into(), detail: String::new(), asked: String::new(), source: String::new(), run_id: String::new(), flow_id: None, node_id: String::new(), data: Value::Object(Default::default()) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gap {
    pub id: i64,
    /// When it was first noticed.
    pub at: i64,
    pub kind: String,
    pub key: String,
    pub summary: String,
    pub detail: String,
    pub asked: String,
    pub source: String,
    pub run_id: String,
    pub flow_id: Option<i64>,
    pub node_id: String,
    pub data: Value,
    /// `new`, `proposed`, `fixed` or `dismissed`.
    pub status: String,
    pub count: i64,
    pub last_at: i64,
    /// What a research found for this gap (card R7-08), or `null`. Text from outside, shown to the person: never an instruction and never the source of a proposal.
    pub finding: Value,
}

/// A proposal as it is made.
#[derive(Debug, Clone)]
pub struct NewProposal {
    pub gap_id: i64,
    pub kind: String,
    pub title: String,
    pub reason: String,
    pub dry_run: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub id: i64,
    pub gap_id: i64,
    pub at: i64,
    /// `allow_folder`, `switch_platform` or `switch_off_step`.
    pub kind: String,
    pub title: String,
    /// One line: why this fix.
    pub reason: String,
    /// What would change, as text, worked out when the proposal was made.
    pub dry_run: String,
    pub payload: Value,
    /// `pending`, `approved` (being applied), `rejected`, `applied`, `undone` or `failed`.
    pub status: String,
    pub approved_by: String,
    pub approved_at: Option<i64>,
    pub applied_at: Option<i64>,
    pub undone_at: Option<i64>,
    /// How to take the change back, recorded when it was applied (`null` before).
    pub undo: Value,
    pub result: Value,
    /// Why it was not applied, when it was not.
    pub error: String,
}

const GAP_COLS: &str = "id, at, kind, gap_key, summary, detail, asked, source, run_id, flow_id, node_id, data, status, count, last_at, finding";
const PROPOSAL_COLS: &str = "id, gap_id, at, kind, title, reason, dry_run, payload, status, approved_by, approved_at, applied_at, undone_at, undo, result, error";

fn json_of(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn gap_from(r: &Row<'_>) -> rusqlite::Result<Gap> {
    Ok(Gap {
        id: r.get(0)?,
        at: r.get(1)?,
        kind: r.get(2)?,
        key: r.get(3)?,
        summary: r.get(4)?,
        detail: r.get(5)?,
        asked: r.get(6)?,
        source: r.get(7)?,
        run_id: r.get(8)?,
        flow_id: r.get(9)?,
        node_id: r.get(10)?,
        data: json_of(r.get(11)?),
        status: r.get(12)?,
        count: r.get(13)?,
        last_at: r.get(14)?,
        finding: r.get::<_, Option<String>>(15)?.map(json_of).unwrap_or(Value::Null),
    })
}

fn proposal_from(r: &Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: r.get(0)?,
        gap_id: r.get(1)?,
        at: r.get(2)?,
        kind: r.get(3)?,
        title: r.get(4)?,
        reason: r.get(5)?,
        dry_run: r.get(6)?,
        payload: json_of(r.get(7)?),
        status: r.get(8)?,
        approved_by: r.get(9)?,
        approved_at: r.get(10)?,
        applied_at: r.get(11)?,
        undone_at: r.get(12)?,
        undo: json_of(r.get(13)?),
        result: json_of(r.get(14)?),
        error: r.get(15)?,
    })
}

/// A text as the store keeps it: secrets scrubbed, then cut to `max` characters.
pub fn clean_text(text: &str, max: usize) -> String {
    let (scrubbed, _) = crate::memory::experience::sanitize::redact(text);
    let t = scrubbed.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!("{}...", t.chars().take(max).collect::<String>().trim_end())
    }
}

fn clean_value(v: &Value) -> Value {
    clean_value_to(v, 400)
}

/// Every text inside a value, scrubbed and cut to `max` characters.
fn clean_value_to(v: &Value, max: usize) -> Value {
    match v {
        Value::String(s) => Value::String(clean_text(s, max)),
        Value::Array(a) => Value::Array(a.iter().map(|x| clean_value_to(x, max)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), clean_value_to(x, max))).collect()),
        other => other.clone(),
    }
}

/// What the facts of a gap become when it happens again: the newest facts, except that a folder the person named stays named and a read-only need grows into a write need.
fn merge_data(kind: &str, old: &Value, new: &Value) -> Value {
    let mut merged = new.clone();
    if kind == GapKind::FolderNotAllowed.as_str() {
        let named = old["named_by_person"].as_bool() == Some(true) || new["named_by_person"].as_bool() == Some(true);
        let rw = old["access"].as_str() == Some("rw") || new["access"].as_str() == Some("rw");
        if let Some(o) = merged.as_object_mut() {
            o.insert("named_by_person".into(), Value::Bool(named));
            o.insert("access".into(), Value::String(if rw { "rw" } else { "ro" }.into()));
            // What the last report knows of who said no (the person, or the guard that never hands a place out) wins; a report that does not know leaves what was known.
            if new["refused_by_guard"].as_bool().is_none() && !old["refused_by_guard"].is_null() {
                o.insert("refused_by_guard".into(), old["refused_by_guard"].clone());
            }
            if new["folder"].as_str().is_none_or(str::is_empty) {
                o.insert("folder".into(), old["folder"].clone());
            }
        }
    }
    merged
}

/// The statements that make the database. Each runs once, in order; `PRAGMA user_version` says how many have. A change to the shape is a new entry at the end, never an edit.
pub(crate) const MIGRATIONS: &[&str] = &["
CREATE TABLE IF NOT EXISTS gaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    gap_key TEXT NOT NULL,
    summary TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    asked TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT '',
    run_id TEXT NOT NULL DEFAULT '',
    flow_id INTEGER,
    node_id TEXT NOT NULL DEFAULT '',
    data TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'new',
    count INTEGER NOT NULL DEFAULT 1,
    last_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_gaps_key ON gaps(kind, gap_key);
CREATE INDEX IF NOT EXISTS idx_gaps_status ON gaps(status, last_at);
CREATE TABLE IF NOT EXISTS proposals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gap_id INTEGER NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    reason TEXT NOT NULL,
    dry_run TEXT NOT NULL DEFAULT '',
    payload TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    approved_by TEXT NOT NULL DEFAULT '',
    approved_at INTEGER,
    applied_at INTEGER,
    undone_at INTEGER,
    undo TEXT NOT NULL DEFAULT 'null',
    result TEXT NOT NULL DEFAULT 'null',
    error TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_proposals_gap ON proposals(gap_id, id);
CREATE INDEX IF NOT EXISTS idx_proposals_status ON proposals(status, at);
", "
ALTER TABLE gaps ADD COLUMN finding TEXT;
"];

pub struct RepairStore {
    path: PathBuf,
}

impl RepairStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let store = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = store.init() {
            tracing::warn!("{e}");
        }
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn opts() -> Opts {
        Opts::standard().busy(10_000)
    }

    /// A connection for one call (the pragmas of a connection; the file was prepared by `init`). A file from a newer build is refused here too.
    fn conn(&self) -> R<Connection> {
        sqlite_store::connect_checked(&self.path, &Self::opts(), MIGRATIONS.len() as u32).map_err(|e| RepairError::Store(format!("self repair store: {e}")))
    }

    /// Makes the file ready: the folder, the check (a damaged file is moved aside), the write-ahead log, and the steps of [`MIGRATIONS`], each in a transaction of its
    /// own with its version bump. A database from a newer build is refused and left alone.
    fn init(&self) -> R<()> {
        let steps: Vec<Migration> = MIGRATIONS.iter().enumerate().map(|(i, sql)| Migration { id: i as u32 + 1, sql }).collect();
        sqlite_store::open_migrated(&self.path, &Self::opts(), &steps).map_err(|e| RepairError::Store(format!("self repair store: {e}")))?;
        Ok(())
    }

    // ---- gaps -------------------------------------------------------------------------------------------

    /// Record that something could not be done. The same gap again (same kind and key) is the same row: its count grows and its newest facts replace the old ones. A dismissed gap
    /// stays dismissed (its count still grows); a fixed one that comes back is open again, because the fix did not hold.
    pub fn record(&self, g: NewGap, at: i64) -> R<Gap> {
        let kind = g.kind.as_str();
        let (summary, detail, asked) = (clean_text(&g.summary, SUMMARY_CHARS), clean_text(&g.detail, DETAIL_CHARS), clean_text(&g.asked, ASKED_CHARS));
        let (source, run_id, node_id) = (clean_text(&g.source, 60), clean_text(&g.run_id, 120), clean_text(&g.node_id, 120));
        let data = clean_value(&g.data);
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<Gap> = tx.query_row(&format!("SELECT {GAP_COLS} FROM gaps WHERE kind=?1 AND gap_key=?2"), params![kind, g.key], gap_from).optional()?;
        let id = match old {
            Some(old) => {
                let status = if old.status == "fixed" { "new" } else { old.status.as_str() };
                let data = merge_data(kind, &old.data, &data);
                let asked = if asked.is_empty() { old.asked.clone() } else { asked };
                let run_id = if run_id.is_empty() { old.run_id.clone() } else { run_id };
                tx.execute(
                    "UPDATE gaps SET count = count + 1, last_at = ?1, summary = ?2, detail = ?3, asked = ?4, source = ?5, run_id = ?6, data = ?7, status = ?8 WHERE id = ?9",
                    params![at, summary, detail, asked, source, run_id, data.to_string(), status, old.id],
                )?;
                old.id
            }
            None => {
                tx.execute(
                    "INSERT INTO gaps (at, kind, gap_key, summary, detail, asked, source, run_id, flow_id, node_id, data, status, count, last_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'new', 1, ?1)",
                    params![at, kind, g.key, summary, detail, asked, source, run_id, g.flow_id, node_id, data.to_string()],
                )?;
                tx.last_insert_rowid()
            }
        };
        prune(&tx)?;
        let gap = tx.query_row(&format!("SELECT {GAP_COLS} FROM gaps WHERE id=?1"), params![id], gap_from)?;
        tx.commit()?;
        Ok(gap)
    }

    pub fn gap(&self, id: i64) -> R<Option<Gap>> {
        Ok(self.conn()?.query_row(&format!("SELECT {GAP_COLS} FROM gaps WHERE id=?1"), params![id], gap_from).optional()?)
    }

    /// The gaps, newest first. `status` is one of the four statuses, or `open` for new and proposed ones; `None` is all of them.
    pub fn gaps(&self, status: Option<&str>, limit: u32) -> R<Vec<Gap>> {
        let c = self.conn()?;
        let limit = limit.clamp(1, 500);
        let (filter, arg): (&str, Option<&str>) = match status {
            None => ("", None),
            Some("open") => (" WHERE status IN ('new','proposed')", None),
            Some(s) if GAP_STATUSES.contains(&s) => (" WHERE status = ?2", Some(s)),
            Some(other) => return Err(RepairError::Refused(format!("\"{other}\" is not a status of a gap (new, proposed, fixed, dismissed or open)"))),
        };
        let sql = format!("SELECT {GAP_COLS} FROM gaps{filter} ORDER BY last_at DESC, id DESC LIMIT ?1");
        let mut stmt = c.prepare(&sql)?;
        let rows = match arg {
            Some(s) => stmt.query_map(params![limit, s], gap_from)?.collect::<Result<Vec<_>, _>>()?,
            None => stmt.query_map(params![limit], gap_from)?.collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// The gaps that happened (first or again) at or after `since`.
    pub fn gaps_since(&self, since: i64) -> R<Vec<Gap>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {GAP_COLS} FROM gaps WHERE last_at >= ?1 ORDER BY last_at DESC, id DESC LIMIT 500"))?;
        let rows = stmt.query_map(params![since], gap_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_gap_status(&self, id: i64, status: &str) -> R<()> {
        if !GAP_STATUSES.contains(&status) {
            return Err(RepairError::Refused(format!("\"{status}\" is not a status of a gap")));
        }
        self.conn()?.execute("UPDATE gaps SET status=?1 WHERE id=?2", params![status, id])?;
        Ok(())
    }

    /// Attach what a research found to a gap (card R7-08), replacing an earlier finding. Every text in it is scrubbed and cut; the finding is kept when the gap happens again.
    pub fn set_finding(&self, id: i64, finding: &Value) -> R<()> {
        let cleaned = clean_value_to(finding, FINDING_CHARS);
        let n = self.conn()?.execute("UPDATE gaps SET finding=?1 WHERE id=?2", params![cleaned.to_string(), id])?;
        if n == 0 {
            return Err(RepairError::NotFound("no such gap".into()));
        }
        Ok(())
    }

    /// Work the status of a gap out from its proposals: fixed while one is applied, proposed while one waits, else new. A dismissed gap stays dismissed.
    pub fn refresh_gap_status(&self, gap_id: i64) -> R<()> {
        let c = self.conn()?;
        let status: Option<String> = c.query_row("SELECT status FROM gaps WHERE id=?1", params![gap_id], |r| r.get(0)).optional()?;
        let Some(status) = status else { return Ok(()) };
        if status == "dismissed" {
            return Ok(());
        }
        let n = |s: &str| -> R<i64> { Ok(c.query_row("SELECT COUNT(*) FROM proposals WHERE gap_id=?1 AND status=?2", params![gap_id, s], |r| r.get(0))?) };
        let next = if n("applied")? > 0 {
            "fixed"
        } else if n("pending")? > 0 || n("approved")? > 0 {
            "proposed"
        } else {
            "new"
        };
        if next != status {
            c.execute("UPDATE gaps SET status=?1 WHERE id=?2", params![next, gap_id])?;
        }
        Ok(())
    }

    // ---- proposals --------------------------------------------------------------------------------------

    pub fn proposal_add(&self, p: NewProposal, at: i64) -> R<Proposal> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO proposals (gap_id, at, kind, title, reason, dry_run, payload, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
            params![p.gap_id, at, p.kind, clean_text(&p.title, SUMMARY_CHARS), clean_text(&p.reason, SUMMARY_CHARS), clean_text(&p.dry_run, DETAIL_CHARS * 2), p.payload.to_string()],
        )?;
        let id = c.last_insert_rowid();
        Ok(c.query_row(&format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE id=?1"), params![id], proposal_from)?)
    }

    pub fn proposal(&self, id: i64) -> R<Option<Proposal>> {
        Ok(self.conn()?.query_row(&format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE id=?1"), params![id], proposal_from).optional()?)
    }

    /// The proposals, newest first, of one status and/or one gap.
    pub fn proposals(&self, status: Option<&str>, gap_id: Option<i64>, limit: u32) -> R<Vec<Proposal>> {
        if let Some(s) = status {
            if !PROPOSAL_STATUSES.contains(&s) {
                return Err(RepairError::Refused(format!("\"{s}\" is not a status of a proposal (pending, approved, rejected, applied, undone or failed)")));
            }
        }
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR gap_id = ?2) ORDER BY id DESC LIMIT ?3"))?;
        let rows = stmt.query_map(params![status, gap_id, limit.clamp(1, 500)], proposal_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The proposals that were made, approved, applied or undone at or after `since`.
    pub fn proposals_since(&self, since: i64) -> R<Vec<Proposal>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {PROPOSAL_COLS} FROM proposals WHERE at >= ?1 OR COALESCE(approved_at, 0) >= ?1 OR COALESCE(applied_at, 0) >= ?1 OR COALESCE(undone_at, 0) >= ?1 ORDER BY id DESC LIMIT 500"
        ))?;
        let rows = stmt.query_map(params![since], proposal_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Take a pending proposal for approval. True for exactly one caller: a second approval of the same proposal (a double click, two windows) finds it taken.
    pub fn proposal_claim(&self, id: i64, by: &str, at: i64) -> R<bool> {
        let n = self.conn()?.execute("UPDATE proposals SET status='approved', approved_by=?1, approved_at=?2 WHERE id=?3 AND status='pending'", params![clean_text(by, 40), at, id])?;
        Ok(n == 1)
    }

    pub fn proposal_applied(&self, id: i64, at: i64, undo: &Value, result: &Value) -> R<()> {
        self.conn()?.execute("UPDATE proposals SET status='applied', applied_at=?1, undo=?2, result=?3, error='' WHERE id=?4 AND status='approved'", params![at, undo.to_string(), clean_value(result).to_string(), id])?;
        Ok(())
    }

    /// An approved proposal that could not be applied: nothing was changed, and the reason is kept.
    pub fn proposal_failed(&self, id: i64, reason: &str) -> R<()> {
        self.conn()?.execute("UPDATE proposals SET status='failed', error=?1 WHERE id=?2 AND status='approved'", params![clean_text(reason, DETAIL_CHARS), id])?;
        Ok(())
    }

    /// A pending proposal the person said no to (or that became pointless). True if it was pending.
    pub fn proposal_reject(&self, id: i64, why: &str) -> R<bool> {
        let n = self.conn()?.execute("UPDATE proposals SET status='rejected', error=?1 WHERE id=?2 AND status='pending'", params![clean_text(why, 200), id])?;
        Ok(n == 1)
    }

    pub fn proposal_undone(&self, id: i64, at: i64, result: &Value) -> R<()> {
        self.conn()?.execute("UPDATE proposals SET status='undone', undone_at=?1, result=?2 WHERE id=?3 AND status='applied'", params![at, clean_value(result).to_string(), id])?;
        Ok(())
    }
}

/// Keep the table small: past `KEEP_GAPS` the oldest dismissed and fixed gaps go; past `MAX_GAPS` the oldest of any. Proposals of a gap that is gone go with it unless they still matter.
fn prune(c: &Connection) -> R<()> {
    let count = |c: &Connection| -> R<i64> { Ok(c.query_row("SELECT COUNT(*) FROM gaps", [], |r| r.get(0))?) };
    let n = count(c)?;
    if n <= KEEP_GAPS {
        return Ok(());
    }
    c.execute("DELETE FROM gaps WHERE id IN (SELECT id FROM gaps WHERE status IN ('dismissed','fixed') ORDER BY last_at ASC, id ASC LIMIT ?1)", params![n - KEEP_GAPS])?;
    let n = count(c)?;
    if n > MAX_GAPS {
        c.execute("DELETE FROM gaps WHERE id IN (SELECT id FROM gaps ORDER BY last_at ASC, id ASC LIMIT ?1)", params![n - MAX_GAPS])?;
    }
    c.execute("DELETE FROM proposals WHERE gap_id NOT IN (SELECT id FROM gaps) AND status NOT IN ('pending','approved','applied')", [])?;
    Ok(())
}
