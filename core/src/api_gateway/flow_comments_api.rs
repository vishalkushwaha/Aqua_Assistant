//! HTTP API for the comment inbox: `/api/v1/flows/comments/drafts/*`.
//!
//! Reading needs the API token. Approving (which posts a public reply), skipping and resolving need the UI token: a
//! reply is public and cannot be taken back, so a script holding only the API token must not be able to send one.

use super::auth::{ui_scope_required, Scope};
use crate::flows::comments_inbox::CommentInbox;
use crate::flows::model::current_owner;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// The comment inbox of the process: a field of `crate::flows::services::FlowServices` (card OA13-03, task AQ-12).
pub fn inbox() -> Arc<CommentInbox> {
    crate::flows::services::global().comments()
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/flows/comments/drafts", get(list))
        .route("/api/v1/flows/comments/drafts/:id/approve", post(approve))
        .route("/api/v1/flows/comments/drafts/:id/skip", post(skip))
        .route("/api/v1/flows/comments/drafts/:id/resolve", post(resolve))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn fail(e: String) -> Response {
    if e.starts_with("flow store:") {
        tracing::warn!("comment inbox: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the comment inbox could not complete that request" }))).into_response()
    } else if e.starts_with("no such") {
        (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response()
    } else if e.starts_with("that draft is not waiting") || e.starts_with("only a reply") {
        (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response()
    } else {
        (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
    }
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<u32>,
}

async fn list(Query(q): Query<ListQuery>) -> Response {
    let inbox = inbox();
    match (inbox.list(current_owner(), q.status.as_deref(), q.limit.unwrap_or(100)), inbox.counts(current_owner())) {
        (Ok(d), Ok(c)) => Json(json!({ "drafts": d, "counts": c })).into_response(),
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

#[derive(Deserialize, Default)]
struct ApproveBody {
    /// The reply as the person wants it (their edit). Left out, the draft is sent as written.
    #[serde(default)]
    text: Option<String>,
}

/// Approve a draft and post it. Answers when the browser has finished, with how it ended.
async fn approve(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, body: Option<Json<ApproveBody>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(browser) = crate::flows::comments_browser::get() else { return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the browser is not available" }))).into_response() };
    let text = body.and_then(|Json(b)| b.text);
    let out = tokio::task::spawn_blocking(move || inbox().approve_and_post(browser.as_ref(), current_owner(), id, text.as_deref(), now())).await;
    match out {
        Ok(Ok(d)) => Json(json!({ "draft": d })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

async fn skip(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match inbox().skip(current_owner(), id, now()) {
        Ok(d) => Json(json!({ "draft": d })).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ResolveBody {
    /// "posted" (you checked: it went out) or "retry" (it did not; send it again).
    action: String,
}

async fn resolve(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(b): Json<ResolveBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match inbox().resolve(current_owner(), id, &b.action, now()) {
        Ok(d) => Json(json!({ "draft": d })).into_response(),
        Err(e) => fail(e),
    }
}
