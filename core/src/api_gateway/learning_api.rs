//! HTTP API for the Agent Learning system: settings, status, statistics, experience management.
//!
//! Reading needs the API token. Anything that changes state (settings, approving, deprecating,
//! deleting, revalidating, reindexing) needs the UI token, like policy changes and approvals do, so
//! a script holding the API token cannot switch learning on or off or promote a memory.

use super::auth::{ui_scope_required, Scope};
use crate::memory::experience::model::{now_secs, Experience, ExperienceStatus, ExperienceType};
use crate::memory::experience::service::service;
use crate::memory::experience::settings::LearningSettings;
use crate::memory::experience::store::ListFilter;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/learning/settings", get(get_settings).put(put_settings))
        .route("/api/v1/learning/status", get(get_status))
        .route("/api/v1/learning/stats", get(get_stats))
        .route("/api/v1/learning/evaluation", get(get_evaluation))
        .route("/api/v1/learning/events", get(get_events))
        .route("/api/v1/learning/jobs", get(get_jobs))
        .route("/api/v1/learning/practice", get(get_practice))
        .route("/api/v1/learning/practice/run-now", post(practice_run_now))
        .route("/api/v1/learning/experiences", get(list_experiences))
        .route("/api/v1/learning/experiences/search", post(search_experiences))
        .route("/api/v1/learning/experiences/:id", get(get_experience))
        .route("/api/v1/learning/experiences/:id/status", post(set_status))
        .route("/api/v1/learning/experiences/:id/revalidate", post(revalidate))
        .route("/api/v1/learning/experiences/:id/delete", post(delete_experience))
        .route("/api/v1/learning/reindex", post(reindex))
        .route("/api/v1/learning/consolidate", post(consolidate))
        .route("/api/v1/learning/clear", post(clear))
        .route("/api/v1/learning/launch-skills", get(get_launch_skills))
        .route("/api/v1/learning/launch-skills/forget", post(forget_launch_skill))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Storage problems are reported without internals (and never with data from the request).
