//! The learning pipeline: finished task -> evaluation -> duplicate check -> create / update / reject.
//!
//! Nothing is stored just because a task finished. A task becomes an experience only when it
//! succeeded, was verified with real evidence (not the model's own claim), used tools worth
//! remembering and produced a record of enough quality. A near-identical existing experience is
//! updated rather than duplicated. The task record passed in is sanitised here again, so nothing
//! that reaches the database can carry a secret even if the caller forgot to clean it.

use super::model::*;
use super::quality::{apply_reuse_outcome, initial_confidence, next_status, quality_score};
use super::sanitize::{clean_for_storage, redact};
use super::settings::LearningSettings;
use super::store::{ExperienceStore, Viewer};
use super::vector::{cosine, embedding_text, BruteForceIndex, EmbeddingProvider, VectorIndex};
use serde::{Deserialize, Serialize};

/// Similarity at which a new experience is "essentially the same" as an existing one.
pub const DUPLICATE_SIMILARITY: f32 = 0.85;
/// Two problems this close, solved with the same tools in an equivalent way, are one experience.
pub const SAME_APPROACH_PROBLEM_SIMILARITY: f32 = 0.45;
/// How alike two approach descriptions must be to count as equivalent.
pub const APPROACH_SIMILARITY: f32 = 0.85;
/// Below this quality a verified task is still not worth remembering.
pub const MIN_QUALITY_TO_STORE: f32 = 0.5;

/// A failed attempt inside a task that later succeeded: what was tried and why it did not work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FailedAttempt {
    pub approach: String,
    pub reason: String,
}

/// An experience that was offered to the agent for this task, so the outcome can be credited.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsedExperience {
    pub experience_id: i64,
    pub usage_id: Option<i64>,
}

/// Everything the learner needs to know about a finished task. Concise facts only: never the
/// model's reasoning, never raw page text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub goal: String,
    pub task_type: String,
    pub project_key: String,
    pub owner_id: String,
    pub technologies: Vec<String>,
    /// One short line per tool call, for example `browser_navigate https://www.google.com/search?q=...`.
    pub steps: Vec<String>,
    pub tools_used: Vec<String>,
    pub constraints: Vec<String>,
    /// A short description of what was produced ("answered with index levels quoted from the page").
    pub result_summary: String,
    pub finished: bool,
    pub verification: Option<Verification>,
    pub used_experiences: Vec<UsedExperience>,
    pub failed_attempts: Vec<FailedAttempt>,
    pub project_version: String,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LearningOutcome {
    Created(i64),
    Updated(i64),
    /// Not stored, with the reason (shown in the dashboard).
    Rejected(String),
}

impl LearningOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Created(_) => "created",
            Self::Updated(_) => "updated",
            Self::Rejected(_) => "rejected",
        }
    }
}

/// A task the agent made up for itself (see `practice`). It can show that an experience still works
/// or has stopped working, but it cannot make an experience more trusted: the agent must not be able
/// to promote its own memory by grading its own homework.
pub fn is_practice(t: &TaskRecord) -> bool {
    t.task_id.starts_with("practice-")
}

fn verified_success(t: &TaskRecord) -> bool {
    t.finished && t.verification.as_ref().is_some_and(|v| v.verified)
}

