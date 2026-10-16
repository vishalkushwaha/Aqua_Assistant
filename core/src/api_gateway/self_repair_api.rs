//! HTTP API for self repair: `/api/v1/self-repair/*` (cards R7-05, R7-09 and R7-20).
//!
//! The gaps Aqua noticed (what it could not do, and why), the fixes it proposes for them, and the weekly report. Every route needs the UI token, reads included: a gap holds
//! what the person asked, and the report names the files and commands of agent runs. Nothing changes without a click: `approve` is the only route that applies a change, it
//! is served only to the app, and for a folder the tool host asks the person once more before anything is allowed. Every applied change can be undone from `undo`.
//! See `documentation/SELF_REPAIR.md` for the shapes of the answers.

use super::auth::{ui_scope_required, Scope};
use crate::flows::model::current_owner;
use crate::self_repair::{self, proposals, report, research, RepairError};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/self-repair/gaps", get(list_gaps))
        .route("/api/v1/self-repair/gaps/:id/dismiss", post(dismiss_gap))
        .route("/api/v1/self-repair/gaps/:id/propose", post(propose_for_gap))
        .route("/api/v1/self-repair/gaps/:id/research", post(research_for_gap))
        .route("/api/v1/self-repair/proposals", get(list_proposals))
        .route("/api/v1/self-repair/proposals/:id/approve", post(approve_proposal))
        .route("/api/v1/self-repair/proposals/:id/reject", post(reject_proposal))
        .route("/api/v1/self-repair/proposals/:id/undo", post(undo_proposal))
        .route("/api/v1/self-repair/report", get(weekly_report))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// A store error is a fault, a thing that is not there, or a request that makes no sense now.
fn fail(e: RepairError) -> Response {
    match e {
        RepairError::NotFound(m) => error(StatusCode::NOT_FOUND, m),
        RepairError::Refused(m) => error(StatusCode::CONFLICT, m),
        RepairError::Store(m) => {
            tracing::warn!("self repair api: {m}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the self-repair store could not complete that request")
        }
    }
}

fn stopped() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "the request did not finish")
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<u32>,
}

/// The things Aqua could not do, newest first: `{"gaps": [...]}`. `?status=` is `new`, `proposed`, `fixed`, `dismissed` or `open` (new and proposed); `?limit=` at most 500. The UI token.
async fn list_gaps(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ListQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if let Some(s) = q.status.as_deref() {
        if s != "open" && !self_repair::store::GAP_STATUSES.contains(&s) {
            return error(StatusCode::BAD_REQUEST, format!("\"{s}\" is not a status of a gap (new, proposed, fixed, dismissed or open)"));
        }
    }
    match self_repair::install().gaps(q.status.as_deref(), q.limit.unwrap_or(100)) {
        Ok(gaps) => Json(json!({ "gaps": gaps })).into_response(),
        Err(e) => fail(e),
    }
}

/// Dismisses a gap: the person does not want to see it again. Its pending proposals are rejected with it. The UI token.
async fn dismiss_gap(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match proposals::dismiss_gap(&self_repair::install(), id) {
        Ok(gap) => Json(json!({ "gap": gap })).into_response(),
        Err(e) => fail(e),
    }
}

