//! What a coding view reads, over HTTP (card OA10-06; tasks UX-14 and UX-16 of the OpenCode audit). The screen itself is the app's; these are the data it is built from.
//!
//! * `GET /api/v1/runs/{id}/transcript?after=`: the events of a run folded into the rows of a conversation (`agent_runs::transcript`): what was asked, each phase, each call with how it
//!   ended and (for the UI token) what it was given and returned, each question to the person and how it ended, the answer, the limit that stopped the run, the end, and a summary for the
//!   card at the end (status, elapsed time, tokens, calls, files changed, the last check). The whole log is folded each time, so the rows are the same however the client got here; `after`
//!   is the number of the last event the client has, and only the rows that an event after it made or changed are sent (`last` is the number to ask with next). The UI token reads every
//!   run; the API token the runs it asked for (another is `404`), and without the fields that show what a call touched or what was asked.
//! * `GET /api/v1/runs/{id}/readback?item=`: what the run changed, said the way a person would say it (`readback`): a summary, a sentence for every file and every change, and with `item=N` the
//!   line-by-line form of change `N`. Nothing is read aloud by this route; it is the text that "read the diff" says, for a screen that shows it or a client that speaks it. Masked.
//!   UI token only: it holds the contents of the person's files. It reads what `GET /api/v1/runs/{id}/diff` reads, so it has something to say only for a run that has a checkpoint
//!   (`AQUA_SNAPSHOT=on`) or whose commands the journal recorded.
//! * `GET /api/v1/coding/staged` and `DELETE /api/v1/coding/staged/{id}`: the cards that a spoken "show me the diff" or "undo the last run" put on the screen, and putting one away. A card
//!   holds the run and a sentence and does nothing by itself: the person presses the button in the app, which asks `POST /api/v1/runs/{id}/undo` with the UI token. UI token only.
//! * `GET /api/v1/coding/terms?folder=`: the words of the project that would be given to the speech model of a dictation clip (`project_terms`), so that the person sees them before they
//!   are used. The folder is one Aqua may already work in; without one it is the folder of the newest session. UI token only.

use super::auth::{ui_scope_required, Scope};
use super::server::api_error;
use crate::agent_runs::{self, transcript};
use crate::{coding_voice, project_terms, readback};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

/// How many events of one run are read to fold its transcript.
const MAX_EVENTS: usize = 20_000;
const PAGE: u32 = 1_000;

/// Tells `coding_voice` where a run's diff is read from (the diff route's own reader). Called once, when the gateway starts.
pub fn install() {
    coding_voice::install_diff_reader(super::route_api::run_diff_body);
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/runs/:run_id/transcript", get(run_transcript))
        .route("/api/v1/runs/:run_id/readback", get(run_readback))
        .route("/api/v1/coding/staged", get(staged_list))
        .route("/api/v1/coding/staged/:id", delete(staged_dismiss))
        .route("/api/v1/coding/terms", get(coding_terms))
}

fn now_ms() -> i64 {
    agent_runs::now_ms()
}

fn not_recording() -> Response {
    api_error(StatusCode::SERVICE_UNAVAILABLE, "agent runs are not being recorded (the run database could not be opened)")
}

#[derive(Deserialize)]
struct TranscriptQuery {
    /// The number of the last event the client has (0: from the start).
    after: Option<i64>,
}

/// The number of the last event that made or changed a row.
fn row_end(row: &Value) -> i64 {
    row["seq_end"].as_i64().or_else(|| row["seq"].as_i64()).unwrap_or(0)
}

/// The whole log of a run (up to [`MAX_EVENTS`] events), oldest first.
fn read_whole_log(store: &agent_runs::AgentRunStore, run_id: &str) -> Result<Vec<agent_runs::events::Stored>, String> {
    let mut all: Vec<agent_runs::events::Stored> = Vec::new();
    let mut after = 0i64;
    while all.len() < MAX_EVENTS {
        let page = store.events_after(run_id, after, PAGE)?;
        let more = page.len() as u32 >= PAGE;
        if let Some(last) = page.last() {
            after = last.seq;
        }
        all.extend(page);
        if !more {
            break;
        }
    }
    Ok(all)
}

/// The events of a run folded into the rows of a conversation, with a summary for its last card: `{ run_id, rows, summary, last, state, finished, rows_dropped }`. The UI token reads every run;
/// the API token the runs it asked for (another is `404`), without what a call was given or returned and without the text of a question.
async fn run_transcript(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Query(q): Query<TranscriptQuery>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = super::run_api::readable(store, scope, &run_id) {
        return response;
    }
    let (ui, after) = (scope == Scope::Ui, q.after.unwrap_or(0).max(0));
    let (store, id) = (store.clone(), run_id.clone());
    match tokio::task::spawn_blocking(move || read_whole_log(&store, &id).map(|events| transcript::fold(&events, ui))).await {
        Ok(Ok(folded)) => {
            let rows: Vec<Value> = folded.rows.into_iter().filter(|r| row_end(r) > after).collect();
            Json(json!({ "run_id": run_id, "rows": rows, "summary": folded.summary, "last": folded.last_seq, "state": folded.state, "finished": folded.finished, "rows_dropped": folded.rows_dropped })).into_response()
        }
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the transcript stopped unexpectedly"),
    }
}

