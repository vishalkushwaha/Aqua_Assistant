//! The events of agent runs over HTTP (card OA10-01; tasks EX-03 and UX-02 of the OpenCode audit).
//!
//! A run writes its events to `data/agent_runs.db` as it goes (`agent_runs::events`, `agent_runs::Recording`); these routes read them back, in three ways:
//!
//! * `GET /api/v1/runs/{id}/events?after=&limit=`: a page, `{ run_id, events, last, finished }`. Poll it with `after=<last>` until `finished` is true.
//! * `GET /api/v1/runs/{id}/usage`: what the model server measured for each planner reply of the run (prompt tokens, tokens taken from its cache, tokens written, the time), added up
//!   by run and by model, from the numbers the run record already holds (card OA9-05, task PR-10); no text. Who may read it is the same as for the events.
//! * `GET /api/v1/runs/{id}/stream?after=`: the same rows as a server-sent stream, then `end`. `id:` is the number of the event inside the run; a client that was cut off asks
//!   again with `?after=<the last id it saw>` (or lets the browser's `EventSource` send `Last-Event-ID`) and gets exactly the events it missed, once, in order. A frame `heartbeat`
//!   comes every ten seconds when nothing else was sent. A run that has ended is replayed whole and the stream ends.
//! * `GET /api/v1/events?after=`: the feed of the app, the events of every run in the order they were written, with the global number as `id:`; it starts with `server.connected` and
//!   goes on until the client hangs up. Without `after` (and without `Last-Event-ID`) it starts at the end of the log. UI token only.
//!
//! **Who may read what.** The UI token reads every run. The API token reads only the runs that were asked for with the API token (`agent_runs::with_origin`; the log of a run the
//! person's voice or app started is theirs); another run is answered `404` as if it did not exist. The fields that show what a call was given or returned, the exact action of a
//! question to the person and what the planner asked are sent to the UI token only (`events::ui_fields`): the API token is told that the run waits for an answer, never what the
//! question is.
//!
//! A stream is one task that ends the moment its client hangs up; it reads the log by number and is woken when a row is added, so a slow or dead client never slows a run.
//!
//! **Steering and ending a run** (card OA10-03; tasks UX-03 and UX-11): `POST /api/v1/runs/{id}/interrupt` ends a run, `POST /api/v1/runs/{id}/messages` says something to it (`delivery`
//! `steer`: added to the task at its next step; `queue`: kept for after it and then started as a run of its own, for runs that were started detached), `GET /api/v1/runs/{id}/queue` and
//! `DELETE /api/v1/runs/{id}/queue/{n}` show and take back what was said, `GET /api/v1/runs/{id}/wait?timeout=` answers when the run ends (or when the time is up) and
//! `GET /api/v1/runs/active` lists the runs that are not over. A run started detached (`POST /api/v1/runs`, `run_control_api`) shows as `queued` here until it has its turn. The same rule
//! as for the log: the UI token reaches every run, the API token the runs it started, and another run is `404`.

