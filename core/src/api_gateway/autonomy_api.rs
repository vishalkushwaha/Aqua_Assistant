//! `GET` and `PUT /api/v1/autonomy`: how much Aqua asks, and how much one run may do (card R7-18, `documentation/AUTONOMY.md`).
//!
//! * `GET` (either token) gives the level and what each level does in words, the caps with their defaults and bounds, the runs that are going with what each has used so
//!   far, the last runs a cap stopped, and the level the tool host's policy is applying.
//! * `PUT` (the UI token only) changes what its body names: `level` (`ask_all`, `ask_risky` or `ask_outside`) and the caps `max_seconds`, `max_tool_calls`,
//!   `max_files_changed`, `max_changed_bytes` (each a whole number from 1 to a bound the `GET` gives). A field set to `null` goes back to its default, a field left out stays as it
//!   is. An unknown level, a cap out of its bounds and a field that is not known are a 400 and change nothing. The setting is written to `data/tools/autonomy.json` first; the
//!   level is then given to the tool host's policy (which can only ask more because of it) and every "approve for this session" answer is forgotten when the level changes.
//!
//! The routes take [`AutonomyState`], the data folder, the tool host and the counters, and not the whole gateway state (which a test cannot build), so that a test builds the
//! router over a folder and a host of its own (`core/tests/autonomy_test.rs`).

use super::auth::{ui_scope_required, Scope};
use super::server::AppState;
use crate::autonomy::{self, Autonomy, Cap, Settings};
use aqua_tool_host::policy::AskLevel;
use aqua_tool_host::ToolHost;
use axum::{
    extract::{FromRef, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// What the routes use: where the setting is kept, the tool host whose policy takes the level, and the counters of the runs. The gateway's [`AppState`] gives it (`FromRef`);
/// a test builds it over a folder and a host of its own.
#[derive(Clone)]
pub struct AutonomyState {
    pub data_dir: PathBuf,
    pub host: Option<Arc<ToolHost>>,
    pub autonomy: Arc<Autonomy>,
}

impl FromRef<AppState> for AutonomyState {
    fn from_ref(app: &AppState) -> Self {
        Self { data_dir: crate::runtime::data_dir(), host: Some(app.tools.clone()), autonomy: autonomy::shared() }
    }
}

/// The settings a body asks for: what it names changes, `null` goes back to the default, what it does not name stays. A word that is not a level, a cap that is not a whole number
/// from 1 to its bound, and a field that is not known are refused, with the reason.
pub fn merge(current: Settings, body: &Value) -> Result<Settings, String> {
    let Some(object) = body.as_object() else { return Err("the body must be a JSON object".to_string()) };
    let mut settings = current;
    for (key, v) in object {
        if key == "level" {
            settings.level = if v.is_null() {
                AskLevel::default()
            } else {
                v.as_str().and_then(AskLevel::from_id).ok_or_else(|| format!("level must be ask_all, ask_risky or ask_outside, not {v}"))?
            };
        } else if let Some(cap) = Cap::from_id(key) {
            let value = if v.is_null() { autonomy::DEFAULT_CAPS.get(cap) } else { autonomy::cap_value(cap, v)? };
            settings.caps.set(cap, value);
        } else {
            return Err(format!("{key:?} is not a field of the setting (level, max_seconds, max_tool_calls, max_files_changed, max_changed_bytes)"));
        }
    }
    Ok(settings)
}

/// The two routes.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    AutonomyState: FromRef<S>,
{
    Router::new().route("/api/v1/autonomy", get(get_autonomy).put(put_autonomy))
}

/// Gives the autonomy level and what each level does, the caps of one run with their defaults and bounds, the runs that are going with what they have used, and the last stops.
async fn get_autonomy(State(state): State<AutonomyState>) -> Response {
    Json(autonomy::describe(&state.autonomy, state.host.as_deref(), &state.data_dir)).into_response()
}

/// Changes the autonomy level and the caps of one run (what the body names; `null` is the default). A level can only make Aqua ask more than it does by default. Needs the UI token.
async fn put_autonomy(State(state): State<AutonomyState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<Value>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let new = match merge(state.autonomy.settings(), &body) {
        Ok(settings) => settings,
        Err(why) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response(),
    };
    let inner = state.clone();
    let applied = tokio::task::spawn_blocking(move || autonomy::apply(inner.host.as_deref(), &inner.autonomy, &inner.data_dir, new)).await;
    match applied {
        Ok(Ok(_)) => Json(autonomy::describe(&state.autonomy, state.host.as_deref(), &state.data_dir)).into_response(),
        Ok(Err(why)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": why }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the setting could not be changed" }))).into_response(),
    }
}
