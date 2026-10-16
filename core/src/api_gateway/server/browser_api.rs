//! The browser layer as routes (card OA3-06, moved out of `server.rs`): its status, start, stop, tools, calls, policy, backend, session and screenshots (`/browser/*`; the questions it asks are in `approvals_api.rs`).

use super::*;

// --- Browser (policy-guarded; managed Playwright browser or Browser MCP extension) ---

async fn browser_status_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.browser.status())
}

async fn browser_start_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let browser = state.browser.clone();
    match tokio::task::spawn_blocking(move || browser.start()).await {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(e)) => upstream_error("browser", e),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

async fn browser_stop_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let browser = state.browser.clone();
    match tokio::task::spawn_blocking(move || browser.stop()).await {
        Ok(status) => Json(status).into_response(),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

async fn browser_tools_endpoint(State(state): State<AppState>) -> Response {
    let browser = state.browser.clone();
    let result = tokio::task::spawn_blocking(move || {
        browser.start()?;
        browser.tools()
    })
    .await;
    match result {
        Ok(Ok(tools)) => Json(serde_json::json!({ "count": tools.len(), "tools": tools })).into_response(),
        Ok(Err(e)) => upstream_error("browser", e),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

#[derive(Deserialize)]
pub struct BrowserCallRequest {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Same policy, confirmations and audit as the agent: this endpoint is not a bypass. The UI token.
async fn browser_call_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserCallRequest>,
) -> Response {
    // These drive the person's logged-in browser and screen with no run behind them (so nothing that was read can make them ask): the person's own app may call them, a
    // script with the API token may not (the agent routes, `/mcp/agent`, are the way for a script).
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let browser = state.browser.clone();
    let tools = state.tools.clone();
    let door = crate::door_shadow::DoorMode::from_env();
    match tokio::task::spawn_blocking(move || {
        // With AQUA_DOOR=on the call goes through the tool host as the app (card R6-08): the person's hooks, a loaded skill's list and the signed ledger apply, and the host asks. The
        // answer keeps the shape this route has always had.
        match crate::agent_tools::door_route_call(door, &tools, &payload.tool, payload.arguments.clone(), aqua_tool_host::Caller::Ui) {
            Some(text) => aqua_mcp_adapters::browser::BrowserObservation::from_agent_text(&payload.tool, "host", text),
            None => browser.call(&payload.tool, payload.arguments),
        }
    })
    .await
    {
        Ok(obs) => Json(obs).into_response(),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

async fn browser_policy_get_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.browser.policy())
}

async fn browser_policy_put_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(policy): Json<BrowserPolicyConfig>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let browser = state.browser.clone();
    match tokio::task::spawn_blocking(move || browser.set_policy(policy).map(|_| browser.policy())).await {
        Ok(Ok(p)) => Json(p).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

#[derive(Deserialize)]
pub struct BrowserBackendRequest {
    pub backend: BackendKind,
}

async fn browser_backend_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserBackendRequest>,
) -> Response {
    // Choosing whether the agent drives Aqua's own browser or the person's real, logged-in one is a security
    // decision: only the UI may make it, not the API token that scripts and tools hold.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    state.browser.set_kind(payload.backend);
    Json(state.browser.status()).into_response()
}

#[derive(Deserialize)]
pub struct BrowserSessionRequest {
    /// `reset_profile` wipes all logins and cookies in the Aqua browser profile.
    pub action: String,
}

async fn browser_session_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserSessionRequest>,
) -> Response {
    match payload.action.as_str() {
        "reset_profile" => {
            // Erases every login and cookie: not something the API token may do.
            if let Some(denied) = ui_scope_required(scope) {
                return denied;
            }
            let browser = state.browser.clone();
            match tokio::task::spawn_blocking(move || browser.reset_profile()).await {
                Ok(Ok(())) => Json(serde_json::json!({ "ok": true, "status": state.browser.status() })).into_response(),
                Ok(Err(e)) => upstream_error("browser", e),
                Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
            }
        }
        "status" => Json(serde_json::json!({ "ok": true, "status": state.browser.status() })).into_response(),
        other => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("unknown action {other:?}; use reset_profile or status") })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct BrowserScreenshotQuery {
    /// Artifact path as returned in `BrowserObservation.artifacts[].path`.
    pub path: String,
}

/// Serve a screenshot from the artifact folder (and nowhere else) for the UI.
async fn browser_screenshot_endpoint(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<BrowserScreenshotQuery>,
) -> Response {
    let artifacts = state.browser.dirs().artifacts.clone();
    let requested = crate::runtime::workspace_root().join(&q.path);
    let file = match (requested.canonicalize(), artifacts.canonicalize()) {
        (Ok(f), Ok(root)) if f.starts_with(&root) => f,
        _ => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "screenshot not found" })),
            )
                .into_response()
        }
    };
    let mime = match file.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) {
        Some(e) if e == "png" => "image/png",
        Some(e) if e == "jpg" || e == "jpeg" => "image/jpeg",
        Some(e) if e == "webp" => "image/webp",
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "not an image" })),
            )
                .into_response()
        }
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, mime)], bytes).into_response(),
        Err(e) => upstream_error("browser", anyhow::anyhow!("{e}")),
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/browser/status", get(browser_status_endpoint))
        .route("/api/v1/browser/start", post(browser_start_endpoint))
        .route("/api/v1/browser/stop", post(browser_stop_endpoint))
        .route("/api/v1/browser/tools", get(browser_tools_endpoint))
        .route("/api/v1/browser/call", post(browser_call_endpoint))
        .route("/api/v1/browser/policy", get(browser_policy_get_endpoint).put(browser_policy_put_endpoint))
        .route("/api/v1/browser/backend", put(browser_backend_endpoint))
        .route("/api/v1/browser/session", post(browser_session_endpoint))
        .route("/api/v1/browser/screenshot", get(browser_screenshot_endpoint))
}