use super::auth::{ui_scope_required, Scope};
use super::run_host;
use crate::agent_runs::events::{self, Event, Stored};
use crate::agent_runs::{self, live, AgentRunStore};
use axum::{
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_stream::StreamExt;

/// Rows read at a time, by a stream and (at most) by a page.
const PAGE: u32 = 200;
/// The longest page a client may ask for.
const PAGE_MAX: u32 = 500;
/// How long a stream may be quiet before it sends a `heartbeat`.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// A stream looks at the log at least this often, whether or not it was woken.
const LOOK_AGAIN: Duration = Duration::from_secs(1);
/// How many event streams may be open at once; the next is refused (429) until one closes. A stream is a task and a read of the log every second: a script that opens them without end
/// should be told, not served.
const MAX_OPEN_STREAMS: usize = 64;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/runs/:run_id/events", get(run_events_page))
        .route("/api/v1/runs/:run_id/usage", get(run_usage))
        .route("/api/v1/runs/:run_id/stream", get(run_event_stream))
        .route("/api/v1/events", get(global_event_stream))
        .route("/api/v1/runs/active", get(runs_active))
        .route("/api/v1/runs/:run_id/wait", get(run_wait))
        .route("/api/v1/runs/:run_id/interrupt", post(run_interrupt))
        .route("/api/v1/runs/:run_id/messages", post(run_message).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/runs/:run_id/queue", get(run_queue))
        .route("/api/v1/runs/:run_id/queue/:n", axum::routing::delete(run_queue_remove))
}

static OPEN_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// How many event streams are open now: a stream's task ends when its client hangs up, and a test reads this to see that it did.
pub fn open_streams() -> usize {
    OPEN_STREAMS.load(Ordering::SeqCst)
}

struct OpenStream;

impl OpenStream {
    /// A place among the open streams, or `None` when [`MAX_OPEN_STREAMS`] are open. Given back when the guard is dropped, which the stream's task does when its client has gone.
    fn try_new() -> Option<Self> {
        if OPEN_STREAMS.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN_STREAMS {
            OPEN_STREAMS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(OpenStream)
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        OPEN_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn not_recording() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "agent runs are not being recorded (the run database could not be opened)" }))).into_response()
}

fn too_many_streams() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": format!("{MAX_OPEN_STREAMS} event streams are open already; close one and ask again") }))).into_response()
}

fn unknown_run(id: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no run called {id}") }))).into_response()
}

/// A run as a reader finds it: one with a record (and a log), or a detached run that waits for its turn and has none yet.
enum Known {
    Recorded(agent_runs::store::LogHead),
    Queued(run_host::View),
}

/// What `scope` may read of `run_id`: for a run with a record the head of its log (the UI token any run, the API token the runs it asked for), and for a detached run that has not begun
/// (it has no record until it does) the registry's view of it. Every other case is the same answer, `404`, so the API token cannot tell a run of the person's from one that does not exist.
fn known(store: &AgentRunStore, scope: Scope, run_id: &str) -> Result<Known, Response> {
    match store.log_head(run_id) {
        Ok(Some(head)) if scope == Scope::Ui || head.origin == "api" => Ok(Known::Recorded(head)),
        Ok(Some(_)) => Err(unknown_run(run_id)),
        // A detached run has no record until its loop begins: while it waits for its turn, and for the moment between its turn and its first line, it is the registry's to describe.
        Ok(None) => match run_host::lookup(run_id).filter(|v| v.state != run_host::State::Ended) {
            Some(view) if scope == Scope::Ui || view.origin == "api" => Ok(Known::Queued(view)),
            _ => Err(unknown_run(run_id)),
        },
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()),
    }
}

/// The head of a run's log when `scope` may read it and the run has one: a detached run that has not begun has no record, so nothing that is read from the record (the usage, say) is there
/// yet and it is a `404` like a run nobody knows.
pub(super) fn readable(store: &AgentRunStore, scope: Scope, run_id: &str) -> Result<agent_runs::store::LogHead, Response> {
    match known(store, scope, run_id)? {
        Known::Recorded(head) => Ok(head),
        Known::Queued(_) => Err(unknown_run(run_id)),
    }
}

#[derive(Deserialize)]
struct PageQuery {
    /// The number of the last event the client has (0: from the start).
    after: Option<i64>,
    /// At most this many events (200 by default, 500 at most).
    limit: Option<u32>,
}