#[derive(Deserialize)]
struct ReadbackQuery {
    /// The number of a change in `items` (0 is the first): its line-by-line form is added to the answer.
    item: Option<usize>,
}

/// The text of a readback, from the body of the diff route: a summary, a line for every file and every change, and (for `item`) the change spelled line by line. Every patch is masked before
/// it is made into words, and what is said once more.
pub fn readback_body(run_id: &str, diff: &Value, item: Option<usize>) -> Value {
    let mut changes = readback::changes_of(diff);
    for c in &mut changes {
        c.patch = coding_voice::mask_spoken(&c.patch);
    }
    let items = readback::items(&changes);
    let files: Vec<Value> = changes
        .iter()
        .map(|c| {
            let hunks = readback::hunks_of(&c.patch);
            json!({
                "path": c.path,
                "status": c.status.to_string(),
                "added": hunks.iter().map(readback::Hunk::added).sum::<usize>(),
                "removed": hunks.iter().map(readback::Hunk::removed).sum::<usize>(),
                "hunks": hunks.len(),
                "truncated": c.truncated,
                "binary": c.binary,
                "path_spoken": readback::say_path(&c.path),
                "spoken": coding_voice::mask_spoken(&readback::describe_file(c)),
            })
        })
        .collect();
    let spelled = item.and_then(|i| items.get(i).and_then(|it| it.hunk.as_ref()).map(|h| json!({ "item": i, "text": coding_voice::mask_spoken(&readback::spell_hunk(h)) })));
    json!({
        "run_id": run_id,
        "available": !changes.is_empty(),
        "checkpoints_on": diff["checkpoints_on"],
        "error": diff["error"],
        "summary": coding_voice::mask_spoken(&readback::summarise(&changes)),
        "files": files,
        "items": items.iter().enumerate().map(|(i, it)| json!({ "item": i, "path": it.path, "spoken": coding_voice::mask_spoken(&it.sentence), "can_spell": it.hunk.is_some() })).collect::<Vec<_>>(),
        "spelled": spelled,
    })
}

/// What the run changed, said the way a person would say it: a summary, a sentence for every file and every change, and with `item=N` change `N` line by line. Nothing is read aloud by this
/// route. It reads what the diff route reads, so it has something to say for a run with a checkpoint or a journal record of its commands only. The UI token: it holds the contents of the
/// person's files.
async fn run_readback(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Query(q): Query<ReadbackQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store() else { return not_recording() };
    match store.log_head(&run_id) {
        Ok(Some(_)) => {}
        Ok(None) => return api_error(StatusCode::NOT_FOUND, &format!("no run called {run_id}")),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
    let id = run_id.clone();
    match tokio::task::spawn_blocking(move || super::route_api::run_diff_body(&id)).await {
        Ok(diff) => Json(readback_body(&run_id, &diff, q.item)).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the readback stopped unexpectedly"),
    }
}

/// The cards that a spoken "show me the diff" or "undo the last run" put on the screen and the person has not answered: `{ staged: [{ id, kind, run_id, text, created_ms, expires_ms, how }] }`.
/// A card does nothing by itself: the person presses the button in the app. The UI token.
async fn staged_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(json!({ "staged": coding_voice::staged().list(now_ms()), "note": "A card waits ten minutes. Nothing is undone until you press the button in the app, which asks POST /api/v1/runs/{id}/undo with the UI token." })).into_response()
}

/// Puts a card away. The UI token.
async fn staged_dismiss(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if coding_voice::staged().dismiss(&id) {
        Json(json!({ "dismissed": id })).into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "there is no such card (it may have expired)")
    }
}

#[derive(Deserialize)]
struct TermsQuery {
    folder: Option<String>,
}

/// The words of a project and the prompt they make, from a walk of its folder.
pub fn terms_body(dir: &std::path::Path) -> Value {
    let found = project_terms::collect(dir);
    json!({
        "project": aqua_native_tools::fs::guard::display(dir),
        "terms": found.terms,
        "prompt": project_terms::prompt_of(&found.terms),
        "files_seen": found.files_seen,
        "skipped_secret": found.skipped_secret,
        "truncated": found.truncated,
        "used_for_dictation": coding_voice::project_terms_on(),
    })
}

