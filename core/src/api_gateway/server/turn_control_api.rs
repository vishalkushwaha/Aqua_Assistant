//! What is said to a turn that runs or that stopped (card OA3-06, moved out of `server.rs`): a sentence that goes on from the task (`/turn/steer`, and the cue), going on from a run that stopped (`/runs/{id}/resume`), and stopping and cancelling (`/tasks/stop`, `/tasks/status`, `/voice/cancel`, `/agent/cancel`).

use super::*;

/// What is said back when a sentence was taken as a continuation of the task that is running (card OA10-03, UX-11), or `None` when it is not one: the setting is off, no task the
/// person waits for is going, the sentence does not begin with a cue (`voice_commands::steer_cue`), or it is for after the task and the task is not one a later run can follow (nothing
/// would start it). Nothing here answers a question of the host: a yes is not a cue.
pub(super) fn steer_by_cue(state: &AppState, text: &str) -> Option<String> {
    if !crate::flags::is_on(&crate::flags::STEER_CUES) {
        return None;
    }
    let wake = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    steer_line(&wake, text, crate::agent_runs::scope::foreground_run_id().as_deref())
}

/// Is this a run a script started with the API token (its origin is `api`)? What the person types is not for it: a script can read what was said to its run
/// (`GET /api/v1/runs/{id}/queue`), and it is not what the person is waiting for. A run that nobody marked (the voice orb, a workflow) is not one, and a detached run that still waits
/// for its turn is described by the registry of detached runs.
fn is_a_scripts_run(run: &str) -> bool {
    crate::agent_runs::scope::origin_of_run(run) == "api" || super::super::run_host::lookup(run).is_some_and(|v| v.origin == "api")
}

/// [`steer_by_cue`] for the run the person waits for (`None`: there is none), with the wake phrase given. A run that a script started is not one the person's sentence goes to: the
/// sentence is a request of its own, as it is when no task is going.
pub(super) fn steer_line(wake: &str, text: &str, run: Option<&str>) -> Option<String> {
    use crate::steer::{Accepted, Delivery};
    if !crate::flags::is_on(&crate::flags::STEER_CUES) {
        return None;
    }
    let (delivery, line) = crate::voice_commands::steer_cue(text, wake)?;
    let run = run.filter(|run| !is_a_scripts_run(run))?;
    if delivery == Delivery::Queue && !super::super::run_host::is_detached(run) {
        return None;
    }
    match crate::steer::accept_for(run, &line, delivery, Some(wake)) {
        Ok(Accepted::Queued(_)) => Some("Okay, I added that to what I am doing.".to_string()),
        Ok(Accepted::Later(_)) => Some("Okay, I will do that when this is done.".to_string()),
        // A line the queue has no room for is a new request, as it was: it waits its turn.
        _ => None,
    }
}

#[derive(Deserialize)]
struct SteerRequest {
    text: String,
}

/// Says something to the task that is running: it is added to the goal at the next step (as the person's own words), and "stop" ends the run at once.
/// Nothing said here answers a question the host asked, and nothing here is an approval. With no task running, the words are a new request (409). The UI
/// token.
async fn turn_steer_endpoint(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<SteerRequest>) -> Response {
    use crate::steer::{Accepted, Refused};
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::steer::accept(&body.text, None) {
        Ok(Accepted::Queued(n)) => (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({ "queued": n, "note": "it is read at the next step" }))).into_response(),
        // (This route only steers; the line kept for after a run is `POST /api/v1/runs/{id}/messages`.)
        Ok(Accepted::Later(n)) => (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({ "later": n, "note": "it is kept for after the run" }))).into_response(),
        Ok(Accepted::Stopped) => Json(serde_json::json!({ "stopped": true })).into_response(),
        Err(e @ (Refused::NothingRunning | Refused::Full)) => api_error(axum::http::StatusCode::CONFLICT, &e.message()),
        Err(e) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.message()),
    }
}

#[derive(Deserialize, Default)]
struct ResumeRequest {
    /// What to do about the calls that were running when the run stopped: `skip`, `retry` or `stop`.
    #[serde(default)]
    unknown: Option<String>,
}

