//! The running side of the learning system: retrieval for the agent, the background learning
//! worker, status for the dashboard and the management actions.
//!
//! Failure isolation is the rule here. Every call the agent makes returns something usable even when
//! the database, the index or the worker is broken; failures are logged and retried in the
//! background, never raised into a user's task.

use super::learner::{learn, LearningOutcome, TaskRecord};
use super::model::*;
use super::quality::{next_status, quality_score};
use super::retrieval::{check_applicability, retrieve, RetrievalResult, TaskProfile};
use super::settings::LearningSettings;
use super::store::{ExperienceStore, JobRow, LearningStats, ListFilter, Viewer};
use super::vector::{cosine, reindex, BruteForceIndex, EmbeddingProvider, HashEmbedding, ReindexReport, VectorIndex};
use serde::{Deserialize, Serialize};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const JOB_KIND_LEARN: &str = "learn_task";
const MAX_JOB_ATTEMPTS: u32 = 3;
/// Above this many waiting jobs, new ones are dropped (and counted) instead of piling up.
const QUEUE_LIMIT_PER_WORKER: u32 = 10;
/// Experiences this similar (and of the same kind) are merged by consolidation.
const CONSOLIDATION_SIMILARITY: f32 = 0.92;

/// Whether people are using the agent, so background practice waits for them.
#[derive(Default)]
pub struct UserActivity {
    active: std::sync::atomic::AtomicUsize,
    last: std::sync::atomic::AtomicI64,
}

/// Held for as long as a user's task runs.
pub struct ActiveRunGuard<'a>(&'a UserActivity);

impl UserActivity {
    pub fn begin(&self) -> ActiveRunGuard<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        self.touch();
        ActiveRunGuard(self)
    }
    /// A user's task is running right now.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }
    /// Unix seconds of the last time a user's task started or finished (0 if none since start-up).
    pub fn last(&self) -> i64 {
        self.last.load(Ordering::SeqCst)
    }
    pub fn touch(&self) {
        self.last.store(now_secs(), Ordering::SeqCst);
    }
}

impl Drop for ActiveRunGuard<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

pub struct LearningService {
    store: ExperienceStore,
    provider: Arc<dyn EmbeddingProvider>,
    worker_started: AtomicBool,
    stage: Mutex<String>,
    activity: UserActivity,
}

static SERVICE: OnceLock<Arc<LearningService>> = OnceLock::new();

