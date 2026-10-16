//! Flows, their version history and their audit trail: `data/flows.db` (SQLite, WAL).
//!
//! Every query is scoped by `owner_id`. Saving never needs a valid flow (a draft on the canvas is
//! often half built); running does, and that is the validator's job. A save that changes nothing does
//! not create a version, and a save made from an older version than the current one is refused, so an
//! editor that has gone stale cannot silently overwrite newer work.

use super::diff::diff;
use super::model::*;
use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

fn db(e: rusqlite::Error) -> String {
    format!("flow store: {e}")
}

/// `data/flows.db` is one file shared by five stores (flows, runs, schedules, notifications, comment drafts). Each makes its own tables when it starts (`IF NOT EXISTS`, and a
/// probe that adds a column to a database that lacks it), so the shape is not a list of SQL steps yet: version 1 is "the shape those five make", and the step is empty. What the
/// version gives the file now is the guard (a file from a newer build is refused, call by call) and the copy kept before a later step. A change that the idempotent
/// start-up cannot express is a step 2 here.
pub(crate) const DB_STEPS: &[Migration] = &[Migration { id: 1, sql: "" }];

pub(crate) fn db_opts() -> Opts {
    Opts::standard().busy(10_000)
}

/// A connection for one call, for all five stores: the folder is made, the pragmas are set, and a file from a newer build is refused.
pub(crate) fn conn_at(path: &Path) -> R<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    sqlite_store::connect_checked(path, &db_opts(), sqlite_store::newest(DB_STEPS)).map_err(|e| format!("flow store: {e}"))
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The newest versions kept per flow (older ones are pruned; the current one never is).
pub const KEEP_VERSIONS: i64 = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FlowSummary {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub version: i64,
    pub node_count: usize,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VersionInfo {
    pub version: i64,
    pub author: String,
    pub note: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub flow_id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

pub struct FlowStore {
    path: PathBuf,
}

fn to_json<T: Serialize>(v: &T) -> R<String> {
    serde_json::to_string(v).map_err(|e| format!("flow store: {e}"))
}

fn from_json(s: &str) -> R<FlowDef> {
    serde_json::from_str(s).map_err(|e| format!("flow store: a stored flow could not be read: {e}"))
}

impl FlowStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let store = Self { path: path.as_ref().to_path_buf() };
        if let Err(e) = store.init() {
            tracing::warn!("{e}");
        }
        store
    }

    fn conn(&self) -> R<Connection> {
        conn_at(&self.path)
    }

    fn init(&self) -> R<()> {
        // The folder, the check (a damaged file is moved aside and a new one made), the write-ahead log and the version; then the tables of the five stores.
        let c = sqlite_store::open_migrated(&self.path, &db_opts(), DB_STEPS).map_err(|e| format!("flow store: {e}"))?.conn;
        super::run::store::init_schema(&c)?;
        super::sched::store::init_schema(&c)?;
        super::comments_inbox::init_schema(&c)?;
        super::notify::init_schema(&c)?;
        c.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS flows (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     owner_id TEXT NOT NULL,
                     current_version INTEGER NOT NULL,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_flows_owner ON flows(owner_id);
                 CREATE TABLE IF NOT EXISTS flow_versions (
                     flow_id INTEGER NOT NULL,
                     version INTEGER NOT NULL,
                     def_json TEXT NOT NULL,
                     author TEXT NOT NULL,
                     note TEXT NOT NULL DEFAULT '',
                     created_at INTEGER NOT NULL,
                     PRIMARY KEY (flow_id, version)
                 );
                 CREATE TABLE IF NOT EXISTS flow_audit (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     owner_id TEXT NOT NULL,
                     flow_id INTEGER NOT NULL,
                     at INTEGER NOT NULL,
                     actor TEXT NOT NULL,
                     action TEXT NOT NULL,
                     detail TEXT NOT NULL DEFAULT ''
                 );
                 CREATE INDEX IF NOT EXISTS idx_audit_flow ON flow_audit(owner_id, flow_id, id);
                 CREATE TABLE IF NOT EXISTS flow_chat (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     owner_id TEXT NOT NULL,
                     flow_id INTEGER NOT NULL,
                     at INTEGER NOT NULL,
                     role TEXT NOT NULL,
                     text TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_chat_flow ON flow_chat(owner_id, flow_id, id);",
            )
            .map_err(db)
    }

    fn audit_in(c: &Connection, owner: &str, flow_id: i64, actor: &str, action: &str, detail: &str, at: i64) -> R<()> {
        c.execute("INSERT INTO flow_audit (owner_id, flow_id, at, actor, action, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![owner, flow_id, at, actor, action, detail]).map_err(db)?;
        Ok(())
    }

    /// Create a flow (version 1). `actor` is "user" or "ai".
    pub fn create(&self, owner: &str, mut def: FlowDef, actor: &str, note: &str, at: i64) -> R<Flow> {
        def.normalize();
        def.check_size()?;
        if def.name.is_empty() {
            return Err("the flow needs a name".into());
        }
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(db)?;
        tx.execute("INSERT INTO flows (owner_id, current_version, created_at, updated_at) VALUES (?1, 1, ?2, ?2)", params![owner, at]).map_err(db)?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO flow_versions (flow_id, version, def_json, author, note, created_at) VALUES (?1, 1, ?2, ?3, ?4, ?5)", params![id, to_json(&def)?, actor, note, at]).map_err(db)?;
        Self::audit_in(&tx, owner, id, actor, "create", &def.name, at)?;
        tx.commit().map_err(db)?;
        self.get(owner, id)?.ok_or_else(|| "flow store: the new flow was not found".to_string())
    }

    fn load(c: &Connection, owner: &str, id: i64) -> R<Option<Flow>> {
        let row: Option<(i64, i64, i64, i64)> = c
            .query_row("SELECT id, current_version, created_at, updated_at FROM flows WHERE id=?1 AND owner_id=?2", params![id, owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()
            .map_err(db)?;
        let Some((id, version, created_at, updated_at)) = row else { return Ok(None) };
        let json: String = c.query_row("SELECT def_json FROM flow_versions WHERE flow_id=?1 AND version=?2", params![id, version], |r| r.get(0)).map_err(db)?;
        Ok(Some(Flow { id, owner_id: owner.to_string(), version, created_at, updated_at, def: from_json(&json)? }))
    }

    pub fn get(&self, owner: &str, id: i64) -> R<Option<Flow>> {
        Self::load(&self.conn()?, owner, id)
    }

    pub fn list(&self, owner: &str) -> R<Vec<FlowSummary>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT id FROM flows WHERE owner_id=?1 ORDER BY updated_at DESC, id DESC").map_err(db)?;
        let ids: Vec<i64> = stmt.query_map(params![owner], |r| r.get(0)).map_err(db)?.collect::<Result<_, _>>().map_err(db)?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(f) = Self::load(&c, owner, id)? {
                out.push(FlowSummary { id, name: f.def.name.clone(), description: f.def.description.clone(), version: f.version, node_count: f.def.nodes.len(), updated_at: f.updated_at });
            }
        }
        Ok(out)
    }

    /// Save a new definition. `expected_version` is the version the editor started from; if the flow
    /// has moved on, the save is refused. A save that changes nothing (not even a node position)
    /// returns the flow as it is. A save that only moves nodes updates the current version in place
    /// (positions are not history).
    pub fn update(&self, owner: &str, id: i64, mut def: FlowDef, actor: &str, note: &str, expected_version: Option<i64>, at: i64) -> R<Option<Flow>> {
        def.normalize();
        def.check_size()?;
        if def.name.is_empty() {
            return Err("the flow needs a name".into());
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let Some(current) = Self::load(&tx, owner, id)? else { return Ok(None) };
        if expected_version.is_some_and(|v| v != current.version) {
            return Err(format!("conflict: this flow is now at version {} but you edited version {}; reload it and apply your change again", current.version, expected_version.unwrap_or(0)));
        }
        let d = diff(&current.def, &def);
        if d.is_empty() && d.nodes_moved.is_empty() {
            return Ok(Some(current));
        }
        if d.only_moves() {
            tx.execute("UPDATE flow_versions SET def_json=?1 WHERE flow_id=?2 AND version=?3", params![to_json(&def)?, id, current.version]).map_err(db)?;
            tx.commit().map_err(db)?;
            return self.get(owner, id);
        }
        let next = current.version + 1;
        tx.execute("INSERT INTO flow_versions (flow_id, version, def_json, author, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, next, to_json(&def)?, actor, note, at]).map_err(db)?;
        tx.execute("UPDATE flows SET current_version=?1, updated_at=?2 WHERE id=?3 AND owner_id=?4", params![next, at, id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_versions WHERE flow_id=?1 AND version <= ?2 - ?3", params![id, next, KEEP_VERSIONS]).map_err(db)?;
        let detail = if note.is_empty() { d.summary.join("; ") } else { format!("{note}: {}", d.summary.join("; ")) };
        Self::audit_in(&tx, owner, id, actor, "update", &detail.chars().take(500).collect::<String>(), at)?;
        tx.commit().map_err(db)?;
        self.get(owner, id)
    }

    pub fn versions(&self, owner: &str, id: i64) -> R<Vec<VersionInfo>> {
        let c = self.conn()?;
        if Self::load(&c, owner, id)?.is_none() {
            return Ok(Vec::new());
        }
        let mut stmt = c.prepare("SELECT version, author, note, created_at FROM flow_versions WHERE flow_id=?1 ORDER BY version DESC").map_err(db)?;
        let rows = stmt.query_map(params![id], |r| Ok(VersionInfo { version: r.get(0)?, author: r.get(1)?, note: r.get(2)?, created_at: r.get(3)? })).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }

    pub fn version(&self, owner: &str, id: i64, version: i64) -> R<Option<FlowDef>> {
        let c = self.conn()?;
        if Self::load(&c, owner, id)?.is_none() {
            return Ok(None);
        }
        let json: Option<String> = c.query_row("SELECT def_json FROM flow_versions WHERE flow_id=?1 AND version=?2", params![id, version], |r| r.get(0)).optional().map_err(db)?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Make an old version current again. This is a new version (so the restore can itself be undone).
    pub fn restore(&self, owner: &str, id: i64, version: i64, actor: &str, at: i64) -> R<Option<Flow>> {
        let Some(old) = self.version(owner, id, version)? else { return Ok(None) };
        let note = format!("restored version {version}");
        self.update(owner, id, old, actor, &note, None, at)
    }

    pub fn delete(&self, owner: &str, id: i64, actor: &str, at: i64) -> R<bool> {
        let mut c = self.conn()?;
        // Take the write lock first: this reads before it writes, and two connections that each hold a
        // read lock and want to write deadlock (the busy timeout does not help with that).
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        let name = Self::load(&tx, owner, id)?.map(|f| f.def.name);
        let Some(name) = name else { return Ok(false) };
        tx.execute("DELETE FROM flow_versions WHERE flow_id=?1", params![id]).map_err(db)?;
        tx.execute("DELETE FROM flow_chat WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        let runs = "SELECT id FROM flow_runs WHERE flow_id=?1 AND owner_id=?2";
        for table in ["flow_node_runs", "flow_run_events", "flow_effects", "flow_run_keys"] {
            tx.execute(&format!("DELETE FROM {table} WHERE run_id IN ({runs})"), params![id, owner]).map_err(db)?;
        }
        tx.execute("DELETE FROM flow_recoveries WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_comment_drafts WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_runs WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_schedule_occurrences WHERE owner_id=?2 AND schedule_id IN (SELECT id FROM flow_schedules WHERE flow_id=?1 AND owner_id=?2)", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_schedules WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flow_notifications WHERE flow_id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        tx.execute("DELETE FROM flows WHERE id=?1 AND owner_id=?2", params![id, owner]).map_err(db)?;
        Self::audit_in(&tx, owner, id, actor, "delete", &name, at)?;
        tx.commit().map_err(db)?;
        Ok(true)
    }

    /// Record something that happened to a flow (used by later phases: runs, approvals, AI edits).
    pub fn audit(&self, owner: &str, flow_id: i64, actor: &str, action: &str, detail: &str, at: i64) -> R<()> {
        Self::audit_in(&self.conn()?, owner, flow_id, actor, action, detail, at)
    }

    pub fn audit_log(&self, owner: &str, flow_id: Option<i64>, limit: u32) -> R<Vec<AuditEntry>> {
        let c = self.conn()?;
        let map = |r: &rusqlite::Row<'_>| Ok(AuditEntry { id: r.get(0)?, flow_id: r.get(1)?, at: r.get(2)?, actor: r.get(3)?, action: r.get(4)?, detail: r.get(5)? });
        let limit = limit.clamp(1, 500);
        match flow_id {
            Some(f) => c
                .prepare("SELECT id, flow_id, at, actor, action, detail FROM flow_audit WHERE owner_id=?1 AND flow_id=?2 ORDER BY id DESC LIMIT ?3")
                .map_err(db)?
                .query_map(params![owner, f, limit], map)
                .map_err(db)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db),
            None => c
                .prepare("SELECT id, flow_id, at, actor, action, detail FROM flow_audit WHERE owner_id=?1 ORDER BY id DESC LIMIT ?2")
                .map_err(db)?
                .query_map(params![owner, limit], map)
                .map_err(db)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db),
        }
    }

    // ---- the conversation with the assistant, kept per flow so a change can build on the last one ----

    /// Add a message ("user" or "assistant"). Only the newest 200 per flow are kept.
    pub fn chat_add(&self, owner: &str, flow_id: i64, role: &str, text: &str, at: i64) -> R<()> {
        let c = self.conn()?;
        if Self::load(&c, owner, flow_id)?.is_none() {
            return Ok(());
        }
        let text: String = text.chars().take(2000).collect();
        c.execute("INSERT INTO flow_chat (owner_id, flow_id, at, role, text) VALUES (?1, ?2, ?3, ?4, ?5)", params![owner, flow_id, at, role, text]).map_err(db)?;
        c.execute("DELETE FROM flow_chat WHERE owner_id=?1 AND flow_id=?2 AND id NOT IN (SELECT id FROM flow_chat WHERE owner_id=?1 AND flow_id=?2 ORDER BY id DESC LIMIT 200)", params![owner, flow_id]).map_err(db)?;
        Ok(())
    }

    /// The newest `limit` messages, oldest first.
    pub fn chat(&self, owner: &str, flow_id: i64, limit: u32) -> R<Vec<ChatMessage>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT id, at, role, text FROM (SELECT id, at, role, text FROM flow_chat WHERE owner_id=?1 AND flow_id=?2 ORDER BY id DESC LIMIT ?3) ORDER BY id").map_err(db)?;
        let rows = stmt.query_map(params![owner, flow_id, limit.clamp(1, 200)], |r| Ok(ChatMessage { id: r.get(0)?, at: r.get(1)?, role: r.get(2)?, text: r.get(3)? })).map_err(db)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatMessage {
    pub id: i64,
    pub at: i64,
    pub role: String,
    pub text: String,
}