/// One page of the events of a run, oldest first: `{ run_id, events, last, finished }`. `last` is the number to ask `after` with next; `finished` is true when the run has ended and
/// nothing after `last` is left to read. The UI token reads every run; the API token reads the runs it asked for (another is `404`).
async fn run_events_page(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Query(q): Query<PageQuery>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    // The end is read before the rows: when the log says it is complete, every row it will ever have is written by the time they are read.
    let head = match known(store, scope, &run_id) {
        Ok(Known::Recorded(head)) => head,
        // A detached run that waits for its turn has no log yet: it is `queued`, with its place in the line.
        Ok(Known::Queued(view)) => {
            let state = if view.state == run_host::State::Queued { "queued" } else { "running" };
            return Json(json!({ "run_id": run_id, "events": [], "last": q.after.unwrap_or(0).max(0), "finished": false, "state": state, "position": view.position })).into_response();
        }
        Err(response) => return response,
    };
    let ui = scope == Scope::Ui;
    let after = q.after.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(PAGE).clamp(1, PAGE_MAX);
    match store.events_after(&run_id, after, limit) {
        Ok(rows) => {
            let more = rows.len() as u32 >= limit;
            let last = rows.last().map_or(after, |r| r.seq);
            Json(json!({ "run_id": run_id, "events": rows.iter().map(|r| r.wire(ui)).collect::<Vec<_>>(), "last": last, "finished": head.finished && !more })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

/// What each planner reply of a run cost, as the model server measured it, and the total by run and by model (see `model::ledger`): `{ run_id, calls, totals, by_model, unmeasured_replies }`.
/// The numbers are the ones the run record holds for the steps of the run, so this answers for a run that is still going (up to its last reply) and for one that ended; there is no text in
/// it. The UI token reads every run; the API token reads the runs it asked for (another is `404`).
async fn run_usage(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = readable(store, scope, &run_id) {
        return response;
    }
    match store.get(&run_id) {
        Ok(Some(run)) => Json(crate::modules::llm_router::ledger::RunUsage::from_run(&run)).into_response(),
        Ok(None) => unknown_run(&run_id),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct StreamQuery {
    /// The number of the last event the client has.
    after: Option<i64>,
}

/// The number a stream starts after: the larger of `?after=` and the `Last-Event-ID` the browser sends when it reconnects (an `EventSource` keeps its first URL, so its `?after=`
/// is stale by then). `None` when neither was given.
fn start_after(query: Option<i64>, headers: &HeaderMap) -> Option<i64> {
    let from_header = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    match (query, from_header) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).max(b.unwrap_or(0)).max(0)),
    }
}

/// The events of a run as a server-sent stream, then `end`: each carries `id:` (its number in the run); take the stream up again with `?after=<id>` or `Last-Event-ID`. A run that has
/// ended is replayed from `after` and the stream ends; one that is going is followed until it ends. `heartbeat` frames keep a quiet stream alive. Same readers as the page.
async fn run_event_stream(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Query(q): Query<StreamQuery>, headers: HeaderMap) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    let (ui, after) = (scope == Scope::Ui, start_after(q.after, &headers).unwrap_or(0));
    let Some(open) = OpenStream::try_new() else { return too_many_streams() };
    let (tx, rx) = mpsc::channel::<SseEvent>(32);
    let store = store.clone();
    tokio::spawn(async move {
        let _open = open;
        follow_run(&store, &run_id, ui, after, tx).await;
    });
    sse_response(rx)
}

/// The events of every run, in the order they were written, as a server-sent stream (UI token only): starts with `server.connected`, then each event with its global number as `id:`.
/// Without `after` or `Last-Event-ID` it starts at the end of the log; with one it replays what came after. It goes on until the client hangs up.
async fn global_event_stream(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<StreamQuery>, headers: HeaderMap) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store() else { return not_recording() };
    let after = start_after(q.after, &headers);
    let Some(open) = OpenStream::try_new() else { return too_many_streams() };
    let (tx, rx) = mpsc::channel::<SseEvent>(32);
    let store = store.clone();
    tokio::spawn(async move {
        let _open = open;
        follow_all(&store, after, tx).await;
    });
    sse_response(rx)
}

fn sse_response(rx: mpsc::Receiver<SseEvent>) -> Response {
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<SseEvent, Infallible>);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

/// A row as a frame: the type is the event name, `id:` the number the client takes the stream up again from.
fn row_frame(row: &Stored, ui: bool, id: i64) -> SseEvent {
    SseEvent::default().event(row.kind.clone()).id(id.to_string()).data(row.wire(ui).to_string())
}

/// A frame of the stream that is not a row (`heartbeat`, `end`, `server.connected`): it has no `id:`, so it never moves the number a client reconnects from.
fn control_frame(event: &Event) -> SseEvent {
    SseEvent::default().event(event.kind()).data(events::frame(event).to_string())
}

/// Sends the log of one run from `after` to its end, and then waits for what comes. Ends when the client hangs up or the run has ended.
async fn follow_run(store: &AgentRunStore, run_id: &str, ui: bool, mut after: i64, tx: mpsc::Sender<SseEvent>) {
    let mut last_sent = Instant::now();
    // The live output of the run's calls (card OA7-04) is the UI token's: what a program printed is what the call touched. It is no row of the log: a frame with no `id:`.
    let mut pieces = ui.then(live::subscribe);
    // Where a detached run that waits for its turn stood when it was last told (card OA10-03): it is told again when it moves up.
    let mut told_position: Option<usize> = None;
    loop {
        // Armed before the log is read: a row added between the read and the wait still wakes the wait.
        let woken = events::changed().notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        // The end is read before the rows (see `LogHead::finished`). A run that is no longer recorded ends the stream, unless it is a detached run that has not begun: that one is told
        // where it stands in the line, and waited for.
        let head = store.log_head(run_id).ok().flatten();
        let queued = head.is_none().then(|| run_host::lookup(run_id).filter(|v| v.state != run_host::State::Ended)).flatten();
        if let Some(view) = queued.as_ref().filter(|v| v.state == run_host::State::Queued) {
            if told_position != Some(view.position) {
                told_position = Some(view.position);
                if tx.send(control_frame(&Event::Status { state: events::RunState::Queued, position: Some(view.position as u32), reason: None })).await.is_err() {
                    return;
                }
                last_sent = Instant::now();
            }
        }
        let finished = if queued.is_some() { false } else { head.is_none_or(|head| head.finished) };
        let Ok(rows) = store.events_after(run_id, after, PAGE) else { return };
        let full = rows.len() as u32 >= PAGE;
        for row in rows {
            after = row.seq;
            if tx.send(row_frame(&row, ui, row.seq)).await.is_err() {
                return;
            }
            last_sent = Instant::now();
        }
        if full {
            continue;
        }
        if finished {
            let _ = tx.send(control_frame(&Event::End { last: after })).await;
            return;
        }
        tokio::select! {
            _ = &mut woken => {}
            _ = tokio::time::sleep(LOOK_AGAIN) => {}
            _ = tx.closed() => return,
            piece = next_piece(&mut pieces) => {
                if let Some(piece) = piece.filter(|p| p.run_id == run_id) {
                    if tx.send(progress_frame(&piece)).await.is_err() {
                        return;
                    }
                    last_sent = Instant::now();
                }
            }
        }
        if last_sent.elapsed() >= HEARTBEAT {
            if tx.send(control_frame(&Event::Heartbeat)).await.is_err() {
                return;
            }
            last_sent = Instant::now();
        }
    }
}

/// The next piece of live output from the feed, when the reader has one (`None` for a reader that is not given them: it waits for ever, so that the other arms of the `select!` decide). A
/// reader that fell behind is told nothing: the pieces it missed are not kept, and the next one it reads is the oldest the feed still holds.
async fn next_piece(feed: &mut Option<broadcast::Receiver<Arc<live::Piece>>>) -> Option<Arc<live::Piece>> {
    let Some(rx) = feed.as_mut() else { return std::future::pending().await };
    loop {
        match rx.recv().await {
            Ok(piece) => return Some(piece),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            // The feed is a static and is never closed; if it were, this reader has no more pieces and the other arms go on.
            Err(broadcast::error::RecvError::Closed) => {
                *feed = None;
                return None;
            }
        }
    }
}

/// A piece of live output as a frame: the type is the event name and there is no `id:`, so that it never moves the number a client takes the stream up again from.
fn progress_frame(piece: &live::Piece) -> SseEvent {
    SseEvent::default().event("tool.progress").data(piece.frame().to_string())
}

/// Sends `server.connected`, then every event of every run after the global number `after` (the end of the log when `None`), and goes on until the client hangs up.
async fn follow_all(store: &AgentRunStore, after: Option<i64>, tx: mpsc::Sender<SseEvent>) {
    let newest = store.last_event_id().unwrap_or(0);
    let mut after = after.unwrap_or(newest);
    if tx.send(control_frame(&Event::Connected { last: newest })).await.is_err() {
        return;
    }
    let mut last_sent = Instant::now();
    // The live output of every run's calls (card OA7-04): the app's feed has the UI token, so it has the lines too, as frames with no `id:`.
    let mut pieces = Some(live::subscribe());
    loop {
        let woken = events::changed().notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let Ok(rows) = store.events_since(after, PAGE) else { return };
        let full = rows.len() as u32 >= PAGE;
        for row in rows {
            after = row.id;
            // The global feed is the app's: it is only ever opened with the UI token, so every field is sent.
            if tx.send(row_frame(&row, true, row.id)).await.is_err() {
                return;
            }
            last_sent = Instant::now();
        }
        if full {
            continue;
        }
        tokio::select! {
            _ = &mut woken => {}
            _ = tokio::time::sleep(LOOK_AGAIN) => {}
            _ = tx.closed() => return,
            piece = next_piece(&mut pieces) => {
                if let Some(piece) = piece {
                    if tx.send(progress_frame(&piece)).await.is_err() {
                        return;
                    }
                    last_sent = Instant::now();
                }
            }
        }
        if last_sent.elapsed() >= HEARTBEAT {
            if tx.send(control_frame(&Event::Heartbeat)).await.is_err() {
                return;
            }
            last_sent = Instant::now();
        }
    }
}

// ---- steering and ending a run (card OA10-03; UX-03, UX-11) ------------------------------------------------------------------

/// Is this a run that is going on in this process right now, or a detached one that has not ended (it waits for its turn, or is about to begin)?
fn is_going(run_id: &str) -> bool {
    crate::agent_runs::scope::is_active(run_id) || run_host::lookup(run_id).is_some_and(|v| v.state != run_host::State::Ended)
}

/// What a run is, in one word, from its record and its log: what the log last said while it goes on, the record's state once it is over.
fn state_word(store: &AgentRunStore, run_id: &str, head: &agent_runs::store::LogHead) -> String {
    if matches!(head.status.as_str(), "running" | "stalled") {
        return store.last_state(run_id).ok().flatten().unwrap_or_else(|| "running".to_string());
    }
    head.status.clone()
}

/// The first part of a text that goes into an answer: masked, one line, cut.
fn shown(text: &str, max: usize) -> String {
    let masked = aqua_tool_host::output::Redactor::new().redact(text);
    let one: String = masked.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        format!("{}…", one.chars().take(max).collect::<String>())
    } else {
        one
    }
}

