//! The routing feedback buttons and what is made of them (card R7-13): `POST /api/v1/laya/feedback` keeps each answer, `GET /api/v1/laya/feedback/stats` says how often the gate's
//! route was wrong, and `POST /api/v1/laya/calibrate` asks whether a threshold of the router could change without hurting the held-out requests.
//!
//! * **Feedback** needs the UI token (it teaches the gate where to send requests like this one). Besides telling the model gate, as it always did, it now keeps the answer in the
//!   routing ledger: the request with secrets removed (cut at 300 characters), the route the gate chose, the route the person says was right, the time and who decided.
//! * **Stats** may be read with the API token. The rate is of the answers, not of all requests, and it says so.
//! * **Calibrate** needs the UI token and changes nothing: it returns a proposal (what to change, from what to what, and the numbers on the held-out requests and on the person's
//!   answers) or the reason there is none. Applying a proposal is not here.
//!
//! The routes take [`FeedbackState`] (the ledger and the model gate), not the whole gateway state, so that a test can build the router over a folder of its own
//! (`core/tests/laya_feedback_test.rs`).

use super::auth::{ui_scope_required, Scope};
use super::server::{api_error, AppState};
use crate::laya_validator::calibrate::{self, builtin_held_out};
use crate::laya_validator::feedback_stats::{first_day_of, latest_answers, route_of_button, FeedbackRow, DEFAULT_DAYS};
use crate::laya_validator::{self, LayaGate, RoutingLedger};
use axum::{
    extract::{FromRef, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// How far back the calibration looks when the caller says nothing.
const CALIBRATE_DAYS: u32 = 90;

/// What the routes need: the ledger of decisions and answers, and the model gate that is taught by an answer. The gateway's [`AppState`] gives them (`FromRef`).
#[derive(Clone)]
pub struct FeedbackState {
    pub ledger: Arc<RoutingLedger>,
    pub laya: Arc<LayaGate>,
}

impl FromRef<AppState> for FeedbackState {
    fn from_ref(app: &AppState) -> Self {
        Self { ledger: laya_validator::ledger().clone(), laya: app.laya.clone() }
    }
}

/// The three routes.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    FeedbackState: FromRef<S>,
{
    Router::new()
        .route("/api/v1/laya/feedback", post(post_feedback))
        .route("/api/v1/laya/feedback/stats", get(get_feedback_stats))
        .route("/api/v1/laya/calibrate", post(post_calibrate))
}

#[derive(Deserialize)]
pub struct FeedbackRequest {
    pub message: String,
    /// `refuse` | `chat` | `mcp_agent`, or `cancel` (the person stopped the tools: taken as chat).
    pub correct_route: String,
}

#[derive(Deserialize)]
struct DaysQuery {
    days: Option<u32>,
}

/// Keeps what the person says the right route for a request was, and teaches the model gate. Needs the UI token.
async fn post_feedback(State(state): State<FeedbackState>, axum::Extension(scope): axum::Extension<Scope>, Json(payload): Json<FeedbackRequest>) -> Response {
    // This teaches the safety gate where to send requests like this one; whoever can do that can teach it to send
    // a refusal to the tools. UI token only.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let button = payload.correct_route.trim().to_ascii_lowercase();
    let Some(route) = route_of_button(&button) else {
        return api_error(StatusCode::BAD_REQUEST, "correct_route must be refuse|chat|mcp_agent|cancel");
    };
    if payload.message.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "message must not be empty");
    }
    let message = payload.message;
    let done = tokio::task::spawn_blocking(move || {
        let recorded = state.ledger.record_feedback(&message, route, &button);
        state.laya.feedback_route(&message, route);
        (recorded, state.laya.is_ready())
    })
    .await;
    match done {
        Ok((recorded, laya_ready)) => Json(json!({
            "ok": true,
            "route": route.as_str(),
            "laya_ready": laya_ready,
            "saved": recorded.saved,
            "matched_decision": recorded.row.chosen.is_some(),
            "gate_route": recorded.row.chosen.map(|r| r.as_str()),
            "source": recorded.row.source,
        }))
        .into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the answer could not be recorded"),
    }
}

/// How often the gate's route was wrong, by the answers the person gave on the buttons: the rate with its Wilson interval, by route and by who decided, and the decisions nobody
/// answered. It is the share of the answered requests, not the true error rate, and the answer says so. The API token may read it.
async fn get_feedback_stats(State(state): State<FeedbackState>, Query(query): Query<DaysQuery>) -> Response {
    let days = query.days.unwrap_or(DEFAULT_DAYS);
    match tokio::task::spawn_blocking(move || state.ledger.wrong_route_rate(days)).await {
        Ok(report) => Json(report).into_response(),
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the rate could not be worked out"),
    }
}

/// Asks whether a threshold of the decision router could change without hurting the held-out requests, and returns the proposal or the reason there is none. Nothing is applied.
/// Needs the UI token.
async fn post_calibrate(State(state): State<FeedbackState>, axum::Extension(scope): axum::Extension<Scope>, Query(query): Query<DaysQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let days = query.days.unwrap_or(CALIBRATE_DAYS);
    let work = tokio::task::spawn_blocking(move || {
        let rows = state.ledger.rows();
        let first_day = first_day_of(days, crate::memory::experience::model::now_secs());
        let answers: Vec<FeedbackRow> = latest_answers(&rows, first_day).into_iter().cloned().collect();
        let held_out = builtin_held_out();
        let outcome = calibrate::evaluate(&answers, &held_out, &laya_validator::decision_thresholds());
        (answers.len(), held_out.len(), held_out.iter().filter(|h| h.evidence.model.is_some()).count(), outcome)
    })
    .await;
    match work {
        Ok((answers, held_out, held_out_answered, outcome)) => {
            let (proposal, reason) = match outcome {
                Ok(p) => (Some(p), None),
                Err(why) => (None, Some(why)),
            };
            Json(json!({
                "applied": false,
                "proposal": proposal,
                "reason": reason.map(|r| json!({ "code": r.code, "text": r.reason })),
                "answers_used": answers,
                "held_out_requests": held_out,
                "held_out_with_model_answer": held_out_answered,
                "days": days,
                "note": "Nothing was changed. A proposal says what to change and what it was measured to do; a person decides.",
            }))
            .into_response()
        }
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the thresholds could not be tried"),
    }
}
