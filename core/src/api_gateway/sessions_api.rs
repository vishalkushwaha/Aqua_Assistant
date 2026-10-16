//! Sessions over runs, run bundles, and the records of coding sessions over HTTP (card OA10-04; tasks UX-10, AG-16, CX-08, CX-12 and AG-06 of the OpenCode audit).
//!
//! * `GET /api/v1/sessions?q=&limit=&cursor=&directory=`: the sessions, newest first: `{ sessions, next }`. `q` is looked for in what the runs were asked and what they answered, never in
//!   what a tool returned. `GET /api/v1/sessions/{id}`: one session with its runs. `GET /api/v1/sessions/{id}/export?format=json|md&sanitize=1`: the session as a file for a person to keep or send
//!   (`agent_runs::bundle`); sanitized, the text of every tool result and of the arguments of every call is gone, and so is every path outside the project.
//!   The UI token reads every session; the API token reads **only the sessions whose every run it asked for** (another is `404`, as for the log of a run).
//! * `PATCH /api/v1/sessions/{id}` gives a session a title, `DELETE /api/v1/sessions/{id}` deletes it for good (every row of its runs, its search entries, its coding record: the
//!   database overwrites what it deletes), `POST /api/v1/sessions/{id}/fork` makes a new, empty session from one of its runs, and `POST /api/v1/sessions/import` and `POST /api/v1/runs/import`
//!   read a file that was exported (a run or a session) in again, read-only. **The UI token only.** What comes in is masked again, limited in depth and size, never replayed.
//! * `GET /api/v1/runs/{id}/bundle`: one run as a file (UI token): see `agent_runs::bundle`.
//! * `GET /api/v1/coding/sessions?project=`, `POST /api/v1/coding/sessions/{id}/close` and `DELETE /api/v1/coding/sessions/{id}`: the records that `AQUA_CODING_SESSIONS` keeps of what coding
//!   runs in a project came to (file names, the last check, the model's own notes; never file contents). UI token only: the record is the person's.
//! * `GET /api/v1/plans`: the plans that plan runs wrote and the person has not answered (UI token).

use super::auth::{ui_scope_required, Scope};
use super::server::api_error;
use crate::agent_runs::bundle::{self, ImportError};
use crate::agent_runs::sessions::{self, DeleteOutcome, SessionQuery};
use crate::agent_runs::{self, AgentRunStore};
use crate::modules::coding_session_record as record;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// The largest file a person may import: a bundle is at most 2 MB, a session export holds up to fifty.
const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;
/// The longest search text.
const MAX_QUERY_CHARS: usize = 200;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/sessions", get(list_sessions))
        .route("/api/v1/sessions/import", post(import_sessions).layer(axum::extract::DefaultBodyLimit::max(MAX_IMPORT_BYTES)))
        .route("/api/v1/sessions/:session_id", get(get_session).patch(rename_session).delete(delete_session))
        .route("/api/v1/sessions/:session_id/fork", post(fork_session).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/sessions/:session_id/export", get(export_session))
        .route("/api/v1/runs/:run_id/bundle", get(run_bundle))
        .route("/api/v1/runs/import", post(import_run).layer(axum::extract::DefaultBodyLimit::max(MAX_IMPORT_BYTES)))
        .route("/api/v1/coding/sessions", get(list_coding_sessions))
        .route("/api/v1/coding/sessions/:session_id/close", post(close_coding_session))
        .route("/api/v1/coding/sessions/:session_id", delete(delete_coding_session))
        .route("/api/v1/plans", get(plans_waiting))
}

fn store() -> Result<&'static Arc<AgentRunStore>, Response> {
    agent_runs::store().ok_or_else(|| api_error(StatusCode::SERVICE_UNAVAILABLE, "agent runs are not being recorded (the run database could not be opened)"))
}

fn no_such_session(id: &str) -> Response {
    api_error(StatusCode::NOT_FOUND, &format!("no session called {id}"))
}

/// The session a request names (`session_id` of an agent request), checked: `Ok(None)` when it names none, a `400` for a name that is not one (1 to 64 letters, digits and `_ . : -`), and a
/// `404` when the API token names a session that has runs it did not ask for (it may name a session that does not exist yet, which is then made by the run; the UI token may name any).
pub(super) fn named_session(named: Option<&str>, ui: bool) -> Result<Option<String>, Response> {
    let Some(id) = named.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if !sessions::valid_session_id(id) {
        return Err(api_error(StatusCode::BAD_REQUEST, "session_id is 1 to 64 letters, digits and _ . : -"));
    }
    if !ui {
        if let Some(store) = agent_runs::store() {
            if store.session_exists(id) && !store.session_is_api_own(id) {
                return Err(no_such_session(id));
            }
        }
    }
    Ok(Some(id.to_string()))
}

