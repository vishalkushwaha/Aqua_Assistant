//! SQLite store for experiences, usage history, learning jobs, events, counters and settings.
//!
//! One file, `data/experience.db`. It is the source of truth for *what an experience is and how it
//! has performed*; the vector index only answers "which ones are similar". Every method opens its own
//! connection (like `SolutionsDb`), so the store is cheap to clone into a worker thread.

use super::model::{now_secs, Experience, ExperienceScope, ExperienceStatus, ExperienceType, VerificationMethod};
use super::settings::LearningSettings;
use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type R<T> = Result<T, String>;

fn e2s<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`).
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS agent_experiences (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT NOT NULL, project_key TEXT NOT NULL DEFAULT '', owner_id TEXT NOT NULL DEFAULT '',
                 task_type TEXT NOT NULL DEFAULT '', experience_type TEXT NOT NULL,
                 problem_summary TEXT NOT NULL, context_summary TEXT NOT NULL DEFAULT '', approach TEXT NOT NULL DEFAULT '',
                 important_steps TEXT NOT NULL DEFAULT '[]', tools_used TEXT NOT NULL DEFAULT '[]',
                 constraints TEXT NOT NULL DEFAULT '[]', technologies TEXT NOT NULL DEFAULT '[]',
                 solution_summary TEXT NOT NULL DEFAULT '', verification_method TEXT NOT NULL DEFAULT 'none',
                 verification_result TEXT NOT NULL DEFAULT '', verified INTEGER NOT NULL DEFAULT 0,
                 result TEXT NOT NULL DEFAULT '', success INTEGER NOT NULL DEFAULT 0,
                 failed_approach TEXT, failure_reason TEXT, correction TEXT,
                 confidence REAL NOT NULL DEFAULT 0.2, quality_score REAL NOT NULL DEFAULT 0.0,
                 status TEXT NOT NULL, human_approved INTEGER NOT NULL DEFAULT 0,
                 embedding_model TEXT NOT NULL DEFAULT '', embedding_version INTEGER NOT NULL DEFAULT 0,
                 embedding BLOB,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 last_verified_at INTEGER NOT NULL DEFAULT 0, last_used_at INTEGER NOT NULL DEFAULT 0,
                 retrieval_count INTEGER NOT NULL DEFAULT 0, reuse_count INTEGER NOT NULL DEFAULT 0,
                 successful_reuse_count INTEGER NOT NULL DEFAULT 0, failed_reuse_count INTEGER NOT NULL DEFAULT 0,
                 source_task_id TEXT NOT NULL DEFAULT '', project_version TEXT NOT NULL DEFAULT '',
                 experience_version INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS idx_exp_status ON agent_experiences(status);
             CREATE INDEX IF NOT EXISTS idx_exp_project ON agent_experiences(project_key);
             CREATE INDEX IF NOT EXISTS idx_exp_owner ON agent_experiences(owner_id);
             CREATE INDEX IF NOT EXISTS idx_exp_task_type ON agent_experiences(task_type);
             CREATE INDEX IF NOT EXISTS idx_exp_created ON agent_experiences(created_at);
             CREATE INDEX IF NOT EXISTS idx_exp_updated ON agent_experiences(updated_at);
             CREATE INDEX IF NOT EXISTS idx_exp_last_used ON agent_experiences(last_used_at);
             CREATE TABLE IF NOT EXISTS agent_experience_usage (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, experience_id INTEGER NOT NULL,
                 used_at INTEGER NOT NULL, task_summary TEXT NOT NULL DEFAULT '',
                 similarity REAL NOT NULL DEFAULT 0, success INTEGER, note TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_usage_exp ON agent_experience_usage(experience_id);
             CREATE TABLE IF NOT EXISTS agent_learning_jobs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, payload TEXT NOT NULL,
                 status TEXT NOT NULL DEFAULT 'queued', attempts INTEGER NOT NULL DEFAULT 0,
                 max_attempts INTEGER NOT NULL DEFAULT 3, next_run_at INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_jobs_status ON agent_learning_jobs(status, next_run_at);
             CREATE TABLE IF NOT EXISTS agent_learning_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, event TEXT NOT NULL,
                 detail TEXT NOT NULL DEFAULT '{}', job_id INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_events_ts ON agent_learning_events(ts);
             CREATE TABLE IF NOT EXISTS agent_practice_tasks (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, family TEXT NOT NULL, source TEXT NOT NULL,
                 status TEXT NOT NULL DEFAULT 'running', created_at INTEGER NOT NULL,
                 started_at INTEGER NOT NULL DEFAULT 0, finished_at INTEGER NOT NULL DEFAULT 0,
                 verified INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '', steps INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_practice_created ON agent_practice_tasks(created_at);
             CREATE TABLE IF NOT EXISTS agent_learning_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS agent_learning_counters (name TEXT PRIMARY KEY, value REAL NOT NULL DEFAULT 0);",
}];

#[derive(Debug, Clone)]
pub struct ExperienceStore {
    path: PathBuf,
}

/// Who is asking. Decides which experiences may be seen.
#[derive(Debug, Clone, Default)]
pub struct Viewer {
    pub project_key: String,
    pub owner_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub status: Option<ExperienceStatus>,
    pub experience_type: Option<ExperienceType>,
    pub project_key: Option<String>,
    pub text: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRow {
    pub id: i64,
    pub experience_id: i64,
    pub used_at: i64,
    pub task_summary: String,
    pub similarity: f32,
    /// `None` until the outcome is known.
    pub success: Option<bool>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRow {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
    pub status: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_run_at: i64,
    pub last_error: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PracticeRow {
    pub id: i64,
    pub goal: String,
    pub family: String,
    /// `variation` (a new task in a kind you use) or `paraphrase` (a past task, reworded).
    pub source: String,
    pub status: String,
    pub created_at: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub verified: bool,
    pub note: String,
    pub steps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub event: String,
    pub detail: Value,
    pub job_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningStats {
    pub tasks_analyzed: u64,
    pub tasks_successful: u64,
    pub tasks_failed: u64,
    pub experiences_created: u64,
    pub experiences_updated: u64,
    pub experiences_rejected: u64,
    pub experiences_deprecated: u64,
    pub experiences_reused: u64,
    pub successful_reuses: u64,
    pub failed_reuses: u64,
    pub retrievals: u64,
    pub retrieval_hits: u64,
    pub retrieval_hit_rate: f32,
    pub average_similarity: f32,
    pub average_quality: f32,
    pub duplicates_prevented: u64,
    pub total_experiences: u64,
    pub by_status: BTreeMap<String, u64>,
    pub jobs: BTreeMap<String, u64>,
    pub last_learning_at: i64,
    pub last_success_at: i64,
    pub practice_runs: u64,
    pub practice_verified: u64,
}

/// Runs with earlier experience offered to the planner against runs without it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Evaluation {
    pub runs_with_memory: u64,
    pub runs_without_memory: u64,
    pub success_rate_with_memory: f32,
    pub success_rate_without_memory: f32,
    pub avg_execution_ms_with_memory: u64,
    pub avg_execution_ms_without_memory: u64,
    pub avg_tool_calls_with_memory: f32,
    pub avg_tool_calls_without_memory: f32,
    pub avg_tokens_with_memory: u64,
    pub avg_tokens_without_memory: u64,
    /// Runs that had memory offered and still could not be verified.
    pub memory_related_failures: u64,
    pub retrieval_hit_rate: f32,
    pub reuse_success_rate: f32,
    pub note: String,
}

const COLS: &str ="id, scope, project_key, owner_id, task_type, experience_type, problem_summary, context_summary, approach, \
    important_steps, tools_used, constraints, technologies, solution_summary, verification_method, verification_result, verified, \
    result, success, failed_approach, failure_reason, correction, confidence, quality_score, status, human_approved, \
    embedding_model, embedding_version, created_at, updated_at, last_verified_at, last_used_at, retrieval_count, reuse_count, \
    successful_reuse_count, failed_reuse_count, source_task_id, project_version, experience_version";

fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn strings(row: &Row, idx: usize) -> rusqlite::Result<Vec<String>> {
    let text: String = row.get(idx)?;
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

fn row_to_experience(row: &Row) -> rusqlite::Result<Experience> {
    Ok(Experience {
        id: row.get(0)?,
        scope: ExperienceScope::parse(&row.get::<_, String>(1)?).unwrap_or(ExperienceScope::Global),
        project_key: row.get(2)?,
        owner_id: row.get(3)?,
        task_type: row.get(4)?,
        experience_type: ExperienceType::parse(&row.get::<_, String>(5)?).unwrap_or(ExperienceType::Solution),
        problem_summary: row.get(6)?,
        context_summary: row.get(7)?,
        approach: row.get(8)?,
        important_steps: strings(row, 9)?,
        tools_used: strings(row, 10)?,
        constraints: strings(row, 11)?,
        technologies: strings(row, 12)?,
        solution_summary: row.get(13)?,
        verification_method: VerificationMethod::parse(&row.get::<_, String>(14)?),
        verification_result: row.get(15)?,
        verified: row.get::<_, i64>(16)? != 0,
        result: row.get(17)?,
        success: row.get::<_, i64>(18)? != 0,
        failed_approach: row.get(19)?,
        failure_reason: row.get(20)?,
        correction: row.get(21)?,
        confidence: row.get::<_, f64>(22)? as f32,
        quality_score: row.get::<_, f64>(23)? as f32,
        status: ExperienceStatus::parse(&row.get::<_, String>(24)?).unwrap_or(ExperienceStatus::Candidate),
        human_approved: row.get::<_, i64>(25)? != 0,
        embedding_model: row.get(26)?,
        embedding_version: row.get::<_, i64>(27)? as u32,
        created_at: row.get(28)?,
        updated_at: row.get(29)?,
        last_verified_at: row.get(30)?,
        last_used_at: row.get(31)?,
        retrieval_count: row.get::<_, i64>(32)? as u32,
        reuse_count: row.get::<_, i64>(33)? as u32,
        successful_reuse_count: row.get::<_, i64>(34)? as u32,
        failed_reuse_count: row.get::<_, i64>(35)? as u32,
        source_task_id: row.get(36)?,
        project_version: row.get(37)?,
        experience_version: row.get::<_, i64>(38)? as u32,
    })
}

pub fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_embedding(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

impl ExperienceStore {
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let store = Self { path: base_dir.as_ref().join("data").join("experience.db") };
        if let Err(e) = store.init() {
            tracing::warn!("experience store init failed: {e}");
        }
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Deleted text (an experience, a task summary) is overwritten, not left in free pages. The setting belongs to the connection, so every connection sets it.
    fn opts() -> Opts {
        Opts::standard().secure_delete(true)
    }

    pub(super) fn conn(&self) -> R<Connection> {
        sqlite_store::connect_checked(&self.path, &Self::opts(), sqlite_store::newest(STEPS)).map_err(e2s("open experience db"))
    }

    /// The folder, the check (a damaged file is moved aside and a new one made), the write-ahead log and the schema version (`PRAGMA user_version`). An old file with
    /// no version holds these tables already: the first step does nothing to it and the version is stamped.
    fn init(&self) -> R<()> {
        sqlite_store::open_migrated(&self.path, &Self::opts(), STEPS).map(|_| ()).map_err(e2s("create experience schema"))
    }

    // ---- experiences -------------------------------------------------------------------------

    pub fn insert(&self, e: &Experience, embedding: &[f32]) -> R<i64> {
        let c = self.conn()?;
        c.execute(
            &format!("INSERT INTO agent_experiences ({}, embedding) VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39)", COLS),
            params![
                e.scope.as_str(), e.project_key, e.owner_id, e.task_type, e.experience_type.as_str(), e.problem_summary,
                e.context_summary, e.approach, to_json(&e.important_steps), to_json(&e.tools_used), to_json(&e.constraints),
                to_json(&e.technologies), e.solution_summary, e.verification_method.as_str(), e.verification_result,
                e.verified as i64, e.result, e.success as i64, e.failed_approach, e.failure_reason, e.correction,
                e.confidence as f64, e.quality_score as f64, e.status.as_str(), e.human_approved as i64,
                e.embedding_model, e.embedding_version as i64, e.created_at, e.updated_at, e.last_verified_at, e.last_used_at,
                e.retrieval_count as i64, e.reuse_count as i64, e.successful_reuse_count as i64, e.failed_reuse_count as i64,
                e.source_task_id, e.project_version, e.experience_version as i64, embedding_to_blob(embedding),
            ],
        )
        .map_err(e2s("insert experience"))?;
        Ok(c.last_insert_rowid())
    }

    /// Overwrite every field of an existing experience (and its embedding when given).
    pub fn update(&self, e: &Experience, embedding: Option<&[f32]>) -> R<()> {
        let c = self.conn()?;
        let n = c
            .execute(
                "UPDATE agent_experiences SET scope=?1, project_key=?2, owner_id=?3, task_type=?4, experience_type=?5, problem_summary=?6, \
                 context_summary=?7, approach=?8, important_steps=?9, tools_used=?10, constraints=?11, technologies=?12, solution_summary=?13, \
                 verification_method=?14, verification_result=?15, verified=?16, result=?17, success=?18, failed_approach=?19, failure_reason=?20, \
                 correction=?21, confidence=?22, quality_score=?23, status=?24, human_approved=?25, embedding_model=?26, embedding_version=?27, \
                 updated_at=?28, last_verified_at=?29, last_used_at=?30, retrieval_count=?31, reuse_count=?32, successful_reuse_count=?33, \
                 failed_reuse_count=?34, source_task_id=?35, project_version=?36, experience_version=?37 WHERE id=?38",
                params![
                    e.scope.as_str(), e.project_key, e.owner_id, e.task_type, e.experience_type.as_str(), e.problem_summary,
                    e.context_summary, e.approach, to_json(&e.important_steps), to_json(&e.tools_used), to_json(&e.constraints),
                    to_json(&e.technologies), e.solution_summary, e.verification_method.as_str(), e.verification_result,
                    e.verified as i64, e.result, e.success as i64, e.failed_approach, e.failure_reason, e.correction,
                    e.confidence as f64, e.quality_score as f64, e.status.as_str(), e.human_approved as i64,
                    e.embedding_model, e.embedding_version as i64, e.updated_at, e.last_verified_at, e.last_used_at,
                    e.retrieval_count as i64, e.reuse_count as i64, e.successful_reuse_count as i64, e.failed_reuse_count as i64,
                    e.source_task_id, e.project_version, e.experience_version as i64, e.id,
                ],
            )
            .map_err(e2s("update experience"))?;
        if n == 0 {
            return Err(format!("experience {} not found", e.id));
        }
        if let Some(emb) = embedding {
            c.execute("UPDATE agent_experiences SET embedding=?1 WHERE id=?2", params![embedding_to_blob(emb), e.id])
                .map_err(e2s("update embedding"))?;
        }
        Ok(())
    }

    pub fn get(&self, id: i64) -> R<Option<Experience>> {
        let c = self.conn()?;
        c.query_row(&format!("SELECT {COLS} FROM agent_experiences WHERE id=?1"), params![id], row_to_experience)
            .optional()
            .map_err(e2s("get experience"))
    }

    pub fn delete(&self, id: i64) -> R<bool> {
        let c = self.conn()?;
        c.execute("DELETE FROM agent_experience_usage WHERE experience_id=?1", params![id]).map_err(e2s("delete usage"))?;
        Ok(c.execute("DELETE FROM agent_experiences WHERE id=?1", params![id]).map_err(e2s("delete experience"))? > 0)
    }

    pub fn clear(&self) -> R<usize> {
        let c = self.conn()?;
        c.execute("DELETE FROM agent_experience_usage", []).map_err(e2s("clear usage"))?;
        let n = c.execute("DELETE FROM agent_experiences", []).map_err(e2s("clear experiences"))?;
        // Cleared on purpose: the deleted text must not stay in the write-ahead log.
        let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(n)
    }

    pub fn list(&self, f: &ListFilter) -> R<Vec<Experience>> {
        let c = self.conn()?;
        let mut sql = format!("SELECT {COLS} FROM agent_experiences WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(s) = f.status {
            args.push(s.as_str().to_string().into());
            sql.push_str(&format!(" AND status=?{}", args.len()));
        }
        if let Some(t) = f.experience_type {
            args.push(t.as_str().to_string().into());
            sql.push_str(&format!(" AND experience_type=?{}", args.len()));
        }
        if let Some(p) = &f.project_key {
            args.push(p.clone().into());
            sql.push_str(&format!(" AND project_key=?{}", args.len()));
        }
        if let Some(t) = f.text.as_ref().filter(|t| !t.trim().is_empty()) {
            let like = format!("%{}%", t.trim().replace(['%', '_'], ""));
            args.push(like.into());
            let n = args.len();
            sql.push_str(&format!(" AND (problem_summary LIKE ?{n} OR approach LIKE ?{n} OR solution_summary LIKE ?{n} OR task_type LIKE ?{n})"));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC");
        sql.push_str(&format!(" LIMIT {} OFFSET {}", f.limit.unwrap_or(100).min(500), f.offset.unwrap_or(0)));
        let mut stmt = c.prepare(&sql).map_err(e2s("prepare list"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_experience).map_err(e2s("list"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read list"))
    }

    /// Every experience the viewer may see, with its embedding, for the vector index.
    /// GLOBAL is visible to everyone; PROJECT only for the same project; USER only to its owner.
    pub fn visible_with_embeddings(&self, viewer: &Viewer) -> R<Vec<(Experience, Vec<f32>)>> {
        let c = self.conn()?;
        let sql = format!(
            "SELECT {COLS}, embedding FROM agent_experiences WHERE \
             (scope='GLOBAL' OR (scope='PROJECT' AND project_key=?1 AND ?1<>'') OR (scope='USER' AND owner_id=?2 AND ?2<>''))"
        );
        let mut stmt = c.prepare(&sql).map_err(e2s("prepare visible"))?;
        let rows = stmt
            .query_map(params![viewer.project_key, viewer.owner_id], |row| {
                let e = row_to_experience(row)?;
                let blob: Option<Vec<u8>> = row.get(39)?;
                Ok((e, blob.map(|b| blob_to_embedding(&b)).unwrap_or_default()))
            })
            .map_err(e2s("visible"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read visible"))
    }

    /// Every experience regardless of scope, for maintenance (reindexing, consolidation, management).
    /// Never use this to answer a task: retrieval goes through `visible_with_embeddings`.
    pub fn all_with_embeddings(&self) -> R<Vec<(Experience, Vec<f32>)>> {
        let c = self.conn()?;
        let mut stmt = c.prepare(&format!("SELECT {COLS}, embedding FROM agent_experiences ORDER BY id")).map_err(e2s("prepare all"))?;
        let rows = stmt
            .query_map([], |row| {
                let e = row_to_experience(row)?;
                let blob: Option<Vec<u8>> = row.get(39)?;
                Ok((e, blob.map(|b| blob_to_embedding(&b)).unwrap_or_default()))
            })
            .map_err(e2s("all"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read all"))
    }

    pub fn count(&self) -> R<u64> {
        let c = self.conn()?;
        c.query_row("SELECT COUNT(*) FROM agent_experiences", [], |r| r.get::<_, i64>(0)).map(|n| n as u64).map_err(e2s("count"))
    }

    pub fn bump_retrieved(&self, ids: &[i64]) -> R<()> {
        let c = self.conn()?;
        for id in ids {
            c.execute("UPDATE agent_experiences SET retrieval_count = retrieval_count + 1 WHERE id=?1", params![id]).map_err(e2s("bump retrieved"))?;
        }
        Ok(())
    }

    // ---- usage -------------------------------------------------------------------------------

    pub fn record_usage(&self, experience_id: i64, task_summary: &str, similarity: f32) -> R<i64> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO agent_experience_usage (experience_id, used_at, task_summary, similarity) VALUES (?1, ?2, ?3, ?4)",
            params![experience_id, now_secs(), task_summary, similarity as f64],
        )
        .map_err(e2s("record usage"))?;
        Ok(c.last_insert_rowid())
    }

    pub fn finish_usage(&self, usage_id: i64, success: bool, note: &str) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE agent_experience_usage SET success=?1, note=?2 WHERE id=?3", params![success as i64, note, usage_id])
            .map_err(e2s("finish usage"))?;
        Ok(())
    }

    pub fn usage_for(&self, experience_id: i64, limit: u32) -> R<Vec<UsageRow>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT id, experience_id, used_at, task_summary, similarity, success, note FROM agent_experience_usage WHERE experience_id=?1 ORDER BY id DESC LIMIT ?2")
            .map_err(e2s("prepare usage"))?;
        let rows = stmt
            .query_map(params![experience_id, limit.min(200)], |r| {
                Ok(UsageRow {
                    id: r.get(0)?,
                    experience_id: r.get(1)?,
                    used_at: r.get(2)?,
                    task_summary: r.get(3)?,
                    similarity: r.get::<_, f64>(4)? as f32,
                    success: r.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                    note: r.get(6)?,
                })
            })
            .map_err(e2s("usage"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read usage"))
    }

    // ---- counters and stats ------------------------------------------------------------------

    pub fn bump(&self, name: &str, delta: f64) -> R<()> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO agent_learning_counters (name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = value + ?2",
            params![name, delta],
        )
        .map_err(e2s("bump counter"))?;
        Ok(())
    }

    pub fn set_counter(&self, name: &str, value: f64) -> R<()> {
        let c = self.conn()?;
        c.execute(
            "INSERT INTO agent_learning_counters (name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = ?2",
            params![name, value],
        )
        .map_err(e2s("set counter"))?;
        Ok(())
    }

    pub fn counter(&self, name: &str) -> f64 {
        self.conn()
            .and_then(|c| {
                c.query_row("SELECT value FROM agent_learning_counters WHERE name=?1", params![name], |r| r.get::<_, f64>(0))
                    .optional()
                    .map_err(e2s("counter"))
            })
            .ok()
            .flatten()
            .unwrap_or(0.0)
    }

    pub fn stats(&self) -> R<LearningStats> {
        let c = self.conn()?;
        let mut s = LearningStats::default();
        let n = |name: &str| self.counter(name);
        s.tasks_analyzed = n("tasks_analyzed") as u64;
        s.tasks_successful = n("tasks_successful") as u64;
        s.tasks_failed = n("tasks_failed") as u64;
        s.experiences_created = n("created") as u64;
        s.experiences_updated = n("updated") as u64;
        s.experiences_rejected = n("rejected") as u64;
        s.duplicates_prevented = n("duplicates_prevented") as u64;
        s.retrievals = n("retrievals") as u64;
        s.retrieval_hits = n("retrieval_hits") as u64;
        s.retrieval_hit_rate = if s.retrievals == 0 { 0.0 } else { s.retrieval_hits as f32 / s.retrievals as f32 };
        let sim_n = n("similarity_n");
        s.average_similarity = if sim_n > 0.0 { (n("similarity_sum") / sim_n) as f32 } else { 0.0 };
        s.practice_runs = n("practice_runs") as u64;
        s.practice_verified = n("practice_verified") as u64;
        s.last_learning_at = n("last_learning_at") as i64;
        s.last_success_at = n("last_success_at") as i64;

        let (total, reuse, ok, bad, quality): (i64, i64, i64, i64, f64) = c
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(reuse_count),0), COALESCE(SUM(successful_reuse_count),0), COALESCE(SUM(failed_reuse_count),0), COALESCE(AVG(quality_score),0) FROM agent_experiences",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(e2s("stats totals"))?;
        s.total_experiences = total as u64;
        s.experiences_reused = reuse as u64;
        s.successful_reuses = ok as u64;
        s.failed_reuses = bad as u64;
        s.average_quality = quality as f32;

        let mut stmt = c.prepare("SELECT status, COUNT(*) FROM agent_experiences GROUP BY status").map_err(e2s("stats status"))?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(e2s("stats status"))? {
            let (status, count) = row.map_err(e2s("stats status row"))?;
            s.by_status.insert(status, count as u64);
        }
        s.experiences_deprecated = s.by_status.get("DEPRECATED").copied().unwrap_or(0);
        let mut stmt = c.prepare("SELECT status, COUNT(*) FROM agent_learning_jobs GROUP BY status").map_err(e2s("stats jobs"))?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(e2s("stats jobs"))? {
            let (status, count) = row.map_err(e2s("stats jobs row"))?;
            s.jobs.insert(status, count as u64);
        }
        Ok(s)
    }

    // ---- practice tasks ----------------------------------------------------------------------

    pub fn practice_start(&self, goal: &str, family: &str, source: &str) -> R<i64> {
        let c = self.conn()?;
        let t = now_secs();
        c.execute(
            "INSERT INTO agent_practice_tasks (goal, family, source, status, created_at, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?4)",
            params![goal, family, source, t],
        )
        .map_err(e2s("start practice"))?;
        Ok(c.last_insert_rowid())
    }

    pub fn practice_finish(&self, id: i64, status: &str, verified: bool, note: &str, steps: u32) -> R<()> {
        let c = self.conn()?;
        c.execute(
            "UPDATE agent_practice_tasks SET status=?1, verified=?2, note=?3, steps=?4, finished_at=?5 WHERE id=?6",
            params![status, verified as i64, note.chars().take(300).collect::<String>(), steps as i64, now_secs(), id],
        )
        .map_err(e2s("finish practice"))?;
        Ok(())
    }

    pub fn practice_recent(&self, limit: u32) -> R<Vec<PracticeRow>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT id, goal, family, source, status, created_at, started_at, finished_at, verified, note, steps FROM agent_practice_tasks ORDER BY id DESC LIMIT ?1")
            .map_err(e2s("prepare practice"))?;
        let rows = stmt
            .query_map(params![limit.min(200)], |r| {
                Ok(PracticeRow {
                    id: r.get(0)?,
                    goal: r.get(1)?,
                    family: r.get(2)?,
                    source: r.get(3)?,
                    status: r.get(4)?,
                    created_at: r.get(5)?,
                    started_at: r.get(6)?,
                    finished_at: r.get(7)?,
                    verified: r.get::<_, i64>(8)? != 0,
                    note: r.get(9)?,
                    steps: r.get::<_, i64>(10)? as u32,
                })
            })
            .map_err(e2s("practice"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read practice"))
    }

    /// Goals practised since `cutoff` (unix seconds), lower-cased, so the same task is not repeated.
    pub fn practice_goals_since(&self, cutoff: i64) -> R<Vec<String>> {
        let c = self.conn()?;
        let mut stmt = c.prepare("SELECT lower(goal) FROM agent_practice_tasks WHERE created_at >= ?1").map_err(e2s("prepare goals"))?;
        let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0)).map_err(e2s("goals"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read goals"))
    }

    pub fn practice_started_since(&self, cutoff: i64) -> R<u32> {
        let c = self.conn()?;
        c.query_row("SELECT COUNT(*) FROM agent_practice_tasks WHERE started_at >= ?1", params![cutoff], |r| r.get::<_, i64>(0))
            .map(|n| n as u32)
            .map_err(e2s("count practice"))
    }

    pub fn last_practice_started(&self) -> i64 {
        self.conn()
            .and_then(|c| c.query_row("SELECT COALESCE(MAX(started_at), 0) FROM agent_practice_tasks", [], |r| r.get::<_, i64>(0)).map_err(e2s("last practice")))
            .unwrap_or(0)
    }

    /// A practice task left `running` by a crash is marked interrupted on start-up.
    pub fn interrupt_stale_practice(&self) -> R<usize> {
        let c = self.conn()?;
        c.execute("UPDATE agent_practice_tasks SET status='interrupted', finished_at=?1 WHERE status='running'", params![now_secs()]).map_err(e2s("interrupt practice"))
    }

    // ---- evaluation --------------------------------------------------------------------------

    /// Record one finished run, split by whether earlier experience was offered to the planner.
    /// This is what lets the dashboard answer "is memory actually helping?".
    pub fn record_run(&self, with_memory: bool, verified: bool, elapsed_ms: u64, tool_calls: u32, tokens: u32) -> R<()> {
        let g = if with_memory { "with" } else { "without" };
        self.bump(&format!("eval_runs_{g}"), 1.0)?;
        if verified {
            self.bump(&format!("eval_success_{g}"), 1.0)?;
        }
        self.bump(&format!("eval_ms_{g}"), elapsed_ms as f64)?;
        self.bump(&format!("eval_tools_{g}"), tool_calls as f64)?;
        self.bump(&format!("eval_tokens_{g}"), tokens as f64)?;
        Ok(())
    }

    pub fn evaluation(&self) -> R<Evaluation> {
        let n = |name: &str| self.counter(name);
        let avg = |sum: f64, runs: f64| if runs > 0.0 { sum / runs } else { 0.0 };
        let (rw, ro) = (n("eval_runs_with"), n("eval_runs_without"));
        let (sw, so) = (n("eval_success_with"), n("eval_success_without"));
        let stats = self.stats()?;
        let reuse_done = stats.successful_reuses + stats.failed_reuses;
        let mut ev = Evaluation {
            runs_with_memory: rw as u64,
            runs_without_memory: ro as u64,
            success_rate_with_memory: if rw > 0.0 { (sw / rw) as f32 } else { 0.0 },
            success_rate_without_memory: if ro > 0.0 { (so / ro) as f32 } else { 0.0 },
            avg_execution_ms_with_memory: avg(n("eval_ms_with"), rw) as u64,
            avg_execution_ms_without_memory: avg(n("eval_ms_without"), ro) as u64,
            avg_tool_calls_with_memory: avg(n("eval_tools_with"), rw) as f32,
            avg_tool_calls_without_memory: avg(n("eval_tools_without"), ro) as f32,
            avg_tokens_with_memory: avg(n("eval_tokens_with"), rw) as u64,
            avg_tokens_without_memory: avg(n("eval_tokens_without"), ro) as u64,
            memory_related_failures: (rw - sw).max(0.0) as u64,
            retrieval_hit_rate: stats.retrieval_hit_rate,
            reuse_success_rate: if reuse_done == 0 { 0.0 } else { stats.successful_reuses as f32 / reuse_done as f32 },
            note: String::new(),
        };
        ev.note = if rw < 10.0 || ro < 10.0 {
            "Fewer than 10 runs in at least one group: these numbers are an early indication, not a conclusion.".to_string()
        } else if ev.success_rate_with_memory > ev.success_rate_without_memory {
            "With memory the agent verified more of its answers than without.".to_string()
        } else {
            "Memory is not improving verified success yet; check the failed reuses and stale experiences.".to_string()
        };
        Ok(ev)
    }

    // ---- events ------------------------------------------------------------------------------

    /// Append a structured event. `detail` must already be free of secrets.
    pub fn log_event(&self, event: &str, detail: &Value, job_id: Option<i64>) {
        let write = || -> R<()> {
            let c = self.conn()?;
            c.execute(
                "INSERT INTO agent_learning_events (ts, event, detail, job_id) VALUES (?1, ?2, ?3, ?4)",
                params![now_secs(), event, detail.to_string(), job_id],
            )
            .map_err(e2s("log event"))?;
            // Keep the log bounded.
            c.execute("DELETE FROM agent_learning_events WHERE id <= (SELECT MAX(id) FROM agent_learning_events) - 5000", [])
                .map_err(e2s("trim events"))?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!("could not record learning event {event}: {e}");
        }
        tracing::info!(target: "aqua_learning", event = event, "{}", detail);
    }

    pub fn recent_events(&self, limit: u32) -> R<Vec<EventRow>> {
        let c = self.conn()?;
        let mut stmt = c
            .prepare("SELECT id, ts, event, detail, job_id FROM agent_learning_events ORDER BY id DESC LIMIT ?1")
            .map_err(e2s("prepare events"))?;
        let rows = stmt
            .query_map(params![limit.min(500)], |r| {
                let detail: String = r.get(3)?;
                Ok(EventRow { id: r.get(0)?, ts: r.get(1)?, event: r.get(2)?, detail: serde_json::from_str(&detail).unwrap_or(Value::Null), job_id: r.get(4)? })
            })
            .map_err(e2s("events"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read events"))
    }

    // ---- jobs --------------------------------------------------------------------------------

    pub fn enqueue_job(&self, kind: &str, payload: &Value, max_attempts: u32) -> R<i64> {
        let c = self.conn()?;
        let t = now_secs();
        c.execute(
            "INSERT INTO agent_learning_jobs (kind, payload, status, attempts, max_attempts, next_run_at, created_at, updated_at) VALUES (?1, ?2, 'queued', 0, ?3, ?4, ?4, ?4)",
            params![kind, payload.to_string(), max_attempts as i64, t],
        )
        .map_err(e2s("enqueue job"))?;
        Ok(c.last_insert_rowid())
    }

    /// Take the next runnable job (queued, due), marking it running. `None` when there is none.
    pub fn claim_next_job(&self, now: i64) -> R<Option<JobRow>> {
        let mut c = self.conn()?;
        // Immediate: take the write lock up front. A deferred transaction reads first and then tries
        // to upgrade, which SQLite refuses at once (no waiting) when another connection is writing.
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(e2s("claim tx"))?;
        let row = tx
            .query_row(
                "SELECT id, kind, payload, status, attempts, max_attempts, next_run_at, last_error, created_at, updated_at \
                 FROM agent_learning_jobs WHERE status='queued' AND next_run_at <= ?1 ORDER BY id LIMIT 1",
                params![now],
                job_from_row,
            )
            .optional()
            .map_err(e2s("claim select"))?;
        if let Some(job) = &row {
            tx.execute("UPDATE agent_learning_jobs SET status='running', updated_at=?1 WHERE id=?2", params![now, job.id]).map_err(e2s("claim update"))?;
        }
        tx.commit().map_err(e2s("claim commit"))?;
        Ok(row.map(|mut j| {
            j.status = "running".into();
            j
        }))
    }

    pub fn complete_job(&self, id: i64) -> R<()> {
        let c = self.conn()?;
        c.execute("UPDATE agent_learning_jobs SET status='done', updated_at=?1 WHERE id=?2", params![now_secs(), id]).map_err(e2s("complete job"))?;
        Ok(())
    }

    /// Record a failed attempt. Re-queues with exponential backoff, or marks the job `dead` (kept for
    /// inspection, never retried again) once its attempts are used up.
    pub fn fail_job(&self, id: i64, error: &str, now: i64) -> R<String> {
        let c = self.conn()?;
        let (attempts, max): (i64, i64) = c
            .query_row("SELECT attempts, max_attempts FROM agent_learning_jobs WHERE id=?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(e2s("fail job read"))?;
        let attempts = attempts + 1;
        let (status, next) = if attempts >= max {
            ("dead", now)
        } else {
            // 5 s, 10 s, 20 s, ... capped at 10 minutes.
            ("queued", now + (5i64 << (attempts - 1).min(7)).min(600))
        };
        c.execute(
            "UPDATE agent_learning_jobs SET status=?1, attempts=?2, next_run_at=?3, last_error=?4, updated_at=?5 WHERE id=?6",
            params![status, attempts, next, error.chars().take(300).collect::<String>(), now, id],
        )
        .map_err(e2s("fail job"))?;
        Ok(status.to_string())
    }

    /// A job left `running` by a crash goes back to the queue on start-up.
    pub fn requeue_interrupted_jobs(&self) -> R<usize> {
        let c = self.conn()?;
        c.execute("UPDATE agent_learning_jobs SET status='queued', updated_at=?1 WHERE status='running'", params![now_secs()]).map_err(e2s("requeue"))
    }

    pub fn list_jobs(&self, status: Option<&str>, limit: u32) -> R<Vec<JobRow>> {
        let c = self.conn()?;
        let sql = "SELECT id, kind, payload, status, attempts, max_attempts, next_run_at, last_error, created_at, updated_at FROM agent_learning_jobs \
                   WHERE (?1 IS NULL OR status=?1) ORDER BY id DESC LIMIT ?2";
        let mut stmt = c.prepare(sql).map_err(e2s("prepare jobs"))?;
        let rows = stmt.query_map(params![status, limit.min(200)], job_from_row).map_err(e2s("jobs"))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(e2s("read jobs"))
    }

    /// Keep the history tables from growing without end: failed ("dead") jobs after 30 days (they are kept that long
    /// for inspection), reuse records after 90 days, and practice tasks after 30 days (the newest 200 always stay).
    /// Returns how many rows were removed. The write-ahead log is then emptied so the removed text is not left in it.
    pub fn prune_history(&self) -> R<usize> {
        let c = self.conn()?;
        let now = now_secs();
        let mut removed = 0;
        removed += c.execute("DELETE FROM agent_learning_jobs WHERE status='dead' AND updated_at < ?1", params![now - 30 * 86_400]).map_err(e2s("prune dead jobs"))?;
        removed += c.execute("DELETE FROM agent_experience_usage WHERE used_at < ?1", params![now - 90 * 86_400]).map_err(e2s("prune usage"))?;
        removed += c
            .execute(
                "DELETE FROM agent_practice_tasks WHERE created_at < ?1 AND id NOT IN (SELECT id FROM agent_practice_tasks ORDER BY id DESC LIMIT 200)",
                params![now - 30 * 86_400],
            )
            .map_err(e2s("prune practice"))?;
        if removed > 0 {
            let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(removed)
    }

    /// Drop finished jobs older than `older_than_secs` (dead ones are kept for inspection).
    pub fn prune_jobs(&self, older_than_secs: i64) -> R<usize> {
        let c = self.conn()?;
        c.execute("DELETE FROM agent_learning_jobs WHERE status='done' AND updated_at < ?1", params![now_secs() - older_than_secs]).map_err(e2s("prune jobs"))
    }

    // ---- settings ----------------------------------------------------------------------------

    pub fn load_settings(&self) -> LearningSettings {
        let stored = self
            .conn()
            .and_then(|c| {
                c.query_row("SELECT value FROM agent_learning_settings WHERE key='settings'", [], |r| r.get::<_, String>(0))
                    .optional()
                    .map_err(e2s("load settings"))
            })
            .ok()
            .flatten();
        match stored.and_then(|t| serde_json::from_str::<LearningSettings>(&t).ok()) {
            Some(s) => s.clamped(),
            None => LearningSettings::default(),
        }
    }

    pub fn save_settings(&self, s: &LearningSettings) -> R<LearningSettings> {
        let s = s.clone().clamped();
        let c = self.conn()?;
        c.execute(
            "INSERT INTO agent_learning_settings (key, value) VALUES ('settings', ?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            params![serde_json::to_string(&s).map_err(e2s("serialize settings"))?],
        )
        .map_err(e2s("save settings"))?;
        Ok(s)
    }
}

fn job_from_row(r: &Row) -> rusqlite::Result<JobRow> {
    let payload: String = r.get(2)?;
    Ok(JobRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        status: r.get(3)?,
        attempts: r.get::<_, i64>(4)? as u32,
        max_attempts: r.get::<_, i64>(5)? as u32,
        next_run_at: r.get(6)?,
        last_error: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}