/// The words of the project the person works in that would be given to the speech model of a dictation clip (file names, package and dependency names, the names the newest source files
/// declare; never a secret-looking file, line or word), with the prompt they make. The folder is one Aqua may already work in; without one it is the folder of the newest session. The UI token.
async fn coding_terms(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<TermsQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let dir: Option<PathBuf> = match q.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(raw) => {
            let roots: Vec<PathBuf> = match crate::tool_host::PROJECT_GUARD.get() {
                Some(g) => g.roots().into_iter().map(|(_, p, _)| p).collect(),
                None => vec![crate::runtime::workspace_root()],
            };
            let candidate = PathBuf::from(raw);
            let allowed = candidate.is_dir() && roots.iter().any(|r| aqua_native_tools::fs::guard::within(&candidate, r));
            if !allowed {
                return api_error(StatusCode::BAD_REQUEST, "the folder must be an existing folder inside the folders Aqua works in");
            }
            Some(candidate)
        }
        None => project_terms::current_project(),
    };
    let Some(dir) = dir else {
        return Json(json!({ "project": null, "terms": [], "prompt": "", "files_seen": 0, "skipped_secret": 0, "truncated": false, "used_for_dictation": coding_voice::project_terms_on(), "note": "There is no project yet: name a folder, or start a coding run in one." })).into_response();
    };
    match tokio::task::spawn_blocking(move || terms_body(&dir)).await {
        Ok(body) => Json(body).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the walk of the project stopped unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -38,5 +38,5 @@ fn load(path: &str) -> Config {\n     let text = read(path);\n     let n = 1;\n-    let cfg = parse(&text);\n+    let cfg = parse_strict(&text);\n     check(&cfg);\n     cfg";

    /// Card OA10-06 (UX-14, UX-16): the readback of a run is a summary, a line for every file and change, and one change spelled on request; a key in a patch is not in any of it.
    #[test]
    fn the_readback_says_the_changes_in_words_and_masks_what_it_says() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let diff = json!({
            "run_id": "r1",
            "checkpoints_on": true,
            "checkpoint": { "files": [
                { "path": "src/main.rs", "status": "M", "patch": PATCH, "truncated": false, "binary": false },
                { "path": "src/keys.rs", "status": "M", "patch": format!("--- a/k\n+++ b/k\n@@ -1 +1 @@\n-let k = 1;\n+let k = \"{secret}\";"), "truncated": false, "binary": false },
            ] },
            "commands": [],
            "error": null
        });
        let body = readback_body("r1", &diff, Some(0));
        assert_eq!(body["available"], true);
        assert_eq!(body["summary"], "2 files changed, 2 lines added and 2 removed: main dot R S and keys dot R S.");
        assert_eq!(body["files"][0]["spoken"], "main dot R S: replaced the call to parse with parse strict in function load, line 40.");
        assert_eq!((body["files"][0]["added"].as_u64(), body["files"][0]["removed"].as_u64(), body["files"][0]["hunks"].as_u64()), (Some(1), Some(1), Some(1)));
        assert_eq!(body["files"][0]["path_spoken"], "src slash main dot R S");
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["items"][0]["can_spell"], true);
        assert!(body["spelled"]["text"].as_str().unwrap().starts_with("line 40 removed: let cfg equals parse open paren"), "{}", body["spelled"]);
        let all = body.to_string();
        assert!(!all.contains(secret) && !all.contains("abcdefghijklmnopqrstuvwxyz"), "{all}");
        // The second change spelled: the key is a mask, not words.
        let second = readback_body("r1", &diff, Some(1));
        assert!(!second.to_string().contains("ghp") && second["spelled"]["text"].as_str().unwrap().contains("REDACTED"), "{}", second["spelled"]);
        // An item that is not there spells nothing.
        assert!(readback_body("r1", &diff, Some(9))["spelled"].is_null());
    }

    #[test]
    fn a_run_with_no_record_of_its_changes_has_nothing_to_read_and_says_whether_checkpoints_are_on() {
        let body = readback_body("r2", &json!({ "run_id": "r2", "checkpoints_on": false, "checkpoint": null, "commands": [], "error": null }), None);
        assert_eq!((body["available"].as_bool(), body["checkpoints_on"].as_bool(), body["summary"].as_str()), (Some(false), Some(false), Some(readback::NOTHING)));
        assert!(body["files"].as_array().unwrap().is_empty() && body["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_rows_after_a_number_are_the_rows_an_event_after_it_made_or_changed() {
        assert_eq!(row_end(&json!({ "seq": 3 })), 3);
        assert_eq!(row_end(&json!({ "seq": 3, "seq_end": 9 })), 9);
        assert_eq!(row_end(&json!({})), 0);
    }

    #[test]
    fn the_words_of_a_project_are_shown_with_the_prompt_they_make() {
        let dir = std::env::temp_dir().join(format!("aqua_coding_terms_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("speech_stream.rs"), "pub struct SentenceSplitter {}\n").unwrap();
        std::fs::write(dir.join(".env"), "TOKEN=1").unwrap();
        let body = terms_body(&dir);
        let terms: Vec<&str> = body["terms"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(terms.contains(&"SentenceSplitter") && terms.contains(&"speech_stream"), "{terms:?}");
        assert_eq!(body["prompt"].as_str().unwrap(), terms.join(", "));
        assert_eq!(body["skipped_secret"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