/// The process-wide service, opened lazily on the workspace. `AQUA_LEARNING_ROOT` moves the store
/// (used by tests so they never touch the real data folder).
pub fn service() -> Arc<LearningService> {
    SERVICE
        .get_or_init(|| {
            let root = std::env::var_os("AQUA_LEARNING_ROOT").map(std::path::PathBuf::from).unwrap_or_else(crate::runtime::workspace_root);
            LearningService::open(&root)
        })
        .clone()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningStatus {
    pub learning_enabled: bool,
    pub retrieval_enabled: bool,
    pub auto_memory_creation: bool,
    pub autonomous_experimentation: bool,
    /// What the worker is doing right now, or empty when idle.
    pub current_activity: String,
    pub jobs_queued: u64,
    pub jobs_running: u64,
    pub jobs_dead: u64,
    pub last_learning_at: i64,
    pub last_success_at: i64,
    pub worker_running: bool,
    pub vector_store: VectorStoreStatus,
    pub embedding: EmbeddingStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStoreStatus {
    pub ok: bool,
    pub provider: String,
    pub indexed: usize,
    /// Experiences whose vectors came from another model or version; they need a reindex.
    pub needs_reindex: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingStatus {
    pub ok: bool,
    pub model: String,
    pub version: u32,
    pub dimensions: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsolidationReport {
    pub merged: usize,
    pub marked_stale: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevalidationReport {
    pub id: i64,
    pub valid: bool,
    pub status: String,
    pub reason: String,
}

impl LearningService {
    pub fn open(root: &Path) -> Arc<Self> {
        let store = ExperienceStore::new(root);
        // A job left running by a crash goes back in the queue.
        if let Err(e) = store.requeue_interrupted_jobs() {
            tracing::warn!("could not requeue interrupted learning jobs: {e}");
        }
        Arc::new(Self { store, provider: Arc::new(HashEmbedding), worker_started: AtomicBool::new(false), stage: Mutex::new(String::new()), activity: UserActivity::default() })
    }

    /// Same as `open` with a specific embedding provider (a real model later, or a test double).
    pub fn with_provider(root: &Path, provider: Arc<dyn EmbeddingProvider>) -> Arc<Self> {
        let store = ExperienceStore::new(root);
        let _ = store.requeue_interrupted_jobs();
        Arc::new(Self { store, provider, worker_started: AtomicBool::new(false), stage: Mutex::new(String::new()), activity: UserActivity::default() })
    }

    /// A service whose background thread is never started: jobs run only when `process_one` is
    /// called. Lets tests be deterministic.
    #[cfg(test)]
    pub fn manual(root: &Path, provider: Arc<dyn EmbeddingProvider>) -> Arc<Self> {
        let store = ExperienceStore::new(root);
        Arc::new(Self { store, provider, worker_started: AtomicBool::new(true), stage: Mutex::new(String::new()), activity: UserActivity::default() })
    }

    pub fn store(&self) -> &ExperienceStore {
        &self.store
    }

    pub fn activity(&self) -> &UserActivity {
        &self.activity
    }

    pub fn provider(&self) -> &dyn EmbeddingProvider {
        self.provider.as_ref()
    }

    pub fn settings(&self) -> LearningSettings {
        self.store.load_settings()
    }

    pub fn update_settings(&self, new: &LearningSettings) -> Result<LearningSettings, String> {
        let saved = self.store.save_settings(new)?;
        self.store.log_event(
            "settings_changed",
            &serde_json::json!({ "learning_enabled": saved.learning_enabled, "retrieval_enabled": saved.retrieval_enabled, "auto_memory_creation": saved.auto_memory_creation, "autonomous_experimentation": saved.autonomous_experimentation }),
            None,
        );
        Ok(saved)
    }

    fn set_stage(&self, text: &str) {
        if let Ok(mut s) = self.stage.lock() {
            *s = text.to_string();
        }
    }

    // ---- agent-facing -----------------------------------------------------------------------

    /// Experiences that might help with this task. Never fails and never panics.
    pub fn retrieve_for(&self, profile: &TaskProfile) -> RetrievalResult {
        let settings = self.settings();
        let viewer = Viewer { project_key: profile.project_key.clone(), owner_id: String::new() };
        match catch_unwind(AssertUnwindSafe(|| retrieve(&self.store, self.provider.as_ref(), &settings, profile, &viewer))) {
            Ok(r) => r,
            Err(_) => RetrievalResult { memory_error: Some("memory search panicked".into()), ..Default::default() },
        }
    }

    /// Queue a finished task for learning. Returns the job id, or `None` when learning is off, the
    /// queue is full or storage is unavailable (all of which are logged, none of which the task sees).
    pub fn enqueue_learning(self: &Arc<Self>, task: &TaskRecord) -> Option<i64> {
        let settings = self.settings();
        if !settings.learning_enabled {
            return None;
        }
        let waiting = self.store.stats().ok().map(|s| s.jobs.get("queued").copied().unwrap_or(0) + s.jobs.get("running").copied().unwrap_or(0)).unwrap_or(0);
        if waiting >= (settings.max_background_jobs * QUEUE_LIMIT_PER_WORKER) as u64 {
            self.store.log_event("learning_job_dropped", &serde_json::json!({ "reason": "queue full", "waiting": waiting }), None);
            return None;
        }
        let payload = match serde_json::to_value(task) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("could not serialise learning task: {e}");
                return None;
            }
        };
        match self.store.enqueue_job(JOB_KIND_LEARN, &payload, MAX_JOB_ATTEMPTS) {
            Ok(id) => {
                self.ensure_worker();
                Some(id)
            }
            Err(e) => {
                tracing::warn!("could not queue learning job: {e}");
                None
            }
        }
    }

    // ---- worker -----------------------------------------------------------------------------

    /// Run one due job. `Ok(false)` when there was nothing to do (or learning is off).
    pub fn process_one(&self, now: i64) -> Result<bool, String> {
        let settings = self.settings();
        if !settings.learning_enabled {
            return Ok(false); // queued jobs wait; nothing learns while the master switch is off
        }
        let Some(job) = self.store.claim_next_job(now)? else { return Ok(false) };
        self.store.log_event("learning_job_started", &serde_json::json!({ "kind": job.kind }), Some(job.id));
        self.set_stage("Analyzing task…");
        let result = catch_unwind(AssertUnwindSafe(|| self.run_job(&job, &settings)));
        self.set_stage("");
        let failure: Option<String> = match result {
            Ok(Ok(out)) => {
                self.store.complete_job(job.id)?;
                self.store.log_event("learning_job_completed", &serde_json::json!({ "outcome": out.label() }), Some(job.id));
                None
            }
            Ok(Err(e)) => Some(e),
            Err(_) => Some("learning job panicked".to_string()),
        };
        if let Some(error) = failure {
            let next = self.store.fail_job(job.id, &error, now)?;
            self.store.log_event("learning_job_failed", &serde_json::json!({ "error": truncate(&error, 200), "next": next }), Some(job.id));
        }
        Ok(true)
    }

    fn run_job(&self, job: &JobRow, settings: &LearningSettings) -> Result<LearningOutcome, String> {
        let task: TaskRecord = serde_json::from_value(job.payload.clone()).map_err(|e| format!("bad job payload: {e}"))?;
        self.set_stage("Evaluating result…");
        // Reuse feedback is always recorded; whether a new experience may be created is the
        // learner's decision (it honours `auto_memory_creation`).
        learn(&self.store, self.provider.as_ref(), settings, &task)
    }

    /// Start the background worker once. It sleeps while learning is off.
    pub fn ensure_worker(self: &Arc<Self>) {
        if self.worker_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("aqua-learning".into()).spawn(move || {
            let mut idle_rounds = 0u32;
            let mut last_prune = 0i64;
            loop {
                let now = now_secs();
                // Tidy by the clock, not by how long the worker has been idle: a busy install is never idle for an hour,
                // and its tables would otherwise never be pruned.
                if now - last_prune >= 3600 {
                    last_prune = now;
                    let _ = this.store.prune_jobs(7 * 86_400);
                    match this.store.prune_history() {
                        Ok(n) if n > 0 => tracing::info!("learning memory: pruned {n} old record(s)"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("learning memory prune: {e}"),
                    }
                }
                match this.process_one(now) {
                    Ok(true) => idle_rounds = 0,
                    Ok(false) => {
                        idle_rounds += 1;
                        if idle_rounds.is_multiple_of(150) {
                            // About every five idle minutes: is the daily tidy-up due?
                            if let Err(e) = this.consolidate_if_due(now_secs()) {
                                tracing::warn!("scheduled consolidation: {e}");
                            }
                        }
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    Err(e) => {
                        tracing::warn!("learning worker: {e}");
                        std::thread::sleep(Duration::from_secs(10));
                    }
                }
            }
        });
        if spawned.is_err() {
            self.worker_started.store(false, Ordering::SeqCst);
            tracing::warn!("could not start the learning worker thread");
        }
    }

    // ---- dashboard --------------------------------------------------------------------------

    pub fn status(&self) -> LearningStatus {
        let s = self.settings();
        let stats = self.store.stats().unwrap_or_default();
        let (vector_store, embedding) = self.health();
        LearningStatus {
            learning_enabled: s.learning_enabled,
            retrieval_enabled: s.retrieval_enabled,
            auto_memory_creation: s.auto_memory_creation,
            autonomous_experimentation: s.autonomous_experimentation,
            current_activity: self.stage.lock().map(|g| g.clone()).unwrap_or_default(),
            jobs_queued: stats.jobs.get("queued").copied().unwrap_or(0),
            jobs_running: stats.jobs.get("running").copied().unwrap_or(0),
            jobs_dead: stats.jobs.get("dead").copied().unwrap_or(0),
            last_learning_at: stats.last_learning_at,
            last_success_at: stats.last_success_at,
            worker_running: self.worker_started.load(Ordering::SeqCst),
            vector_store,
            embedding,
        }
    }

    fn health(&self) -> (VectorStoreStatus, EmbeddingStatus) {
        let embedding = match self.provider.embed("health check") {
            Ok(v) => EmbeddingStatus { ok: v.len() == self.provider.dim(), model: self.provider.model().into(), version: self.provider.version(), dimensions: v.len(), error: None },
            Err(e) => EmbeddingStatus { ok: false, model: self.provider.model().into(), version: self.provider.version(), dimensions: 0, error: Some(e) },
        };
        let vector = match self.store.all_with_embeddings() {
            Ok(rows) => {
                let index = BruteForceIndex::build(&rows, self.provider.as_ref());
                VectorStoreStatus { ok: true, provider: "in-process cosine index".into(), indexed: index.len(), needs_reindex: index.skipped_incompatible, error: None }
            }
            Err(e) => VectorStoreStatus { ok: false, provider: "in-process cosine index".into(), indexed: 0, needs_reindex: 0, error: Some(e) },
        };
        (vector, embedding)
    }

    pub fn stats(&self) -> Result<LearningStats, String> {
        self.store.stats()
    }

    pub fn recent_jobs(&self, limit: u32) -> Result<Vec<JobRow>, String> {
        self.store.list_jobs(None, limit)
    }

    // ---- management -------------------------------------------------------------------------

    /// A person's decision about one experience.
    pub fn set_status(&self, id: i64, action: &str) -> Result<Experience, String> {
        let mut e = self.store.get(id)?.ok_or_else(|| format!("experience {id} not found"))?;
        let settings = self.settings();
        let now = now_secs();
        match action {
            "approve" => {
                if !e.verified {
                    return Err("only an experience with verification evidence can be approved".into());
                }
                e.human_approved = true;
                e.status = ExperienceStatus::Trusted;
            }
            "trust" => {
                if !e.verified {
                    return Err("an unverified experience cannot be marked trusted".into());
                }
                e.human_approved = true;
                e.status = ExperienceStatus::Trusted;
            }
            "reject" | "invalid" => {
                e.status = ExperienceStatus::Invalid;
                e.human_approved = false;
            }
            "deprecate" => {
                // Withdrawing trust withdraws the approval too: it can come back through
                // revalidation, but only as VERIFIED until a person approves it again.
                e.status = ExperienceStatus::Deprecated;
                e.human_approved = false;
            }
            other => return Err(format!("unknown action {other:?}; use approve, trust, reject, invalid or deprecate")),
        }
        e.updated_at = now;
        e.quality_score = quality_score(&e, now, settings.stale_after_days);
        self.store.update(&e, None)?;
        self.store.log_event("experience_status_changed", &serde_json::json!({ "id": id, "action": action, "status": e.status.as_str() }), None);
        Ok(e)
    }

    /// Check whether a stale or deprecated experience still fits the current environment (its tools
    /// exist, nothing in it is retired or instruction-like). A pass returns it to VERIFIED; a fail
    /// deprecates it. This proves it is still *usable*; whether it still *works* is proven by the
    /// next real reuse, which moves its confidence either way.
    pub fn revalidate(&self, id: i64, available_tools: &[String]) -> Result<RevalidationReport, String> {
        let mut e = self.store.get(id)?.ok_or_else(|| format!("experience {id} not found"))?;
        let settings = self.settings();
        let now = now_secs();
        if e.status == ExperienceStatus::Invalid {
            return Ok(RevalidationReport { id, valid: false, status: e.status.as_str().into(), reason: "marked invalid by a person; revalidation does not override that".into() });
        }
        let profile = TaskProfile { text: e.problem_summary.clone(), task_type: e.task_type.clone(), project_key: e.project_key.clone(), technologies: e.technologies.clone(), available_tools: available_tools.to_vec() };
        // Judge it as if it were verified today, so only structural problems count.
        let mut probe = e.clone();
        probe.status = ExperienceStatus::Verified;
        probe.last_verified_at = now;
        probe.confidence = probe.confidence.max(settings.memory_confidence_threshold);
        let verdict = check_applicability(&probe, &profile, &settings, now);
        let (valid, reason) = match verdict {
            Ok(()) => (true, "still fits the current tools and environment".to_string()),
            Err(why) => (false, why),
        };
        if valid {
            e.status = if e.human_approved { ExperienceStatus::Trusted } else { ExperienceStatus::Verified };
            e.last_verified_at = now;
        } else {
            e.status = ExperienceStatus::Deprecated;
        }
        e.updated_at = now;
        e.quality_score = quality_score(&e, now, settings.stale_after_days);
        self.store.update(&e, None)?;
        self.store.log_event("experience_revalidated", &serde_json::json!({ "id": id, "valid": valid, "reason": reason }), None);
        Ok(RevalidationReport { id, valid, status: e.status.as_str().into(), reason })
    }

    pub fn delete(&self, id: i64) -> Result<bool, String> {
        let removed = self.store.delete(id)?;
        if removed {
            self.store.log_event("experience_deleted", &serde_json::json!({ "id": id }), None);
        }
        Ok(removed)
    }

    pub fn clear_all(&self) -> Result<usize, String> {
        let n = self.store.clear()?;
        self.store.log_event("experiences_cleared", &serde_json::json!({ "count": n }), None);
        Ok(n)
    }

    pub fn list(&self, filter: &ListFilter) -> Result<Vec<Experience>, String> {
        self.store.list(filter)
    }

    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<(Experience, f32)>, String> {
        let query = self.provider.embed(text)?;
        let rows = self.store.all_with_embeddings()?;
        let index = BruteForceIndex::build(&rows, self.provider.as_ref());
        Ok(index.search(&query, limit).into_iter().filter_map(|(id, sim)| rows.iter().find(|(e, _)| e.id == id).map(|(e, _)| (e.clone(), sim))).collect())
    }

    pub fn reindex_all(&self) -> Result<ReindexReport, String> {
        let report = reindex(&self.store, self.provider.as_ref())?;
        self.store.log_event("memory_reindexed", &serde_json::json!({ "total": report.total, "reindexed": report.reindexed, "failed": report.failed }), None);
        Ok(report)
    }

    /// Keep the memory small and good: merge near-identical experiences, mark old ones stale and
    /// drop the ones that never proved useful.
    /// Run `consolidate` if it has not run for a day and no user run is active. Returns whether it ran.
    /// The time of the last run is kept in the database, so a restart does not reset the schedule.
    pub fn consolidate_if_due(&self, now: i64) -> Result<Option<ConsolidationReport>, String> {
        const EVERY_SECS: i64 = 86_400;
        if self.activity.is_active() || now - (self.store.counter("last_consolidation") as i64) < EVERY_SECS {
            return Ok(None);
        }
        // Record first: a run that fails is retried tomorrow, not every five minutes.
        self.store.set_counter("last_consolidation", now as f64)?;
        self.consolidate().map(Some)
    }

    pub fn consolidate(&self) -> Result<ConsolidationReport, String> {
        let settings = self.settings();
        let now = now_secs();
        let mut report = ConsolidationReport::default();
        let mut rows = self.store.all_with_embeddings()?;
        rows.sort_by(|a, b| b.0.quality_score.partial_cmp(&a.0.quality_score).unwrap_or(std::cmp::Ordering::Equal));
        let mut gone: Vec<i64> = Vec::new();
        for i in 0..rows.len() {
            if gone.contains(&rows[i].0.id) {
                continue;
            }
            for j in (i + 1)..rows.len() {
                if gone.contains(&rows[j].0.id) || rows[i].0.scope != rows[j].0.scope || rows[i].0.project_key != rows[j].0.project_key || rows[i].0.owner_id != rows[j].0.owner_id {
                    continue;
                }
                let same_type = rows[i].0.task_type == rows[j].0.task_type;
                if same_type && cosine(&rows[i].1, &rows[j].1) >= CONSOLIDATION_SIMILARITY {
                    // Keep the higher-quality one (rows are sorted best first); fold the other's history into it.
                    let mut keep = rows[i].0.clone();
                    let drop = rows[j].0.clone();
                    keep.reuse_count += drop.reuse_count;
                    keep.successful_reuse_count += drop.successful_reuse_count;
                    keep.failed_reuse_count += drop.failed_reuse_count;
                    keep.retrieval_count += drop.retrieval_count;
                    keep.updated_at = now;
                    self.store.update(&keep, None)?;
                    self.store.delete(drop.id)?;
                    rows[i].0 = keep;
                    gone.push(drop.id);
                    report.merged += 1;
                }
            }
        }
        for (e, _) in rows.iter().filter(|(e, _)| !gone.contains(&e.id)) {
            let mut e = e.clone();
            let useless_retired = e.status.is_retired() && e.successful_reuse_count == 0 && now - e.updated_at > 90 * 86_400;
            let never_proven = e.status == ExperienceStatus::Candidate && now - e.created_at > 30 * 86_400;
            if useless_retired || never_proven {
                self.store.delete(e.id)?;
                report.removed += 1;
                continue;
            }
            let before = e.status;
            e.status = next_status(&e, &settings, now);
            if e.status != before {
                e.updated_at = now;
                self.store.update(&e, None)?;
                if e.status == ExperienceStatus::Stale {
                    report.marked_stale += 1;
                }
            }
        }
        self.store.log_event("memory_consolidated", &serde_json::json!({ "merged": report.merged, "marked_stale": report.marked_stale, "removed": report.removed }), None);
        Ok(report)
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