/// Run the pipeline for one task. `Err` means infrastructure failed (the job is retried); a task
/// that simply is not worth remembering is `Ok(Rejected(..))`.
pub fn learn(store: &ExperienceStore, provider: &dyn EmbeddingProvider, settings: &LearningSettings, task: &TaskRecord) -> Result<LearningOutcome, String> {
    let now = now_secs();
    store.bump("tasks_analyzed", 1.0)?;
    store.set_counter("last_learning_at", now as f64)?;
    let success = verified_success(task);
    let practice = is_practice(task);
    store.bump(if success { "tasks_successful" } else { "tasks_failed" }, 1.0)?;

    // 1. Reuse feedback: the outcome of using an experience is recorded whether or not this task is
    //    itself remembered. A verified success credits it, a definite failure debits it, and a
    //    finished-but-unverified task counts as neither.
    let definite_failure = !task.finished || task.verification.as_ref().is_some_and(|v| !v.verified && v.method != VerificationMethod::None && v.method != VerificationMethod::ModelClaim);
    if success || definite_failure {
        for used in &task.used_experiences {
            record_reuse_outcome(store, settings, used, success, &task.goal, practice)?;
        }
    }

    // 2. Is it worth remembering?
    if !settings.auto_memory_creation {
        return reject(store, task, "automatic memory creation is off");
    }
    if !success {
        return reject(store, task, "not a verified success; unverified failure analysis is not stored as knowledge");
    }
    if task.tools_used.is_empty() || task.steps.is_empty() {
        return reject(store, task, "no reusable steps (answered without tools)");
    }
    if task.goal.split_whitespace().count() < 3 {
        return reject(store, task, "task too short to generalise");
    }
    let Some(problem) = clean_for_storage(&task.goal) else {
        return reject(store, task, "the task text is mostly sensitive data");
    };

    // 3. Build the experience from sanitised, concise pieces.
    let mut redactions = 0usize;
    let mut clean = |s: &str| {
        let (c, n) = redact(s);
        redactions += n;
        c
    };
    let mut e = Experience::candidate(clean(&problem), task.task_type.clone());
    e.project_key = task.project_key.clone();
    e.owner_id = task.owner_id.clone();
    e.scope = if !task.owner_id.is_empty() && task.project_key.is_empty() { ExperienceScope::User } else if !task.project_key.is_empty() { ExperienceScope::Project } else { ExperienceScope::Global };
    e.technologies = task.technologies.clone();
    e.context_summary = clean(&format!("{} task using {}", task.task_type, if task.technologies.is_empty() { "general tools".to_string() } else { task.technologies.join(", ") }));
    e.important_steps = task.steps.iter().take(6).map(|s| clean(&truncate(s, 140))).collect();
    e.tools_used = dedup(&task.tools_used);
    e.constraints = task.constraints.iter().take(4).map(|c| clean(&truncate(c, 140))).collect();
    e.approach = clean(&truncate(&e.important_steps.join("; "), 400));
    e.solution_summary = clean(&truncate(&task.result_summary, 240));
    e.result = e.solution_summary.clone();
    e.success = true;
    let v = task.verification.clone().unwrap_or_else(Verification::none);
    e.verification_method = v.method;
    e.verification_result = clean(&truncate(&v.result, 200));
    e.verified = v.verified;
    e.last_verified_at = v.timestamp.max(now - 1);
    e.confidence = initial_confidence(&v);
    e.source_task_id = task.task_id.clone();
    e.project_version = task.project_version.clone();
    e.experience_type = if !task.failed_attempts.is_empty() {
        ExperienceType::ErrorFix
    } else if e.important_steps.len() >= 3 {
        ExperienceType::Procedure
    } else {
        ExperienceType::Solution
    };
    // A failure that was corrected: keep what was tried, why it failed, and what worked. Only
    // because the correction was verified (this branch is only reached for a verified success).
    if let Some(f) = task.failed_attempts.first() {
        e.failed_approach = Some(clean(&truncate(&f.approach, 200)));
        e.failure_reason = Some(clean(&truncate(&f.reason, 200)));
        e.correction = Some(e.approach.clone());
    }
    if redactions > 0 {
        store.log_event("experience_sanitized", &serde_json::json!({ "redactions": redactions }), None);
    }
    e.embedding_model = provider.model().to_string();
    e.embedding_version = provider.version();
    e.quality_score = quality_score(&e, now, settings.stale_after_days);
    if e.quality_score < MIN_QUALITY_TO_STORE {
        return reject(store, task, &format!("quality {:.2} is below {:.2}", e.quality_score, MIN_QUALITY_TO_STORE));
    }
    e.status = next_status(&e, settings, now);

    // 4. Duplicate check: update the closest existing experience instead of adding another.
    let embedding = provider.embed(&embedding_text(&e))?;
    let viewer = Viewer { project_key: task.project_key.clone(), owner_id: task.owner_id.clone() };
    let rows = store.visible_with_embeddings(&viewer)?;
    let index = BruteForceIndex::build(&rows, provider);
    for (id, sim) in index.search(&embedding, 5) {
        let Some((existing, _)) = rows.iter().find(|(x, _)| x.id == id) else { continue };
        if !same_kind(existing, &e) {
            continue;
        }
        // Essentially the same problem, or a reasonably close one solved the same way (same tools,
        // equivalent approach). The second rule matters because the built-in embedding rarely scores
        // a reworded question above 0.85.
        let same_problem = sim >= DUPLICATE_SIMILARITY;
        let same_way = sim >= SAME_APPROACH_PROBLEM_SIMILARITY && same_approach(provider, existing, &e)?;
        if same_problem || same_way {
            return merge_into(store, settings, existing.clone(), e, embedding, sim, now, practice);
        }
    }

    // 5. New experience.
    let id = store.insert(&e, &embedding)?;
    store.bump("created", 1.0)?;
    store.set_counter("last_success_at", now as f64)?;
    store.log_event("experience_created", &serde_json::json!({ "id": id, "type": e.experience_type.as_str(), "status": e.status.as_str(), "quality": e.quality_score }), None);
    Ok(LearningOutcome::Created(id))
}

/// Both used the same set of tools and describe an equivalent approach.
fn same_approach(provider: &dyn EmbeddingProvider, a: &Experience, b: &Experience) -> Result<bool, String> {
    let mut ta: Vec<String> = a.tools_used.iter().map(|t| t.to_ascii_lowercase()).collect();
    let mut tb: Vec<String> = b.tools_used.iter().map(|t| t.to_ascii_lowercase()).collect();
    ta.sort();
    tb.sort();
    if ta != tb || a.approach.trim().is_empty() || b.approach.trim().is_empty() {
        return Ok(false);
    }
    Ok(cosine(&provider.embed(&a.approach)?, &provider.embed(&b.approach)?) >= APPROACH_SIMILARITY)
}