fn internal(e: String) -> Response {
    tracing::warn!("learning api: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "the learning store could not complete that request")
}

async fn get_settings() -> Response {
    Json(service().settings()).into_response()
}

async fn put_settings(axum::Extension(scope): axum::Extension<Scope>, Json(new): Json<LearningSettings>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match service().update_settings(&new) {
        Ok(saved) => Json(saved).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_status() -> Response {
    Json(service().status()).into_response()
}

async fn get_stats() -> Response {
    match service().stats() {
        Ok(stats) => {
            let success_rate = if stats.tasks_analyzed == 0 { 0.0 } else { stats.tasks_successful as f32 / stats.tasks_analyzed as f32 };
            let reuse_done = stats.successful_reuses + stats.failed_reuses;
            let reuse_success_rate = if reuse_done == 0 { 0.0 } else { stats.successful_reuses as f32 / reuse_done as f32 };
            let mut value = serde_json::to_value(&stats).unwrap_or(Value::Null);
            if let Some(o) = value.as_object_mut() {
                o.insert("task_success_rate".into(), json!(success_rate));
                o.insert("reuse_success_rate".into(), json!(reuse_success_rate));
            }
            Json(value).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn get_practice() -> Response {
    let status = crate::memory::experience::practice::status(&service(), &crate::modules::practice_runner::current_gate());
    let mut value = serde_json::to_value(&status).unwrap_or(Value::Null);
    if let Some(o) = value.as_object_mut() {
        o.insert("last_request".into(), json!(crate::modules::practice_runner::last_note()));
    }
    Json(value).into_response()
}

/// Ask for one practice task now. Skips the idle wait and the gap; the daily limit, the switches and
/// "never while a user's task is running" all still apply. Needs the UI token.
async fn practice_run_now(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if !service().settings().may_practice() {
        return error(StatusCode::BAD_REQUEST, "turn on Agent Learning and Auto practice first");
    }
    crate::modules::practice_runner::request_run_now();
    Json(json!({ "requested": true })).into_response()
}

async fn get_evaluation() -> Response {
    match service().store().evaluation() {
        Ok(ev) => Json(ev).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}

async fn get_events(Query(q): Query<LimitQuery>) -> Response {
    match service().store().recent_events(q.limit.unwrap_or(50)) {
        Ok(events) => Json(json!({ "events": events })).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_jobs(Query(q): Query<LimitQuery>) -> Response {
    match service().recent_jobs(q.limit.unwrap_or(30)) {
        Ok(jobs) => Json(json!({ "jobs": jobs })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    status: Option<String>,
    #[serde(rename = "type")]
    experience_type: Option<String>,
    project: Option<String>,
    q: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

/// One row of the experience list: enough to understand it without opening it.
#[derive(Serialize)]
struct ExperienceSummary {
    id: i64,
    problem_summary: String,
    experience_type: &'static str,
    approach_summary: String,
    status: &'static str,
    confidence: f32,
    quality_score: f32,
    verified: bool,
    verification_method: &'static str,
    created_at: i64,
    last_verified_at: i64,
    reuse_count: u32,
    success_rate: f32,
    scope: &'static str,
    project_key: String,
    human_approved: bool,
}

fn summarize(e: &Experience) -> ExperienceSummary {
    ExperienceSummary {
        id: e.id,
        problem_summary: e.problem_summary.clone(),
        experience_type: e.experience_type.as_str(),
        approach_summary: e.approach.chars().take(200).collect(),
        status: e.status.as_str(),
        confidence: e.confidence,
        quality_score: e.quality_score,
        verified: e.verified,
        verification_method: e.verification_method.as_str(),
        created_at: e.created_at,
        last_verified_at: e.last_verified_at,
        reuse_count: e.reuse_count,
        success_rate: e.reuse_success_rate(),
        scope: e.scope.as_str(),
        project_key: e.project_key.clone(),
        human_approved: e.human_approved,
    }
}

async fn list_experiences(Query(q): Query<ListQuery>) -> Response {
    let status = match q.status.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match ExperienceStatus::parse(s) {
            Some(v) => Some(v),
            None => return error(StatusCode::BAD_REQUEST, format!("unknown status {s:?}")),
        },
        None => None,
    };
    let experience_type = match q.experience_type.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match ExperienceType::parse(s) {
            Some(v) => Some(v),
            None => return error(StatusCode::BAD_REQUEST, format!("unknown type {s:?}")),
        },
        None => None,
    };
    let filter = ListFilter { status, experience_type, project_key: q.project.filter(|p| !p.is_empty()), text: q.q, limit: q.limit, offset: q.offset };
    match service().list(&filter) {
        Ok(list) => Json(json!({ "experiences": list.iter().map(summarize).collect::<Vec<_>>() })).into_response(),
        Err(e) => internal(e),
    }
}

/// Why this experience is (or is not) trusted, in plain sentences, from its own evidence.
fn why_trusted(e: &Experience) -> Vec<String> {
    let mut why = Vec::new();
    if e.verified {
        why.push(format!("Verified by {}: {}", e.verification_method.as_str().replace('_', " "), e.verification_result));
    } else {
        why.push("Not verified with real evidence, so it is not offered to the agent.".to_string());
    }
    if e.human_approved {
        why.push("Approved by a person.".to_string());
    }
    let done = e.successful_reuse_count + e.failed_reuse_count;
    if done > 0 {
        why.push(format!("Reused {} time(s): {} succeeded, {} failed ({:.0}% success).", done, e.successful_reuse_count, e.failed_reuse_count, e.reuse_success_rate() * 100.0));
    } else {
        why.push("Not reused yet, so its confidence rests on the original verification only.".to_string());
    }
    if e.last_verified_at > 0 {
        why.push(format!("Last verified {} day(s) ago.", (now_secs() - e.last_verified_at).max(0) / 86_400));
    }
    why.push(format!("Confidence {:.0}%, quality {:.0}%, status {}.", e.confidence * 100.0, e.quality_score * 100.0, e.status.as_str()));
    if e.status.is_retired() {
        why.push("It is retired and does not influence the agent.".to_string());
    }
    why
}

async fn get_experience(Path(id): Path<i64>) -> Response {
    let svc = service();
    match svc.store().get(id) {
        Ok(Some(e)) => {
            let usage = svc.store().usage_for(id, 50).unwrap_or_default();
            Json(json!({ "experience": e, "usage": usage, "why_trusted": why_trusted(&e) })).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such experience"),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct StatusBody {
    action: String,
}

async fn set_status(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(body): Json<StatusBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match service().set_status(id, &body.action) {
        Ok(e) => Json(summarize(&e)).into_response(),
        Err(msg) if msg.contains("not found") => error(StatusCode::NOT_FOUND, msg),
        Err(msg) if msg.contains("unknown action") || msg.contains("cannot be") || msg.contains("only an experience") => error(StatusCode::BAD_REQUEST, msg),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Default)]
struct RevalidateBody {
    /// The tools the agent has now. When omitted, only retired tools and instruction-like text are checked.
    #[serde(default)]
    available_tools: Vec<String>,
}

async fn revalidate(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, body: Option<Json<RevalidateBody>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let tools = body.map(|b| b.0.available_tools).unwrap_or_default();
    match service().revalidate(id, &tools) {
        Ok(report) => Json(report).into_response(),
        Err(msg) if msg.contains("not found") => error(StatusCode::NOT_FOUND, msg),
        Err(e) => internal(e),
    }
}

async fn delete_experience(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match service().delete(id) {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such experience"),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct SearchBody {
    text: String,
    limit: Option<usize>,
}

async fn search_experiences(Json(body): Json<SearchBody>) -> Response {
    if body.text.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is required");
    }
    match service().search(&body.text, body.limit.unwrap_or(10).min(50)) {
        Ok(hits) => Json(json!({ "results": hits.iter().map(|(e, sim)| json!({ "similarity": sim, "experience": summarize(e) })).collect::<Vec<_>>() })).into_response(),
        Err(e) => internal(e),
    }
}

async fn reindex(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match service().reindex_all() {
        Ok(report) => Json(report).into_response(),
        Err(e) => internal(e),
    }
}

async fn consolidate(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match service().consolidate() {
        Ok(report) => Json(report).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct ClearBody {
    /// Must be exactly `"delete all experiences"`: clearing memory is irreversible.
    confirm: String,
}

async fn clear(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<ClearBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if body.confirm != "delete all experiences" {
        return error(StatusCode::BAD_REQUEST, "send {\"confirm\": \"delete all experiences\"} to clear the whole memory");
    }
    match service().clear_all() {
        Ok(n) => Json(json!({ "deleted": n })).into_response(),
        Err(e) => internal(e),
    }
}

/// What Aqua has learned about starting apps: the way that worked for each name, how sure it is, how long it takes.
async fn get_launch_skills() -> Response {
    let settings = service().settings();
    let skills = aqua_mcp_adapters::app_launch::all_skills();
    let total_ok: u32 = skills.iter().map(|s| s.successes).sum();
    let total_failed: u32 = skills.iter().map(|s| s.failures).sum();
    Json(json!({
        "learning_enabled": settings.learning_enabled,
        "used_for_new_requests": settings.learning_enabled && settings.retrieval_enabled,
        "skills": skills,
        "totals": { "skills": skills.len(), "successes": total_ok, "failures": total_failed },
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ForgetSkill {
    /// The app's name as listed ("notepad"), or empty with `all: true`.
    #[serde(default)]
    key: String,
    #[serde(default)]
    all: bool,
}

async fn forget_launch_skill(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<ForgetSkill>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let removed = if body.all { aqua_mcp_adapters::app_launch::forget_all() } else { usize::from(aqua_mcp_adapters::app_launch::forget(&body.key)) };
    Json(json!({ "forgotten": removed })).into_response()
}