/// Ends a run (the UI token any run, the API token the runs it started): a run that waits for its turn is taken out of the line and nothing runs; the run that is going is told to stop (the
/// model call hangs up, the agent loop ends at its next step, a question that is open is withdrawn) and this answers when it has ended, or after a few seconds if it has not. What was said
/// to the run to be kept for after it is let go. A run that is not the one the person waits for (a background job) is ended from `POST /api/v1/queue/{job}/cancel`, and a run that is
/// over is `409`.
async fn run_interrupt(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    end_run(&run_id).await
}

/// Ends the run `run_id`, which the caller may reach (see [`run_interrupt`]).
async fn end_run(run_id: &str) -> Response {
    let run_id = run_id.to_string();
    match run_host::interrupt(&run_id) {
        run_host::Interrupted::Dequeued => {
            return Json(json!({ "run_id": run_id, "interrupted": true, "ended": true, "state": "cancelled", "note": "it was waiting for its turn and was taken out of the line: nothing ran" })).into_response();
        }
        run_host::Interrupted::AlreadyOver => return (StatusCode::CONFLICT, Json(json!({ "error": "the run has ended already" }))).into_response(),
        run_host::Interrupted::Running | run_host::Interrupted::Unknown => {}
    }
    // The run that is going: the one the person waits for is stopped as Stop stops it. A run that is going and is not that one is not stopped by this (the turn that would be is another).
    if crate::agent_runs::scope::foreground_run_id().as_deref() != Some(run_id.as_str()) {
        // A detached run that has its turn and has not begun its loop yet is told at its first step (it looks at the mark before it starts): nothing else is stopped for it.
        if run_host::lookup(&run_id).is_some_and(|v| v.state == run_host::State::Running) {
            return Json(json!({ "run_id": run_id, "interrupted": true, "ended": false, "state": "stopping" })).into_response();
        }
        let why = if crate::agent_runs::scope::is_active(&run_id) { "this run is not the one the person waits for (a background job): end it from POST /api/v1/queue/{job}/cancel" } else { "the run has ended already" };
        return (StatusCode::CONFLICT, Json(json!({ "error": why }))).into_response();
    }
    let stopped_turn = super::turn_control::stop_current();
    crate::modules::browser_agent::cancel_running_agents();
    // What was kept for after it is let go with it (a person who stops a run does not want the next one it would have started).
    let _ = crate::steer::take_run_end(&run_id);
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    if let Some(turn) = stopped_turn {
        while super::turn_control::is_running_turn(turn) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let ended = stopped_turn.is_none_or(|turn| !super::turn_control::is_running_turn(turn));
    Json(json!({ "run_id": run_id, "interrupted": true, "ended": ended, "state": if ended { "cancelled" } else { "stopping" } })).into_response()
}

#[derive(Deserialize)]
struct MessageRequest {
    /// What to say to the run: one line, at most 500 characters.
    text: String,
    /// `steer` (the default): added to the task at its next step. `queue`: kept for after it, and then started as a run of its own (only for a run started detached).
    #[serde(default)]
    delivery: Option<String>,
}

/// Says something to a run that is going (the UI token any run, the API token the runs it started). With `delivery` `steer` the words are added to the task at its next step, as the
/// person's own; with `queue` they are kept for after the run and then started as a run of its own, when the run ended well. Five lines at most wait for one run in each way. "stop"
/// is a stop. Nothing said this way answers a question of the host: a yes here is words, and the question is answered at `POST /api/v1/browser/confirm`.
async fn run_message(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Json(body): Json<MessageRequest>) -> Response {
    use crate::steer::{Accepted, Delivery, Refused};
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    let delivery = match body.delivery.as_deref().map(Delivery::parse) {
        None => Delivery::Steer,
        Some(Some(d)) => d,
        Some(None) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "delivery is steer or queue" }))).into_response(),
    };
    if !is_going(&run_id) {
        return (StatusCode::CONFLICT, Json(json!({ "error": "the run is not going on: send it as a new request" }))).into_response();
    }
    // A stop said to a run is a stop of **that** run: it is never a stop of whichever run happens to be going (a script that holds a run in the line must not be able to stop the one in front of it).
    if crate::voice_commands::is_stop_command(&body.text, "") {
        return end_run(&run_id).await;
    }
    if delivery == Delivery::Queue && !run_host::is_detached(&run_id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "this run was not started detached (POST /api/v1/runs), so nothing starts a run after it: say it as a new request when it has ended, or steer it now" })),
        )
            .into_response();
    }
    match crate::steer::accept_for(&run_id, &body.text, delivery, None) {
        Ok(Accepted::Queued(n)) => (StatusCode::ACCEPTED, Json(json!({ "run_id": run_id, "delivery": "steer", "queued": n, "note": "it is read at the next step and added to the task as your own words" }))).into_response(),
        Ok(Accepted::Later(n)) => (StatusCode::ACCEPTED, Json(json!({ "run_id": run_id, "delivery": "queue", "kept": n, "note": "it starts as a run of its own when this one has ended well" }))).into_response(),
        Ok(Accepted::Stopped) => Json(json!({ "run_id": run_id, "stopped": true })).into_response(),
        Err(e @ (Refused::NothingRunning | Refused::Full)) => (StatusCode::CONFLICT, Json(json!({ "error": e.message() }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.message() }))).into_response(),
    }
}