/// Goes on with a run that stopped (the UI token: it starts work and shows what the run did). If calls were running when it stopped and may have
/// changed something, the answer is 409 with the question, and the person's choice comes back in `unknown`.
async fn run_resume_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, axum::extract::Path(run_id): axum::extract::Path<String>, body: Option<Json<ResumeRequest>>) -> Response {
    use crate::agent_runs::resume::{self, Choice};
    if let Some(denied) = super::super::auth::ui_scope_required(scope) {
        return denied;
    }
    let choice = match body.and_then(|Json(b)| b.unknown) {
        None => None,
        Some(word) => match Choice::parse(&word) {
            Some(c) => Some(c),
            None => return api_error(axum::http::StatusCode::BAD_REQUEST, "unknown is skip, retry or stop"),
        },
    };
    let Some(store) = crate::agent_runs::store() else {
        return api_error(axum::http::StatusCode::SERVICE_UNAVAILABLE, "runs are not being recorded, so there is nothing to go on from");
    };
    let host = crate::agent_tools::host().cloned();
    // Two requests for the same run at once (a second click, a retry after a time-out) would both find nothing that goes on from it: one at a time. A "stop" takes
    // the claim too: it ends the run, which must not happen between another request's check and the start of its continuation.
    let Some(_claim) = resume::claim(&run_id) else {
        return api_error(axum::http::StatusCode::CONFLICT, "a request to go on from this run is being served: wait for it");
    };
    if choice == Some(Choice::Stop) {
        return match resume::decline(store, &run_id, crate::agent_runs::now_ms()) {
            Ok(()) => Json(serde_json::json!({ "run_id": run_id, "state": "cancelled", "note": "this run will not be offered again" })).into_response(),
            Err(e) => refused_response(&e),
        };
    }
    let plan = match resume::plan(store, &run_id, &crate::agent_runs::scope::is_active, &|t| crate::agent_tools::can_change_something(host.as_deref(), t), choice) {
        Ok(p) => p,
        Err(e) => return refused_response(&e),
    };
    // The record is not taken on trust: it is checked against what the host's ledger says the run did (the chain, the signed heads, which calls worked) before anything is
    // started from it. That reads the whole ledger, so it is not done on the thread of the request.
    if let Some(h) = host.clone() {
        let (rid, keys) = (run_id.clone(), crate::audit_key::known_public_keys(&crate::runtime::data_dir()));
        let checked = tokio::task::spawn_blocking(move || {
            let view = resume::ledger_view(h.audit(), &keys, &rid);
            let served: std::collections::HashSet<String> = h.manifests().into_iter().map(|m| m.name).collect();
            resume::check_record(store, &rid, &view, &|t| served.contains(t))
        })
        .await;
        match checked {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return refused_response(&e),
            Err(e) => return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("the record of the run could not be checked: {e}")),
        }
    }
    let turn = match take_turn().await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let started = std::time::Instant::now();
    let work = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        for_client(true, || {
            crate::pipeline::run_resumed_agent(&state.llm, Some(state.browser.clone()), Some(state.windows_mcp.clone()), &plan, Some(&state.laya)).map_err(|e| format!("{e:#}"))
        })
    })
    .await;
    let answer = match work {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => return pipeline_failure(&e),
        Err(e) => return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("the run stopped unexpectedly: {e}")),
    };
    Json(serde_json::json!({
        "run_id": run_id,
        "continued_by": store.child_of(&run_id).ok().flatten(),
        "answer": answer,
        "question": question_for(true),
        "latency_ms": started.elapsed().as_millis() as u64,
    }))
    .into_response()
}

/// Why a run cannot be gone on from, as an answer: the question when there is one.
fn refused_response(e: &crate::agent_runs::resume::Refused) -> Response {
    use crate::agent_runs::resume::Refused;
    let code = match e {
        Refused::NotFound => axum::http::StatusCode::NOT_FOUND,
        Refused::NoGoal => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        _ => axum::http::StatusCode::CONFLICT,
    };
    let mut body = serde_json::json!({ "error": e.message() });
    if let Refused::NeedsAnswer(steps) = e {
        body["question"] = serde_json::json!({
            "calls": steps.iter().map(|s| serde_json::json!({ "step": s.step, "tool": s.tool, "arguments": s.args_redacted })).collect::<Vec<_>>(),
            "choices": ["skip", "retry", "stop"],
            "meaning": {
                "skip": "go on without running it; the agent is told it may or may not have happened, and the same call asks every time it is repeated",
                "retry": "the agent may run it again, under the usual rules",
                "stop": "end this run and do not offer it again",
            },
        });
    }
    (code, Json(body)).into_response()
}

/// Stop the task that is running. The running turn is told to stop (the model call hangs up, the agent loop ends at its next
/// step, the pipeline stops between stages), pending speech is dropped, and this answers only once that turn's work has really
/// ended (or after a few seconds if it has not), so the caller can start the next task knowing nothing of it is left running.
async fn tasks_stop_endpoint(State(state): State<AppState>) -> Json<serde_json::Value> {
    let stopped_turn = super::super::turn_control::stop_current();
    crate::modules::browser_agent::cancel_running_agents();
    state.active_session_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Wait for that very turn to end. A task that was waiting and has already started in its place is not waited for.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    if let Some(id) = stopped_turn {
        while super::super::turn_control::is_running_turn(id) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    let ended = stopped_turn.is_none_or(|id| !super::super::turn_control::is_running_turn(id));
    info!("Stop requested (a task was running: {}); ended: {ended}", stopped_turn.is_some());
    Json(serde_json::json!({ "stopped": stopped_turn.is_some(), "idle": ended, "waiting": super::super::turn_control::waiting_count() }))
}

/// Is a task running, and how many are waiting behind it.
async fn tasks_status_endpoint() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "busy": super::super::turn_control::is_running(),
        "waiting": super::super::turn_control::waiting_count(),
        "running_ms": super::super::turn_control::running_for_ms(),
    }))
}

#[derive(Serialize)]
pub struct CancelResponse {
    pub status: String,
    pub session_id: u64,
}

async fn voice_cancel_endpoint(State(state): State<AppState>) -> Json<CancelResponse> {
    let new_session_id = state.active_session_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    info!("Voice pipeline cancelled; active_session_id bumped to {}", new_session_id);
    Json(CancelResponse {
        status: "CANCELLED".to_string(),
        session_id: new_session_id,
    })
}

/// Stop any agent run in progress before its next step.
async fn agent_cancel_endpoint() -> impl IntoResponse {
    crate::modules::browser_agent::cancel_running_agents();
    Json(serde_json::json!({ "cancelled": true }))
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/turn/steer", post(turn_steer_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/runs/:run_id/resume", post(run_resume_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/voice/cancel", post(voice_cancel_endpoint))
        .route("/api/v1/tasks/stop", post(tasks_stop_endpoint))
        .route("/api/v1/tasks/status", get(tasks_status_endpoint))
        .route("/api/v1/agent/cancel", post(agent_cancel_endpoint))
}