/// May `scope` read session `id`? The UI token any session; the API token only one whose every run it asked for. The answer for a session it may not read is the one for a session that is
/// not there.
fn may_read(store: &AgentRunStore, scope: Scope, id: &str) -> bool {
    scope == Scope::Ui || store.session_is_api_own(id)
}

#[derive(Deserialize)]
struct ListQuery {
    q: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    directory: Option<String>,
}

/// The sessions, newest first, 50 a page (200 at most): `{ sessions: [{ id, title, directory, runs, started_at, updated_at, going, imported, forked_from }], next }`; ask again with `cursor=<next>` for the
/// following page. The UI token reads every session, the API token the sessions whose every run it asked for.
async fn list_sessions(axum::Extension(scope): axum::Extension<Scope>, Query(query): Query<ListQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let q: String = query.q.as_deref().unwrap_or("").chars().take(MAX_QUERY_CHARS).collect();
    let asked = SessionQuery { q: &q, directory: query.directory.as_deref().unwrap_or(""), limit: query.limit.unwrap_or(50), cursor: query.cursor.as_deref(), api_own_only: scope != Scope::Ui };
    match store.list_sessions(&asked) {
        Ok((sessions, next)) => Json(json!({ "sessions": sessions, "next": next })).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// One session: its title, folder, runs (as the list of runs shows them), and the run it was forked from when it is a fork. Same readers as the list.
async fn get_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !may_read(store, scope, &id) {
        return no_such_session(&id);
    }
    match store.get_session(&id) {
        Ok(Some(mut session)) => {
            // The record of the coding session that has this id, as the UI token may read it (the API token is told only that there is one).
            let row = store.coding_session(&id);
            session["coding_session"] = match (&row, scope == Scope::Ui) {
                (Some(row), true) => row.view(),
                (Some(row), false) => json!({ "id": row.id, "status": row.status, "updated_at": row.updated_at }),
                (None, _) => Value::Null,
            };
            Json(session).into_response()
        }
        Ok(None) => no_such_session(&id),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(Deserialize)]
struct RenameRequest {
    /// The new title; an empty one takes the title away (the first sentence of the first goal is the title again). At most 120 characters.
    title: String,
}

/// Gives a session a title (the UI token).
async fn rename_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>, Json(body): Json<RenameRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    if body.title.chars().count() > 400 {
        return api_error(StatusCode::BAD_REQUEST, "a title is at most 120 characters");
    }
    let title = sessions::clean_title(&body.title);
    match store.rename_session(&id, &title, agent_runs::now_ms()) {
        Ok(true) => Json(json!({ "id": id, "title": if title.is_empty() { Value::Null } else { json!(title) } })).into_response(),
        Ok(false) => no_such_session(&id),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Deletes a session for good (the UI token): every run of it with its steps, events, questions, records and search entries, and its coding record. A session with a run that is going is `409`.
async fn delete_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.delete_session(&id) {
        Ok(DeleteOutcome::Deleted(runs)) => Json(json!({ "id": id, "deleted_runs": runs })).into_response(),
        Ok(DeleteOutcome::Going) => api_error(StatusCode::CONFLICT, "a run of this session is still going: stop it first, then delete the session"),
        Ok(DeleteOutcome::NotFound) => no_such_session(&id),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(Deserialize, Default)]
struct ForkRequest {
    /// The run to fork from; the newest run of the session when none is named.
    #[serde(default)]
    from_run_id: Option<String>,
}

/// Makes a new, empty session from a run of this one (the UI token): `{ session_id, forked_from }`. The first run started in it (`session_id` in the request of an agent run) is told what that
/// run was asked and what it answered, as data.
async fn fork_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>, body: Option<Json<ForkRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let from_run = body.and_then(|Json(b)| b.from_run_id);
    match store.fork_session(&id, from_run.as_deref(), agent_runs::now_ms()) {
        Ok(Some((session, run))) => (StatusCode::CREATED, Json(json!({ "session_id": session, "forked_from": { "session_id": id, "run_id": run } }))).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "there is no such session, or no such run in it"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
    sanitize: Option<String>,
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("1" | "true" | "yes" | "on"))
}

/// The session as a file, `format=json` (the default: a session export of `agent_runs::bundle`) or `format=md` (a transcript to read), with `sanitize=1` taking the text of what the tools did and
/// the paths outside the project out. A local download only: nothing is sent anywhere. Same readers as the list.
async fn export_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>, Query(q): Query<ExportQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !may_read(store, scope, &id) {
        return no_such_session(&id);
    }
    let sanitize = truthy(q.sanitize.as_deref());
    let Some(export) = bundle::export_session(store, &id, sanitize, agent_runs::now_ms()) else { return no_such_session(&id) };
    match q.format.as_deref().unwrap_or("json") {
        "json" => Json(export).into_response(),
        "md" => (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")], bundle::transcript_md(&export)).into_response(),
        _ => api_error(StatusCode::BAD_REQUEST, "format is json or md"),
    }
}

/// One run as a file, with its steps (masked as the record keeps them), what it held, its model and its tokens (the UI token). At most 2 MB: the text of the oldest steps goes first.
async fn run_bundle(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match bundle::export_run(store, &run_id, agent_runs::now_ms()) {
        Some(b) => Json(b).into_response(),
        None => api_error(StatusCode::NOT_FOUND, &format!("no run called {run_id}")),
    }
}

fn refusal(e: &ImportError) -> Response {
    let status = match e {
        ImportError::Bad(_) => StatusCode::BAD_REQUEST,
        ImportError::TooBig => StatusCode::PAYLOAD_TOO_LARGE,
        ImportError::Exists(_) => StatusCode::CONFLICT,
        ImportError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, &e.message())
}

/// Reads a file that was exported (a run bundle or a session export) in, read-only (the UI token). `{ imported: [run ids], session_id }`; the runs are `imported` and are never resumed,
/// continued, answered or counted; a run that is there already refuses the whole file (409); every string is masked again.
fn import_file(file: Value) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match bundle::import(store, &file, agent_runs::now_ms()) {
        Ok((ids, session)) => (StatusCode::CREATED, Json(json!({ "imported": ids, "session_id": session }))).into_response(),
        Err(e) => refusal(&e),
    }
}

/// Reads a session export (or a run bundle) in, read-only (the UI token): see `import_run`.
async fn import_sessions(axum::Extension(scope): axum::Extension<Scope>, Json(file): Json<Value>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    import_file(file)
}

/// Reads a run bundle (or a session export) in, read-only (the UI token): `{ imported: [run ids], session_id }`. The runs are `imported`: shown as data, never started again.
async fn import_run(axum::Extension(scope): axum::Extension<Scope>, Json(file): Json<Value>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    import_file(file)
}

#[derive(Deserialize)]
struct CodingQuery {
    /// A folder of a project (or the key of one): the sessions of that project only.
    project: Option<String>,
    limit: Option<u32>,
}

/// The records of coding sessions, newest first (the UI token): `{ sessions: [{ id, project, status, started_at, updated_at, record }] }`. With `project=<folder>` only that project's. A record
/// holds the goals, the names of the files, what the last check said, the plan and the notes of the model: no file contents.
async fn list_coding_sessions(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<CodingQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let key = q.project.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(|p| record::project_key(std::path::Path::new(p)));
    match store.list_coding_sessions(key.as_deref(), q.limit.unwrap_or(50)) {
        Ok(rows) => Json(json!({ "sessions": rows.iter().map(|r| r.view()).collect::<Vec<_>>() })).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Closes a coding session (the UI token): its record stays, closed, to list or delete, and no run is told it. `409` for a session that is closed already.
async fn close_coding_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.close_coding_session(&id) {
        Ok(true) => Json(json!({ "id": id, "status": "closed" })).into_response(),
        Ok(false) if store.coding_session(&id).is_some() => api_error(StatusCode::CONFLICT, "this coding session is closed already"),
        Ok(false) => api_error(StatusCode::NOT_FOUND, &format!("no coding session called {id}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Deletes the record of a coding session for good (the UI token): the file is overwritten where the record was. The runs of the session stay.
async fn delete_coding_session(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.forget_coding_session(&id) {
        Ok(true) => Json(json!({ "id": id, "deleted": true })).into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, &format!("no coding session called {id}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// The plans that plan runs wrote and nobody has answered yet, newest first (the UI token): `{ plans: [{ run_id, goal, plan, at }] }`. A plan is open for a day; `POST /api/v1/runs/{id}/plan/approve`
/// starts the build from it and `POST /api/v1/runs/{id}/plan/revise` asks for another.
async fn plans_waiting(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let store = match store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.plans_waiting(agent_runs::now_ms(), 50) {
        Ok(plans) => Json(json!({ "plans": plans })).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}
