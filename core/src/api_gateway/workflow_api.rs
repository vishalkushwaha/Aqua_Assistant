//! HTTP API for workflow automation: `/api/v1/workflows/*`.
//!
//! Reading needs the API token. Creating, changing, enabling, running by hand, cancelling and
//! deleting need the UI token, like approvals and policy: a workflow runs tools by itself later, so a
//! script holding only the API token must not be able to plant one. The one exception is
//! `POST /:id/trigger`, which the API token may call, and only for a workflow whose trigger a person
//! set to `webhook`.
//!
//! Every request is for the local owner (`current_owner()`); the store filters on it.

use super::auth::{ui_scope_required, Scope};
use crate::workflows::model::{current_owner, Trigger, WorkflowInput};
use crate::workflows::runner::{scheduler, store};
use crate::workflows::schedule;
use crate::workflows::store::now;
use axum::{
    body::Bytes,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/workflows", get(list).post(create))
        .route("/api/v1/workflows/schedule/preview", post(preview))
        .route("/api/v1/workflows/runs/:run_id", get(get_run))
        .route("/api/v1/workflows/runs/:run_id/cancel", post(cancel_run))
        .route("/api/v1/workflows/:id", get(get_one).put(update))
        .route("/api/v1/workflows/:id/delete", post(delete))
        .route("/api/v1/workflows/:id/enable", post(enable))
        .route("/api/v1/workflows/:id/run", post(run_now))
        .route("/api/v1/workflows/:id/trigger", post(trigger))
        .route("/api/v1/workflows/:id/runs", get(list_runs))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Storage problems are reported without internals.
fn internal(e: String) -> Response {
    tracing::warn!("workflow api: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "the workflow store could not complete that request")
}

/// A store error is either something the person wrote (worth showing) or a storage fault.
fn store_error(e: String) -> Response {
    if e.starts_with("workflow store:") {
        internal(e)
    } else {
        error(StatusCode::BAD_REQUEST, e)
    }
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such workflow")
}

async fn list() -> Response {
    match store().list(current_owner()) {
        Ok(workflows) => Json(json!({ "workflows": workflows, "scheduler": scheduler().map(|s| s.status()) })).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_one(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(w)) => Json(w).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn create(axum::Extension(scope): axum::Extension<Scope>, Json(input): Json<WorkflowInput>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().create(current_owner(), &input, now()) {
        Ok(w) => (StatusCode::CREATED, Json(w)).into_response(),
        Err(e) => store_error(e),
    }
}

async fn update(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(input): Json<WorkflowInput>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().update(current_owner(), id, &input, now()) {
        Ok(Some(w)) => Json(w).into_response(),
        Ok(None) => not_found(),
        Err(e) => store_error(e),
    }
}

async fn delete(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().delete(current_owner(), id) {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct EnableBody {
    enabled: bool,
}

async fn enable(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(body): Json<EnableBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().set_enabled(current_owner(), id, body.enabled, now()) {
        Ok(Some(w)) => Json(w).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// Start a run in the background and answer at once with its id; the history shows how it went.
fn start(id: i64, kind: &str, payload: Value, must_be_webhook: bool) -> Response {
    let w = match store().get(current_owner(), id) {
        Ok(Some(w)) => w,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    if must_be_webhook && w.trigger != Trigger::Webhook {
        return error(StatusCode::FORBIDDEN, "this workflow does not accept webhook triggers; a person can set its trigger to webhook in the workflow editor");
    }
    if !w.enabled && kind != "manual" {
        return error(StatusCode::CONFLICT, "this workflow is switched off");
    }
    let Some(sched) = scheduler() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "the workflow scheduler is not running");
    };
    match sched.start(&w, kind, payload) {
        Ok(run_id) => (StatusCode::ACCEPTED, Json(json!({ "run_id": run_id }))).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

async fn run_now(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    start(id, "manual", Value::Null, false)
}

const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

async fn trigger(Path(id): Path<i64>, body: Bytes) -> Response {
    if body.len() > MAX_PAYLOAD_BYTES {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "the payload is at most 64 KB");
    }
    let payload = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v,
            Err(_) => return error(StatusCode::BAD_REQUEST, "the body must be JSON"),
        }
    };
    start(id, "webhook", payload, true)
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<u32>,
}

async fn list_runs(Path(id): Path<i64>, Query(q): Query<RunsQuery>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match store().list_runs(current_owner(), id, q.limit.unwrap_or(30)) {
        Ok(runs) => Json(json!({ "runs": runs })).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_run(Path(run_id): Path<i64>) -> Response {
    match store().get_run(current_owner(), run_id) {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => internal(e),
    }
}

async fn cancel_run(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // Only a run of this owner can be cancelled.
    match store().get_run(current_owner(), run_id) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => return internal(e),
    }
    let cancelled = scheduler().is_some_and(|s| s.cancel(run_id));
    Json(json!({ "cancelled": cancelled })).into_response()
}

#[derive(Deserialize)]
struct PreviewBody {
    cron: String,
    count: Option<usize>,
}

/// What a schedule means, as its next run times, so the editor can show them before saving.
async fn preview(Json(body): Json<PreviewBody>) -> Response {
    match schedule::preview(&body.cron, now(), body.count.unwrap_or(5).clamp(1, 10)) {
        Ok(runs) => Json(json!({ "runs": runs })).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}
