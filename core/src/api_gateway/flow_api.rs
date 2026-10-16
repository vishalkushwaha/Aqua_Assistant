//! HTTP API for AI Flows: `/api/v1/flows/*`.
//!
//! Reading, validating and diffing need the API token. Creating, saving, restoring, importing and
//! deleting need the UI token, like workflows and policy: a flow will run tools by itself later, so a
//! script holding only the API token must not be able to plant one. Every request is for the local
//! owner; the store filters on it.

use super::auth::{ui_scope_required, Scope};
use crate::flows::diff::diff;
use crate::flows::model::{current_owner, FlowDef};
use crate::flows::registry;
use crate::flows::store::{now, FlowStore};
use crate::flows::templates;
use crate::flows::validate::validate;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

// The stores are the fields of `crate::flows::services::FlowServices` (card OA13-03, task AQ-12). The functions below keep their names and their values for the routes, the tests and the
// code that was written to reach them here: each is one call to the stores of the process.

/// Where flows, their runs and their history live. `AQUA_FLOWS_DB` moves it (tests use this so they
/// never touch real data).
pub fn db_path() -> PathBuf {
    crate::flows::services::db_path()
}

/// The process-wide store.
pub fn store() -> Arc<FlowStore> {
    crate::flows::services::global().flows()
}