/// What was said to a run and is still waiting (the UI token any run, the API token the runs it started): the lines for its next step (`steer`) and the lines kept for after it (`queue`),
/// each with the number `DELETE /api/v1/runs/{id}/queue/{n}` takes back.
async fn run_queue(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    let (steer, later) = crate::steer::waiting_for(&run_id);
    let list = |lines: Vec<String>| lines.into_iter().enumerate().map(|(n, text)| json!({ "n": n, "text": shown(&text, crate::steer::MAX_CHARS) })).collect::<Vec<_>>();
    Json(json!({ "run_id": run_id, "steer": list(steer), "queue": list(later), "max": crate::steer::MAX_QUEUED })).into_response()
}

/// Takes back the `n`th line kept for after a run (the number `GET /api/v1/runs/{id}/queue` gave it). `404` when there is none. The lines for the next step are read by the run at its next
/// step and cannot be taken back. The UI token: it is the app's queue to edit (a script that wants none of it ends the run).
async fn run_queue_remove(axum::Extension(scope): axum::Extension<Scope>, Path((run_id, n)): Path<(String, usize)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    if crate::steer::remove_queued(&run_id, n) {
        Json(json!({ "run_id": run_id, "removed": n })).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no line number {n} is kept for after this run") }))).into_response()
    }
}