fn same_kind(a: &Experience, b: &Experience) -> bool {
    (a.task_type == b.task_type || a.task_type.is_empty() || b.task_type.is_empty())
        && (a.tools_used.is_empty() || b.tools_used.is_empty() || a.tools_used.iter().any(|t| b.tools_used.iter().any(|u| u.eq_ignore_ascii_case(t))))
}

/// Fold a new run into the experience it duplicates. The better approach wins; the history is
/// preserved in the event log and the version number; evidence and confidence accumulate.
#[allow(clippy::too_many_arguments)]
fn merge_into(store: &ExperienceStore, settings: &LearningSettings, mut existing: Experience, new: Experience, embedding: Vec<f32>, sim: f32, now: i64, practice: bool) -> Result<LearningOutcome, String> {
    let previous_approach = existing.approach.clone();
    let previous_version = existing.experience_version;
    let better = new.quality_score > existing.quality_score + 0.02 || (!existing.verified && new.verified);
    if better {
        existing.approach = new.approach.clone();
        existing.important_steps = new.important_steps.clone();
        existing.tools_used = new.tools_used.clone();
        existing.solution_summary = new.solution_summary.clone();
        existing.result = new.result.clone();
        if new.failed_approach.is_some() {
            existing.failed_approach = new.failed_approach.clone();
            existing.failure_reason = new.failure_reason.clone();
            existing.correction = new.correction.clone();
            existing.experience_type = ExperienceType::ErrorFix;
        }
        existing.experience_version += 1;
    }
    // Stronger evidence replaces weaker; a fresh verification refreshes the clock either way.
    if new.verification_method.weight() > existing.verification_method.weight() || !existing.verified {
        existing.verification_method = new.verification_method;
        existing.verification_result = new.verification_result.clone();
        existing.verified = existing.verified || new.verified;
    }
    existing.last_verified_at = now;
    // Seen again with the same result: a little more confidence, never past what evidence supports.
    // Not when the agent set the task itself: finding its own solution again proves nothing new.
    if !practice {
        existing.confidence = (existing.confidence + (1.0 - existing.confidence) * 0.10).min(0.95);
    }
    existing.technologies = dedup(&existing.technologies.iter().chain(new.technologies.iter()).cloned().collect::<Vec<_>>());
    if !existing.status.is_retired() {
        existing.status = next_status(&existing, settings, now);
    }
    existing.quality_score = quality_score(&existing, now, settings.stale_after_days);
    existing.updated_at = now;
    store.update(&existing, if better { Some(&embedding) } else { None })?;
    store.bump("updated", 1.0)?;
    store.bump("duplicates_prevented", 1.0)?;
    store.set_counter("last_success_at", now as f64)?;
    store.log_event(
        "experience_updated",
        &serde_json::json!({ "id": existing.id, "similarity": sim, "approach_replaced": better, "previous_version": previous_version, "previous_approach": truncate(&previous_approach, 200), "version": existing.experience_version }),
        None,
    );
    Ok(LearningOutcome::Updated(existing.id))
}

fn record_reuse_outcome(store: &ExperienceStore, settings: &LearningSettings, used: &UsedExperience, success: bool, goal: &str, practice: bool) -> Result<(), String> {
    let Some(mut e) = store.get(used.experience_id)? else { return Ok(()) };
    if practice && success {
        // A re-check that passed: it still works, so it is not stale. Nothing else moves: no reuse
        // count, no confidence, no promotion. Only real use by a person earns trust.
        let now = now_secs();
        e.last_verified_at = now;
        e.last_used_at = now;
        e.updated_at = now;
        store.update(&e, None)?;
        if let Some(usage_id) = used.usage_id {
            store.finish_usage(usage_id, true, "practice re-check passed")?;
        }
        return Ok(());
    }
    let before = e.status;
    apply_reuse_outcome(&mut e, success, settings);
    store.update(&e, None)?;
    if let Some(usage_id) = used.usage_id {
        store.finish_usage(usage_id, success, &truncate(goal, 80))?;
    }
    if e.status != before && e.status == ExperienceStatus::Deprecated {
        store.log_event("experience_deprecated", &serde_json::json!({ "id": e.id, "reason": "repeated failed reuse" }), None);
    }
    Ok(())
}

fn reject(store: &ExperienceStore, task: &TaskRecord, why: &str) -> Result<LearningOutcome, String> {
    store.bump("rejected", 1.0)?;
    store.log_event("experience_rejected_for_storage", &serde_json::json!({ "task_id": task.task_id, "reason": why }), None);
    Ok(LearningOutcome::Rejected(why.to_string()))
}

fn dedup(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(i)) {
            out.push(i.clone());
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}