/// Makes the fixes a gap suggests, as proposals that wait for a click: `{"proposals": [...], "note": ""}`. Nothing is applied. The UI token.
async fn propose_for_gap(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = self_repair::install();
    let out = tokio::task::spawn_blocking(move || {
        let flows = super::flow_api::store();
        let env = proposals::Env { flows: &flows, folders: &proposals::HostFolders, owner: current_owner(), now: now() };
        proposals::create_proposals_for(&store, &env, id)
    })
    .await;
    match out {
        Ok(Ok(p)) => Json(json!({ "proposals": p.proposals, "note": p.note })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => stopped(),
    }
}

/// Looks up a gap that says Aqua does not know how, and attaches the cited finding to it: `{"gap": {...}}` with the finding in `gap.finding`. It runs `research.run` through the tool host, so it needs `AQUA_RESEARCH=on`; with research off the answer is 409 with the reason and nothing is done. The finding is text from the web for the person to read: it is never followed and never turned into a proposal. The UI token.
async fn research_for_gap(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = self_repair::install();
    let out = tokio::task::spawn_blocking(move || research::research_gap(&store, &research::HostResearch, id, now())).await;
    match out {
        Ok(Ok(research::Researched::Done(gap))) => Json(json!({ "gap": gap })).into_response(),
        Ok(Ok(research::Researched::Disabled(why))) => (StatusCode::CONFLICT, Json(json!({ "error": why, "research": "off" }))).into_response(),
        Ok(Ok(research::Researched::Failed(why))) => error(StatusCode::BAD_GATEWAY, why),
        Ok(Err(e)) => fail(e),
        Err(_) => stopped(),
    }
}

#[derive(Deserialize)]
struct ProposalQuery {
    status: Option<String>,
    gap: Option<i64>,
    limit: Option<u32>,
}

/// The proposals, newest first: `{"proposals": [...]}`. `?status=` is `pending`, `approved`, `rejected`, `applied`, `undone` or `failed`; `?gap=` limits them to one gap. The UI token.
async fn list_proposals(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ProposalQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if let Some(s) = q.status.as_deref() {
        if !self_repair::store::PROPOSAL_STATUSES.contains(&s) {
            return error(StatusCode::BAD_REQUEST, format!("\"{s}\" is not a status of a proposal (pending, approved, rejected, applied, undone or failed)"));
        }
    }
    match self_repair::install().proposals(q.status.as_deref(), q.gap, q.limit.unwrap_or(100)) {
        Ok(p) => Json(json!({ "proposals": p })).into_response(),
        Err(e) => fail(e),
    }
}

/// Approves a pending proposal and applies it, once. The UI token only: the API token, a script, the voice and the model cannot approve. A folder is allowed only if the person answers the tool host's question as well; the request waits for that answer. A change that cannot be applied (the flow was edited since) is answered with 409 and the reason, and nothing is changed.
async fn approve_proposal(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = self_repair::install();
    let out = tokio::task::spawn_blocking(move || {
        let flows = super::flow_api::store();
        let env = proposals::Env { flows: &flows, folders: &proposals::HostFolders, owner: current_owner(), now: now() };
        proposals::approve(&store, &env, id, "ui")
    })
    .await;
    match out {
        Ok(Ok(p)) if p.status == "applied" => Json(json!({ "proposal": p })).into_response(),
        Ok(Ok(p)) => (StatusCode::CONFLICT, Json(json!({ "error": p.error, "proposal": p }))).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => stopped(),
    }
}

/// Rejects a pending proposal. Nothing is changed anywhere. The UI token.
async fn reject_proposal(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match proposals::reject(&self_repair::install(), id) {
        Ok(p) => Json(json!({ "proposal": p })).into_response(),
        Err(e) => fail(e),
    }
}

/// Takes an applied proposal back with the undo recorded when it was applied. A flow that was edited since is not touched (409 with the reason); a folder grant is taken back through the tool host (`fs.revoke_folder`). The UI token.
async fn undo_proposal(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = self_repair::install();
    let out = tokio::task::spawn_blocking(move || {
        let flows = super::flow_api::store();
        let env = proposals::Env { flows: &flows, folders: &proposals::HostFolders, owner: current_owner(), now: now() };
        proposals::undo(&store, &env, id)
    })
    .await;
    match out {
        Ok(Ok(p)) => Json(json!({ "proposal": p })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => stopped(),
    }
}

#[derive(Deserialize)]
struct ReportQuery {
    days: Option<u32>,
}

/// What Aqua did by itself in the last `?days=` days (7 by default, at most 90): what it noticed, looked up, proposed, applied with an approval, and undid, with the changes that have no undo listed apart and marked `no_undo`. The answer has the figures and a plain-text version in `text`. The UI token.
async fn weekly_report(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ReportQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let days = q.days.unwrap_or(report::DEFAULT_DAYS);
    let out = tokio::task::spawn_blocking(move || {
        let store = self_repair::install();
        let audit_path = crate::agent_tools::host().map(|h| h.audit().path().to_path_buf()).unwrap_or_else(|| crate::runtime::data_dir().join("tools").join("host.audit.jsonl"));
        // The undo store of the file tools: does it still hold what this agent run changed?
        let has_undo = |run: &str| crate::tool_host::TRASH.get().is_some_and(|t| !t.run_entries(run).is_empty());
        let verify = || aqua_tool_host::audit::verify_chain(&audit_path).map_err(|e| e.to_string());
        // A folder grant has an undo when the running host has the tool that takes it back (`fs.revoke_folder`); otherwise the report lists it as having none.
        let folders_can_be_taken_back = proposals::FolderHost::can_revoke(&proposals::HostFolders);
        report::build(&store, report::ReportInput { audit_path: &audit_path, days, now: now(), has_undo: &has_undo, folders_can_be_taken_back, verify_chain: Some(&verify) })
    })
    .await;
    match out {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => stopped(),
    }
}