#[derive(Deserialize)]
struct WaitQuery {
    /// How long to wait for the run to end, in seconds (0 to 120; 60 by default).
    timeout: Option<u64>,
}

/// The longest one wait lasts.
const WAIT_MAX_SECS: u64 = 120;

/// What a run is now, as a client that asks needs it: its state (`queued`, `running`, `waiting_approval`, `waiting_user`, or how it ended), whether it has ended, and what it waits for
/// (`approval` of the host's question, which the UI answers at `POST /api/v1/browser/confirm`, or an `answer` to the planner's question, at `POST /api/v1/runs/{id}/answer`). The UI token
/// also gets the planner's question. The answer of a run that ended is its answer, masked and cut.
fn snapshot(store: &AgentRunStore, run_id: &str, scope: Scope) -> Option<serde_json::Value> {
    let ui = scope == Scope::Ui;
    let detached = run_host::lookup(run_id);
    let Some(head) = store.log_head(run_id).ok().flatten() else {
        // Not recorded yet: a detached run that waits for its turn.
        let v = detached.filter(|v| v.state != run_host::State::Ended)?;
        let state = if v.state == run_host::State::Queued { "queued" } else { "running" };
        return Some(json!({ "run_id": run_id, "state": state, "finished": false, "position": v.position, "goal": v.goal_head, "needs": null }));
    };
    let run = store.get(run_id).ok().flatten()?;
    let state = state_word(store, run_id, &head);
    let needs = match state.as_str() {
        "waiting_approval" => Some("approval"),
        "waiting_user" => Some("answer"),
        _ => None,
    };
    // A detached run is over when the host of detached runs has written down how it ended, not a moment before: a client that is told "finished" gets its outcome with it.
    let over = head.finished && detached.as_ref().is_none_or(|v| v.state == run_host::State::Ended);
    let mut out = json!({
        "run_id": run_id,
        "state": state,
        "finished": over,
        "status": run["status"],
        "stopped_reason": run["stopped_reason"],
        "answer": shown(run["final_answer"].as_str().unwrap_or(""), 600),
        "started_at": run["started_at"],
        "ended_at": run["ended_at"],
        "stopped_by": run["stopped_by"],
        "needs": needs,
    });
    if let Some(v) = detached {
        out["outcome"] = json!(v.ended.as_ref().map(|e| e.outcome));
        out["detached"] = json!(true);
    }
    if ui && head.status == "waiting_user" {
        if let Some(q) = store.question_of(run_id).ok().flatten() {
            out["question"] = q.view();
        }
    }
    Some(out)
}

