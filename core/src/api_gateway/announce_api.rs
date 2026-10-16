//! HTTP API for the sentences Aqua is to say on its own: a reminder that has come due (card R6-06, task T6-06.9).
//!
//! The `voice` channel of flows (`flows/notify_voice.rs`) puts the sentence in a queue; the app's player takes it here, says it with the speech engine, and tells whether it
//! did. It is modelled on the device announcements (`devices_api.rs`): `next` hands out the next thing to say (204 when there is nothing), `ack` says what became of it. Both need
//! the UI token, so a script holding only the API token cannot make Aqua talk, or take a reminder away before the person hears it. What to say is a reminder that the person asked
//! for, or the title of a notification they asked to have spoken; there is no route to put a sentence in the queue from outside. The player and the toast are the app's.

use super::auth::{ui_scope_required, Scope};
use crate::flows::notify_voice::queue;
use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/api/v1/announcements/next", post(post_next)).route("/api/v1/announcements/:id/ack", post(post_ack))
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NextRequest {
    /// The player is busy with an answer, or a task is thinking: nothing is handed out now (it stays in the queue).
    busy: bool,
}

/// The player asks for the next thing to say: `{"id", "text", "kind", "at"}`, or 204 when there is nothing it may say now. A sentence that is handed out is held for the player for
/// 45 seconds, and is offered again if the player does not answer. The UI token.
async fn post_next(axum::Extension(scope): axum::Extension<Scope>, body: Option<Json<NextRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let busy = body.is_some_and(|b| b.0.busy);
    let now = unix_now();
    // A player that is busy is still a player that is there (it connects the channel); it is just not given anything.
    let next = if busy {
        queue().note_player(now);
        None
    } else {
        queue().next(now)
    };
    match next {
        Some(a) => Json(json!({ "id": a.id, "text": a.text, "kind": a.kind, "at": a.at })).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
struct AckRequest {
    spoken: bool,
}

/// The player says what became of a sentence it was given: `{"spoken": true}` takes it out of the queue; `{"spoken": false}` (the speech could not be made) offers it again, up to
/// three times in all. 404 for a sentence that is not waiting (it was spoken already, or it was too old). The UI token.
async fn post_ack(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<u64>, Json(b): Json<AckRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if queue().ack(id, b.spoken, unix_now()) {
        Json(json!({ "ok": true })).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({ "error": "no such announcement is waiting (it may have been said already)" }))).into_response()
    }
}