/// The process-wide run store (the same database file).
pub fn run_store() -> Arc<crate::flows::run::store::RunStore> {
    crate::flows::services::global().runs()
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/flows", get(list).post(create))
        .route("/api/v1/flows/node-types", get(node_types))
        .route("/api/v1/flows/n8n/health", get(n8n_health))
        .route("/api/v1/flows/templates", get(template_list))
        .route("/api/v1/flows/validate", post(validate_draft))
        .route("/api/v1/flows/import", post(import))
        .route("/api/v1/flows/plan", post(plan_request))
        .route("/api/v1/flows/plan/apply", post(plan_apply))
        .route("/api/v1/flows/:id/chat", get(chat))
        .route("/api/v1/flows/inbox", get(inbox))
        .route("/api/v1/flows/runs/:rid", get(run_detail))
        .route("/api/v1/flows/runs/:rid/events", get(run_events))
        .route("/api/v1/flows/runs/:rid/stream", get(run_stream))
        .route("/api/v1/flows/runs/:rid/artifact", get(run_artifact))
        .route("/api/v1/flows/runs/:rid/respond", post(run_respond))
        .route("/api/v1/flows/runs/:rid/nodes/:nid/:action", post(node_action))
        .route("/api/v1/flows/runs/:rid/:action", post(run_control))
        .route("/api/v1/flows/:id/run", post(start_run))
        .route("/api/v1/flows/:id/runs", get(list_flow_runs))
        .route("/api/v1/flows/:id", get(get_one).put(save))
        .route("/api/v1/flows/:id/delete", post(delete))
        .route("/api/v1/flows/:id/validate", get(validate_stored))
        .route("/api/v1/flows/:id/versions", get(versions))
        .route("/api/v1/flows/:id/versions/:v", get(get_version))
        .route("/api/v1/flows/:id/restore/:v", post(restore))
        .route("/api/v1/flows/:id/diff", get(diff_versions))
        .route("/api/v1/flows/:id/export", get(export))
        .route("/api/v1/flows/:id/audit", get(audit))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn internal(e: String) -> Response {
    tracing::warn!("flow api: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "the flow store could not complete that request")
}

/// A store error is a fault, a version conflict, or something the person wrote.
fn store_error(e: String) -> Response {
    if e.starts_with("flow store:") {
        internal(e)
    } else if e.starts_with("conflict:") {
        error(StatusCode::CONFLICT, e)
    } else {
        error(StatusCode::BAD_REQUEST, e)
    }
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such flow")
}

async fn list() -> Response {
    match store().list(current_owner()) {
        Ok(flows) => Json(json!({ "flows": flows })).into_response(),
        Err(e) => internal(e),
    }
}

async fn node_types() -> Response {
    Json(json!({ "node_types": registry::all(), "platforms": registry::platforms(), "platform_actions": registry::platform_actions() })).into_response()
}

async fn template_list() -> Response {
    Json(json!({ "templates": templates::list() })).into_response()
}

/// Create from a template (`{"template": "social_post", "name": "..."}`) or from a definition.
async fn create(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<Value>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let def = match body.get("template").and_then(|t| t.as_str()) {
        Some(id) => match templates::build(id) {
            Some(mut d) => {
                if let Some(name) = body.get("name").and_then(|n| n.as_str()).filter(|n| !n.trim().is_empty()) {
                    d.name = name.to_string();
                }
                d
            }
            None => return error(StatusCode::BAD_REQUEST, format!("there is no template called \"{id}\"")),
        },
        None => match serde_json::from_value::<FlowDef>(body) {
            Ok(d) => d,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("that is not a flow: {e}")),
        },
    };
    match store().create(current_owner(), def, "user", "", now()) {
        Ok(f) => (StatusCode::CREATED, Json(f)).into_response(),
        Err(e) => store_error(e),
    }
}

/// A flow from a file (export from another Aqua). Refused if it holds anything that looks like a secret.
async fn import(axum::Extension(scope): axum::Extension<Scope>, Json(def): Json<FlowDef>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let report = validate(&def);
    if let Some(bad) = report.issues.iter().find(|i| i.code == "secret_in_config") {
        return error(StatusCode::BAD_REQUEST, format!("not imported: {}", bad.message));
    }
    match store().create(current_owner(), def, "user", "imported", now()) {
        Ok(f) => (StatusCode::CREATED, Json(f)).into_response(),
        Err(e) => store_error(e),
    }
}

async fn get_one(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(f)) => Json(f).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct SaveBody {
    def: FlowDef,
    /// The version the editor started from. If the flow has moved on, the save is refused.
    expected_version: Option<i64>,
    #[serde(default)]
    note: String,
}

async fn save(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(body): Json<SaveBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().update(current_owner(), id, body.def, "user", body.note.trim(), body.expected_version, now()) {
        Ok(Some(f)) => Json(f).into_response(),
        Ok(None) => not_found(),
        Err(e) => store_error(e),
    }
}

async fn delete(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().delete(current_owner(), id, "user", now()) {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

/// Check a draft that is not saved yet (what is on the canvas).
async fn validate_draft(Json(mut def): Json<FlowDef>) -> Response {
    def.normalize();
    Json(validate(&def)).into_response()
}

async fn validate_stored(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(f)) => Json(validate(&f.def)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn versions(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match store().versions(current_owner(), id) {
        Ok(v) => Json(json!({ "versions": v })).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_version(Path((id, v)): Path<(i64, i64)>) -> Response {
    match store().version(current_owner(), id, v) {
        Ok(Some(def)) => Json(def).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such version"),
        Err(e) => internal(e),
    }
}

async fn restore(axum::Extension(scope): axum::Extension<Scope>, Path((id, v)): Path<(i64, i64)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match store().restore(current_owner(), id, v, "user", now()) {
        Ok(Some(f)) => Json(f).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such flow or version"),
        Err(e) => store_error(e),
    }
}

#[derive(Deserialize)]
struct DiffQuery {
    from: i64,
    to: i64,
}

async fn diff_versions(Path(id): Path<i64>, Query(q): Query<DiffQuery>) -> Response {
    let s = store();
    let (a, b) = match (s.version(current_owner(), id, q.from), s.version(current_owner(), id, q.to)) {
        (Ok(Some(a)), Ok(Some(b))) => (a, b),
        (Ok(_), Ok(_)) => return error(StatusCode::NOT_FOUND, "no such flow or version"),
        (Err(e), _) | (_, Err(e)) => return internal(e),
    };
    Json(diff(&a, &b)).into_response()
}

async fn export(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(f)) => {
            let filename = format!("attachment; filename=\"flow-{}.json\"", f.id);
            let mut resp = Json(f.def).into_response();
            if let Ok(v) = filename.parse() {
                resp.headers_mut().insert(axum::http::header::CONTENT_DISPOSITION, v);
            }
            resp
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

// ---- running flows ------------------------------------------------------------------------------------

fn runner_or_503() -> Result<Arc<crate::flows::run::manager::FlowRunner>, Response> {
    crate::flows::run::manager::runner().ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "the flow runner is not running"))
}

/// A refusal from the runner is either "there is no such thing" or "that does not make sense now".
fn run_error(e: String) -> Response {
    if e.starts_with("flow store:") {
        internal(e)
    } else if e.starts_with("no such") {
        error(StatusCode::NOT_FOUND, e)
    } else {
        error(StatusCode::BAD_REQUEST, e)
    }
}

#[derive(Deserialize)]
struct RunBody {
    /// The flow's inputs (`{"topic": "AI safety"}`); anything left out takes its default.
    #[serde(default)]
    inputs: serde_json::Map<String, Value>,
    /// Simulate everything outside Aqua.
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    payload: Value,
    /// A key for this start: the same key with the same request (inputs and dry run) starts nothing new, and the answer is the run it started the first time, with
    /// `duplicate: true`. A retry after a timeout cannot make the run twice. The same key with another request is refused (422). Remembered for a week.
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// Start a run. It goes on in the background; the answer is the new run, and its progress is read from
/// `/runs/:rid`, `/runs/:rid/events` or the stream. With an `idempotency_key`, a retry gives back the run of the first start (`duplicate: true`) and starts nothing.
async fn start_run(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, body: Option<Json<RunBody>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let runner = match runner_or_503() {
        Ok(r) => r,
        Err(e) => return e,
    };
    let b = body.map(|Json(b)| b).unwrap_or(RunBody { inputs: Default::default(), dry_run: false, payload: Value::Null, idempotency_key: None });
    // (A key that starts with `schedule:` is the scheduler's own, for a time that came due: a caller's key may not be one.)
    if b.idempotency_key.as_deref().is_some_and(|k| !crate::queue::valid_idempotency_key(k) || k.starts_with("schedule:")) {
        return error(StatusCode::BAD_REQUEST, "an idempotency key is 1 to 128 letters, digits and _ . : - and does not start with schedule:");
    }
    let started = tokio::task::spawn_blocking(move || {
        // The request, for a key: the inputs and whether it is a dry run (the payload is the caller's note and not part of what was asked for).
        let fingerprint = format!("{}|{}", aqua_tool_host::canonical_json(&Value::Object(b.inputs.clone())), b.dry_run);
        runner.start_keyed(current_owner(), id, b.inputs, b.payload, b.dry_run, "manual", b.idempotency_key.as_deref().map(|k| (k, fingerprint.as_str())))
    })
    .await;
    match started {
        Ok(Ok((run, duplicate))) => (if duplicate { StatusCode::OK } else { StatusCode::ACCEPTED }, Json(json!({ "run": crate::flows::run::model::Run { def: Default::default(), ..run }, "duplicate": duplicate }))).into_response(),
        Ok(Err(e)) if e == crate::flows::run::store::KEY_MISMATCH => error(StatusCode::UNPROCESSABLE_ENTITY, e),
        Ok(Err(e)) => run_error(e),
        Err(_) => internal("the runner stopped unexpectedly".into()),
    }
}

async fn list_flow_runs(Path(id): Path<i64>, Query(q): Query<AuditQuery>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match run_store().list_runs(current_owner(), Some(id), q.limit.unwrap_or(30)) {
        Ok(runs) => Json(json!({ "runs": runs })).into_response(),
        Err(e) => internal(e),
    }
}

/// One run with every step's state: what the canvas and the run panel draw.
async fn run_detail(Path(rid): Path<i64>) -> Response {
    let s = run_store();
    match s.get_run(current_owner(), rid) {
        Ok(Some(run)) => match s.node_runs(rid) {
            Ok(nodes) => Json(json!({ "run": run, "nodes": nodes })).into_response(),
            Err(e) => internal(e),
        },
        Ok(None) => error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<u32>,
}

async fn run_events(Path(rid): Path<i64>, Query(q): Query<EventsQuery>) -> Response {
    let s = run_store();
    match s.get_run(current_owner(), rid) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => return internal(e),
    }
    match s.events(current_owner(), rid, q.after.unwrap_or(0), q.limit.unwrap_or(200)) {
        Ok(events) => Json(json!({ "events": events })).into_response(),
        Err(e) => internal(e),
    }
}

/// Live events of a run as server-sent events. It ends when the run has finished; a run that is only
/// waiting stays open (an approval may come). `?after=<event id>` resumes where a client left off.
async fn run_stream(Path(rid): Path<i64>, Query(q): Query<EventsQuery>) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let s = run_store();
    match s.get_run(current_owner(), rid) {
        Ok(Some(_)) => {}
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such run"),
        Err(e) => return internal(e),
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let mut after = q.after.unwrap_or(0);
    tokio::spawn(async move {
        loop {
            // A client that has gone is noticed here as well as on a send: a run that waits for an approval sends nothing but its status, and
            // the status send below did not check, so a closed tab left two database queries every half second for as long as the wait lasted.
            if tx.is_closed() {
                return;
            }
            let s2 = s.clone();
            let batch = tokio::task::spawn_blocking(move || {
                let events = s2.events(current_owner(), rid, after, 200).unwrap_or_default();
                let status = s2.get_run(current_owner(), rid).ok().flatten().map(|r| r.status);
                (events, status)
            })
            .await;
            let Ok((events, status)) = batch else { return };
            let had = !events.is_empty();
            for e in events {
                after = e.id;
                let ev = Event::default().event("event").id(e.id.to_string()).data(serde_json::to_string(&e).unwrap_or_default());
                if tx.send(Ok(ev)).await.is_err() {
                    return; // the client went away
                }
            }
            if let Some(st) = status {
                if tx.send(Ok(Event::default().event("status").data(st.as_str()))).await.is_err() {
                    return; // the client went away
                }
                if st.is_finished() && !had {
                    let _ = tx.send(Ok(Event::default().event("end").data("finished"))).await;
                    return;
                }
            } else {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Deserialize)]
struct RespondBody {
    node: String,
    #[serde(flatten)]
    response: crate::flows::run::model::Response,
}

/// Answer a step that is waiting: approve or reject (with an edit), answer a question, choose what to do about a failure.
async fn run_respond(axum::Extension(scope): axum::Extension<Scope>, Path(rid): Path<i64>, Json(body): Json<RespondBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let runner = match runner_or_503() {
        Ok(r) => r,
        Err(e) => return e,
    };
    let done = tokio::task::spawn_blocking(move || runner.respond(current_owner(), rid, &body.node, body.response)).await;
    match done {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(e)) => run_error(e),
        Err(_) => internal("the runner stopped unexpectedly".into()),
    }
}

/// pause | resume | stop | restart
async fn run_control(axum::Extension(scope): axum::Extension<Scope>, Path((rid, action)): Path<(i64, String)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let runner = match runner_or_503() {
        Ok(r) => r,
        Err(e) => return e,
    };
    let done = tokio::task::spawn_blocking(move || match action.as_str() {
        "pause" => runner.pause(current_owner(), rid).map(|_| json!({ "ok": true })),
        "resume" => runner.resume(current_owner(), rid).map(|_| json!({ "ok": true })),
        "stop" => runner.stop(current_owner(), rid).map(|_| json!({ "ok": true })),
        "restart" => runner.restart(current_owner(), rid).map(|r| json!({ "run": crate::flows::run::model::Run { def: Default::default(), ..r } })),
        other => Err(format!("no such action \"{other}\"")),
    })
    .await;
    match done {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => run_error(e),
        Err(_) => internal("the runner stopped unexpectedly".into()),
    }
}

/// retry | skip | run-from, on one step of a run.
async fn node_action(axum::Extension(scope): axum::Extension<Scope>, Path((rid, nid, action)): Path<(i64, String, String)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let runner = match runner_or_503() {
        Ok(r) => r,
        Err(e) => return e,
    };
    let done = tokio::task::spawn_blocking(move || match action.as_str() {
        "retry" => runner.retry_node(current_owner(), rid, &nid).map(|_| json!({ "ok": true })),
        "skip" => runner.skip_node(current_owner(), rid, &nid).map(|_| json!({ "ok": true })),
        "run-from" => runner.run_from(current_owner(), rid, &nid).map(|r| json!({ "run": crate::flows::run::model::Run { def: Default::default(), ..r } })),
        other => Err(format!("no such action \"{other}\"")),
    })
    .await;
    match done {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => run_error(e),
        Err(_) => internal("the runner stopped unexpectedly".into()),
    }
}

/// Everything waiting for a person, across all flows.
async fn inbox() -> Response {
    match run_store().inbox(current_owner()) {
        Ok(items) => Json(json!({ "items": items })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- the AI builder --------------------------------------------------------------------------------

#[derive(Deserialize)]
struct PlanBody {
    request: String,
    /// The flow to change. Absent means: build a new one.
    flow_id: Option<i64>,
    /// Answers to questions an earlier proposal asked (`{"topic": "AI safety"}`).
    #[serde(default)]
    answers: std::collections::HashMap<String, String>,
}

/// Propose a flow (or a change to one) from a sentence. Nothing is stored: the answer is the proposal.
/// The language model may be used, which can take a minute or more on this computer; the work runs off
/// the request thread.
async fn plan_request(Json(body): Json<PlanBody>) -> Response {
    let request = body.request.trim().to_string();
    if request.is_empty() || request.chars().count() > 2000 {
        return error(StatusCode::BAD_REQUEST, "write what you want in 1 to 2000 characters");
    }
    let s = store();
    let owner = current_owner();
    let base = match body.flow_id {
        Some(id) => match s.get(owner, id) {
            Ok(Some(f)) => Some(f),
            Ok(None) => return not_found(),
            Err(e) => return internal(e),
        },
        None => None,
    };
    let history: Vec<(String, String)> = match &base {
        Some(f) => s.chat(owner, f.id, 12).unwrap_or_default().into_iter().map(|m| (m.role, m.text)).collect(),
        None => vec![],
    };
    let answers = body.answers;
    let req = request.clone();
    let base2 = base.clone();
    let proposal = tokio::task::spawn_blocking(move || {
        let model = crate::flows::planner::installed();
        crate::flows::planner::plan(&req, base2.as_ref(), &answers, chrono::Local::now().naive_local(), model.as_deref(), &history)
    })
    .await;
    let Ok(proposal) = proposal else { return internal("the planner stopped unexpectedly".into()) };
    if let Some(f) = &base {
        let summary = proposal.explanation.first().cloned().unwrap_or_else(|| "I could not make a change from that.".into());
        let _ = s.chat_add(owner, f.id, "user", &request, now());
        let _ = s.chat_add(owner, f.id, "assistant", &summary, now());
    }
    Json(proposal).into_response()
}

#[derive(Deserialize)]
struct ApplyBody {
    flow_id: Option<i64>,
    /// The version the proposal was made against; if the flow has moved on, nothing is applied.
    expected_version: Option<i64>,
    commands: Vec<crate::flows::commands::Command>,
    #[serde(default)]
    title: String,
    /// The repeating schedule the proposal asked for; created as a real schedule once the change is saved.
    #[serde(default)]
    schedule: Option<crate::flows::planner::ScheduleSuggestion>,
}

/// Apply the commands of an accepted proposal. The commands are applied again here, to the flow as it is
/// now, and checked: a new flow must be valid, and a change must not add problems the flow did not have.
/// The result is saved as a new version by "ai", so it can be undone by restoring the one before.
async fn plan_apply(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<ApplyBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let s = store();
    let owner = current_owner();
    let base = match body.flow_id {
        Some(id) => match s.get(owner, id) {
            Ok(Some(f)) => Some(f),
            Ok(None) => return not_found(),
            Err(e) => return internal(e),
        },
        None => None,
    };
    if let (Some(f), Some(v)) = (&base, body.expected_version) {
        if f.version != v {
            return error(StatusCode::CONFLICT, format!("conflict: this flow is now at version {} but the proposal was made for version {v}; ask again", f.version));
        }
    }
    let blank = crate::flows::model::FlowDef::default();
    let base_def = base.as_ref().map(|f| &f.def).unwrap_or(&blank);
    let applied = match crate::flows::commands::apply(base_def, &body.commands) {
        Ok(a) => a,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let after = validate(&applied.def);
    let before = validate(base_def);
    let codes = |r: &crate::flows::validate::Report| r.errors().map(|i| (i.code.clone(), i.node.clone())).collect::<Vec<_>>();
    let introduced: Vec<String> = codes(&after).into_iter().filter(|e| base.is_none() || !codes(&before).contains(e)).map(|(c, n)| format!("{c}{}", n.map(|n| format!(" at {n}")).unwrap_or_default())).collect();
    if !introduced.is_empty() {
        return error(StatusCode::BAD_REQUEST, format!("not applied: the result would have problems ({})", introduced.join(", ")));
    }
    let title: String = body.title.trim().chars().take(120).collect();
    let saved = match &base {
        Some(f) => s.update(owner, f.id, applied.def, "ai", &title, Some(f.version), now()).map(|o| o.map(|x| (x, Some(f.version)))),
        None => s.create(owner, applied.def, "ai", &title, now()).map(|x| Some((x, None))),
    };
    match saved {
        Ok(Some((flow, previous))) => {
            let _ = s.chat_add(owner, flow.id, "assistant", &format!("Applied: {}", if title.is_empty() { "the change" } else { &title }), now());
            // The schedule the person asked for in words becomes a real one now that the flow is saved. If it cannot be
            // made (or already exists) the change still stands, and the answer says so.
            let (schedule, schedule_note) = match &body.schedule {
                Some(sg) => {
                    let store = super::flow_sched_api::sched_store();
                    let existing = store.list(owner, Some(flow.id)).unwrap_or_default().into_iter().find(|x| matches!(&x.when, crate::flows::sched::model::When::Cron { cron } if *cron == sg.cron) && x.timezone == sg.timezone);
                    match existing {
                        Some(x) => (Some(x), Some("It was already scheduled that way, so no second schedule was made.".to_string())),
                        None => {
                            let input = crate::flows::sched::model::ScheduleInput {
                                name: format!("{} schedule", flow.def.name).chars().take(80).collect(),
                                when: crate::flows::sched::model::When::Cron { cron: sg.cron.clone() },
                                timezone: sg.timezone.clone(),
                                inputs: Default::default(),
                                misfire: Default::default(),
                                notify: Default::default(),
                                enabled: true,
                                campaign: crate::flows::sched::model::Campaign { end_on: sg.end_on.clone(), max_runs: sg.max_runs, series: vec![] },
                            };
                            match store.create(owner, flow.id, &input, now()) {
                                Ok(x) => (Some(x), None),
                                Err(e) => (None, Some(format!("The flow was saved, but the schedule could not be made: {e}"))),
                            }
                        }
                    }
                }
                None => (None, None),
            };
            (StatusCode::OK, Json(json!({ "flow": flow, "previous_version": previous, "notes": applied.notes, "schedule": schedule, "schedule_note": schedule_note }))).into_response()
        }
        Ok(None) => not_found(),
        Err(e) => store_error(e),
    }
}

async fn chat(Path(id): Path<i64>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match store().chat(current_owner(), id, 100) {
        Ok(m) => Json(json!({ "messages": m })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<u32>,
}

async fn audit(Path(id): Path<i64>, Query(q): Query<AuditQuery>) -> Response {
    match store().get(current_owner(), id) {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match store().audit_log(current_owner(), Some(id), q.limit.unwrap_or(50)) {
        Ok(a) => Json(json!({ "audit": a })).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct ArtifactQuery {
    node: String,
    #[serde(default)]
    key: Option<String>,
}

/// The image a step made, for the run panel. Only a file inside Aqua's generated-images folder that the run's own
/// step recorded as its output is ever served; nothing is read from a path a caller supplies.
async fn run_artifact(Path(rid): Path<i64>, Query(q): Query<ArtifactQuery>) -> Response {
    let out = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let runs = run_store();
        runs.get_run(current_owner(), rid)?.ok_or("no such run")?;
        let n = runs.node_run(rid, &q.node)?.ok_or("no such step")?;
        let recorded = n.outputs.get(q.key.as_deref().unwrap_or("image")).and_then(|v| v.as_str()).ok_or("that step made no image")?.to_string();
        let dir = std::fs::canonicalize(crate::flows::image::output_dir()).map_err(|_| "no such image".to_string())?;
        let file = std::fs::canonicalize(&recorded).map_err(|_| "no such image".to_string())?;
        if !file.starts_with(&dir) || file.extension().and_then(|e| e.to_str()) != Some("png") {
            return Err("no such image".into());
        }
        std::fs::read(&file).map_err(|_| "no such image".into())
    })
    .await;
    match out {
        Ok(Ok(bytes)) => ([(axum::http::header::CONTENT_TYPE, "image/png"), (axum::http::header::CACHE_CONTROL, "private, max-age=3600")], bytes).into_response(),
        Ok(Err(e)) if e.starts_with("no such") || e.starts_with("that step") => error(StatusCode::NOT_FOUND, e),
        Ok(Err(e)) => internal(e),
        Err(_) => internal("interrupted".into()),
    }
}

/// Is n8n set up and reachable? Read-only: it asks n8n's own health address and sends nothing else.
async fn n8n_health() -> Response {
    match tokio::task::spawn_blocking(|| crate::flows::n8n::health(crate::flows::n8n::configured_base(), std::time::Duration::from_secs(5))).await {
        Ok(h) => Json(h).into_response(),
        Err(_) => internal("the health check was interrupted".into()),
    }
}