/// Waits for a run to end, for at most `timeout` seconds (60 by default, 120 at most), and says what it is then (the UI token any run, the API token the runs it started): the same answer
/// whether the wait ended because the run did or because the time was up, so a client asks again until `finished` is true. It also answers early when the run starts to wait for the
/// person (`needs`), so that a client that cannot see the screen can say so.
async fn run_wait(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, Query(q): Query<WaitQuery>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    if let Err(response) = known(store, scope, &run_id) {
        return response;
    }
    let deadline = Instant::now() + Duration::from_secs(q.timeout.unwrap_or(60).min(WAIT_MAX_SECS));
    let first = snapshot(store, &run_id, scope).and_then(|s| s["state"].as_str().map(str::to_string));
    loop {
        // Armed before the log is read: a row added between the read and the wait still wakes the wait.
        let woken = events::changed().notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let Some(now) = snapshot(store, &run_id, scope) else { return unknown_run(&run_id) };
        let changed_to_waiting = now["needs"].is_string() && now["state"].as_str().map(str::to_string) != first;
        if now["finished"] == true || changed_to_waiting || Instant::now() >= deadline {
            return Json(now).into_response();
        }
        tokio::select! {
            _ = &mut woken => {}
            _ = tokio::time::sleep(LOOK_AGAIN) => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

/// The runs that are not over (the UI token every run, the API token the runs it started): the ones going, the ones waiting for an answer to their question, and the detached ones that
/// wait for their turn, each with its state, what it waits for and its place in the line.
async fn runs_active(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    let Some(store) = agent_runs::store() else { return not_recording() };
    let ui = scope == Scope::Ui;
    let mut runs: Vec<serde_json::Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for v in run_host::live().into_iter().filter(|v| ui || v.origin == "api") {
        seen.push(v.id.clone());
        let recorded = store.log_head(&v.id).ok().flatten();
        let state = match (&v.state, &recorded) {
            (run_host::State::Queued, None) => "queued".to_string(),
            (_, Some(head)) => state_word(store, &v.id, head),
            (_, None) => "running".to_string(),
        };
        runs.push(json!({ "run_id": v.id, "state": state, "position": v.position, "goal": v.goal_head, "origin": v.origin, "detached": true, "started_at": v.started_ms.unwrap_or(v.queued_ms) }));
    }
    for r in store.active_runs(100).unwrap_or_default().into_iter().filter(|r| ui || r.origin == "api") {
        if seen.contains(&r.id) {
            continue;
        }
        let state = if r.status == "waiting_user" { "waiting_user".to_string() } else { store.last_state(&r.id).ok().flatten().unwrap_or_else(|| "running".to_string()) };
        runs.push(json!({ "run_id": r.id, "state": state, "goal": shown(&r.goal, 160), "origin": r.origin, "detached": false, "started_at": r.started_at, "route": r.route }));
    }
    Json(json!({ "runs": runs })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_starts_after_the_larger_of_the_query_and_the_last_event_id_the_browser_sends() {
        let with = |id: &str| {
            let mut h = HeaderMap::new();
            h.insert("last-event-id", id.parse().unwrap());
            h
        };
        assert_eq!(start_after(None, &HeaderMap::new()), None);
        assert_eq!(start_after(Some(5), &HeaderMap::new()), Some(5));
        assert_eq!(start_after(None, &with("12")), Some(12));
        assert_eq!(start_after(Some(0), &with("12")), Some(12), "an EventSource keeps its first URL: its ?after=0 is stale on the second connection");
        assert_eq!(start_after(Some(30), &with("12")), Some(30));
        assert_eq!(start_after(Some(-4), &HeaderMap::new()), Some(0));
        assert_eq!(start_after(None, &with("not a number")), None, "a header that is not a number says nothing");
    }

    #[test]
    fn only_so_many_streams_are_open_at_once_and_a_closed_one_gives_its_place_back() {
        let base = open_streams();
        let held: Vec<OpenStream> = (base..MAX_OPEN_STREAMS).map(|_| OpenStream::try_new().expect("a place")).collect();
        assert_eq!(open_streams(), MAX_OPEN_STREAMS);
        assert!(OpenStream::try_new().is_none(), "the next is refused");
        assert_eq!(open_streams(), MAX_OPEN_STREAMS, "and a refusal takes no place");
        drop(held);
        assert_eq!(open_streams(), base);
        assert!(OpenStream::try_new().is_some());
    }
}
