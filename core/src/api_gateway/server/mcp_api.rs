//! Browser MCP and Windows-MCP (card OA3-06, moved out of `server.rs`): their health, start, stop, tools and calls, and the agent that drives the browser alone (`/browser-mcp/*`, `/windows-mcp/*`).

use super::*;

// --- Browser MCP (https://github.com/BrowserMCP/mcp) ---

async fn browser_mcp_health_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let ext = state.browser.extension_client();
        let mut client = ext.lock().unwrap_or_else(PoisonError::into_inner);
        aqua_mcp_adapters::check_browser_mcp_health(
            &mut client,
            &EnsureBrowserMcpOptions {
                auto_setup: false,
                auto_start: true,
                poll_connect_secs: 0,
                open_extension_docs: false,
                on_progress: None,
            },
        )
    })
    .await;
    match result {
        Ok(health) => Json(serde_json::json!(health)).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string(), "ready": false })),
        )
            .into_response(),
    }
}

async fn browser_mcp_ensure_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let ext = state.browser.extension_client();
        let mut client = ext.lock().unwrap_or_else(PoisonError::into_inner);
        ensure_browser_mcp_ready(
            &mut client,
            EnsureBrowserMcpOptions {
                auto_setup: true,
                auto_start: true,
                poll_connect_secs: 12,
                open_extension_docs: true,
                on_progress: None,
            },
        )
    })
    .await;
    match result {
        Ok(Ok(health)) => {
            let status = if health.ready {
                axum::http::StatusCode::OK
            } else {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            };
            (status, Json(serde_json::json!(health))).into_response()
        }
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "ready": false,
                "error": e.to_string(),
                "user_action_required": "Run scripts/setup-browser-mcp.ps1, start aqua-core, Connect the Chrome extension."
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ready": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn browser_mcp_setup_endpoint(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Runs an npm and PowerShell setup script on this computer: UI token only.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let result = tokio::task::spawn_blocking(|| {
        let root = aqua_workspace_root();
        run_browser_mcp_setup_script(&root)
    })
    .await;
    match result {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn browser_mcp_status_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    match state.browser.extension_client().try_lock() {
        Ok(client) => Json(serde_json::json!(client.status())).into_response(),
        Err(_) => Json(serde_json::json!({
            "label": "browser",
            "process_running": true,
            "initialized": true,
            "command": "npx",
            "args": ["-y", "@browsermcp/mcp@0.1.3"],
            "tool_count": 0,
            "server_name": null,
            "server_version": null,
            "hint": "Browser MCP is busy (tool call in progress). Status will refresh shortly."
        }))
        .into_response(),
    }
}

async fn browser_mcp_start_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let result = tokio::task::spawn_blocking(move || {
        let ext = state.browser.extension_client();
        let mut client = ext.lock().unwrap_or_else(PoisonError::into_inner);
        client.start()
    })
    .await;
    match result {
        Ok(Ok(status)) => (axum::http::StatusCode::OK, Json(serde_json::json!(status))).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": e.to_string(),
                "hint": "Install Node.js, run npx @browsermcp/mcp@0.1.3, and Connect the Browser MCP Chrome extension."
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn browser_mcp_stop_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // The lock may be held by a long tool call; wait for it on a blocking thread.
    let status = tokio::task::spawn_blocking(move || {
        let ext = state.browser.extension_client();
        let mut client = ext.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = client.shutdown();
        client.status()
    })
    .await;
    match status {
        Ok(s) => Json(serde_json::json!(s)).into_response(),
        Err(e) => upstream_error("browser_mcp", anyhow::anyhow!("{e}")),
    }
}

async fn browser_mcp_tools_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let ext = state.browser.extension_client();
        let mut client = ext.lock().unwrap_or_else(PoisonError::into_inner);
        if !client.is_initialized() {
            client.start()?;
        }
        client.list_tools().map(aqua_mcp_adapters::filter_windows_tools)
    })
    .await;
    match result {
        Ok(Ok(tools)) => Json(serde_json::json!({ "tools": tools, "count": tools.len() })).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct BrowserMcpCallRequest {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Manual browser tool call. It goes through the same policy layer as the agent (tool allowlist,
/// site rules, confirmations, audit); it no longer talks to the extension client directly.
async fn browser_mcp_call_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserMcpCallRequest>,
) -> impl IntoResponse {
    // These drive the person's logged-in browser and screen with no run behind them (so nothing that was read can make them ask): the person's own app may call them, a
    // script with the API token may not (the agent routes, `/mcp/agent`, are the way for a script).
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let door = crate::door_shadow::DoorMode::from_env();
    let result = tokio::task::spawn_blocking(move || {
        // With AQUA_DOOR=on the call goes through the tool host as the app (card R6-08): the person's hooks, a loaded skill's list and the signed ledger apply, and the host asks.
        if let Some(text) = crate::agent_tools::door_route_call(door, &state.tools, &payload.tool, payload.arguments.clone(), aqua_tool_host::Caller::Ui) {
            return text;
        }
        state.browser.call(&payload.tool, payload.arguments).to_agent_text()
    })
    .await;
    match result {
        Ok(text) => tool_call_response(text),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `ERROR [CODE] tool: message` text becomes an HTTP status; anything else is a result.
fn tool_call_response(text: String) -> axum::response::Response {
    let status = if text.starts_with("ERROR [PERMISSION_DENIED]") || text.contains("[CONFIRMATION_DENIED]") || text.contains("[POLICY_") {
        Some(axum::http::StatusCode::FORBIDDEN)
    } else if text.starts_with("ERROR [INVALID_ARGUMENTS]") {
        Some(axum::http::StatusCode::BAD_REQUEST)
    } else if text.starts_with("ERROR [UNAVAILABLE]") {
        Some(axum::http::StatusCode::BAD_GATEWAY)
    } else {
        None
    };
    match status {
        Some(code) => (code, Json(serde_json::json!({ "error": text }))).into_response(),
        None => Json(serde_json::json!({ "result": text })).into_response(),
    }
}

#[derive(Deserialize)]
pub struct BrowserMcpAgentRequest {
    pub goal: String,
    #[serde(default = "default_agent_max_steps")]
    pub max_steps: u32,
}

pub(in crate::api_gateway) fn default_agent_max_steps() -> u32 {
    10
}

async fn browser_mcp_agent_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<BrowserMcpAgentRequest>,
) -> impl IntoResponse {
    let llm = state.llm.clone();
    let browser_mcp = state.browser.clone();
    let laya = state.laya.clone();
    let goal = payload.goal;
    let max_steps = payload.max_steps.clamp(1, crate::modules::browser_agent::max_rounds_for_goal(&goal));

    // An agent task takes its turn in the queue like every other task, and Stop (or hanging up) ends it at its next step.
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let result = run_turn(move || (move || -> anyhow::Result<McpAgentResult> {
        if let Some(answer) = crate::live_weather::handle_request(&goal) {
            return Ok(McpAgentResult {
                final_answer: answer,
                steps: vec![],
                tool_rounds: 0,
                stopped_reason: "weather".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
        }
        let decision = laya.decide_with_timeout(&goal, true, LayaGate::gate_timeout(), |_| {});
        let decision = crate::laya_validator::refine_route(decision, &goal, &llm, true);
        match decision.route {
            GateRoute::Refuse => Ok(McpAgentResult {
                final_answer: crate::laya_validator::GateDecision::refuse_reply().into(),
                steps: vec![],
                tool_rounds: 0,
                stopped_reason: "laya_gate".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            }
            .with_gate(decision)),
            GateRoute::Chat => {
                let answer = llm
                    .generate_chat_response(&grounded_chat_prompt(&goal))
                    .unwrap_or_else(|e| format!("LLM error: {e}"));
                Ok(McpAgentResult {
                    final_answer: crate::modules::llm_router::strip_reasoning_tags(&answer),
                    steps: vec![],
                    tool_rounds: 0,
                    stopped_reason: "laya_gate".into(),
                    gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                }
                .with_gate(decision))
            }
            GateRoute::McpAgent => {
                let hint = decision.surface.agent_prompt_line();
                let out = run_mcp_agent_shared(
                    &llm,
                    Some(browser_mcp),
                    None,
                    &goal,
                    max_steps,
                    hint,
                )?;
                if out.tool_rounds > 0 {
                    laya.feedback_route(&goal, GateRoute::McpAgent);
                }
                Ok(out.with_gate(decision))
            }
        }
    })().map_err(|e| format!("{e:#}")))
    .await;

    match result {
        Ok(agent_result) => Json(agent_result).into_response(),
        Err(e) if e.trim_start().to_lowercase().starts_with("stopped") => pipeline_failure(&e),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

// --- Windows-MCP (https://github.com/CursorTouch/Windows-MCP) ---

async fn windows_mcp_status_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    match state.windows_mcp.try_lock() {
        Ok(client) => Json(serde_json::json!(client.status())).into_response(),
        Err(_) => Json(serde_json::json!({
            "label": "windows",
            "process_running": true,
            "initialized": true,
            "command": "uvx",
            "args": ["windows-mcp", "serve"],
            "tool_count": 0,
            "server_name": null,
            "server_version": null,
            "hint": "Windows-MCP is busy (tool call in progress). Status will refresh shortly."
        }))
        .into_response(),
    }
}

async fn windows_mcp_start_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut client = state.windows_mcp.lock().unwrap_or_else(PoisonError::into_inner);
        client.start()
    })
    .await;
    match result {
        Ok(Ok(status)) => (axum::http::StatusCode::OK, Json(serde_json::json!(status))).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": e.to_string(),
                "hint": "Install UV and run: uvx windows-mcp serve  (see scripts/setup_windows_mcp.ps1)"
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn windows_mcp_stop_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    // Starting and stopping tools is a UI action: the API token cannot do it.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let status = tokio::task::spawn_blocking(move || {
        let mut client = state.windows_mcp.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = client.shutdown();
        client.status()
    })
    .await;
    match status {
        Ok(s) => Json(serde_json::json!(s)).into_response(),
        Err(e) => upstream_error("windows_mcp", anyhow::anyhow!("{e}")),
    }
}

async fn windows_mcp_tools_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let mut client = state.windows_mcp.lock().unwrap_or_else(PoisonError::into_inner);
        if !client.is_initialized() {
            client.start()?;
        }
        client.list_tools()
    })
    .await;
    match result {
        Ok(Ok(tools)) => Json(serde_json::json!({ "tools": tools, "count": tools.len() })).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Manual desktop tool call. Only the UI tools pass, and only through the guard (allowlist,
/// argument rules, approvals, audit); PowerShell, FileSystem, Registry, Process, Scrape and
/// Clipboard answer 403.
async fn windows_mcp_call_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserMcpCallRequest>,
) -> impl IntoResponse {
    // These drive the person's logged-in browser and screen with no run behind them (so nothing that was read can make them ask): the person's own app may call them, a
    // script with the API token may not (the agent routes, `/mcp/agent`, are the way for a script).
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let door = crate::door_shadow::DoorMode::from_env();
    let result = tokio::task::spawn_blocking(move || {
        // With AQUA_DOOR=on the call goes through the tool host as the app (card R6-08): the provider holds the guard's rules, and the person's hooks, a loaded skill's list and the
        // signed ledger apply.
        if let Some(text) = crate::agent_tools::door_route_call(door, &state.tools, &payload.tool, payload.arguments.clone(), aqua_tool_host::Caller::Ui) {
            return text;
        }
        aqua_mcp_adapters::WindowsGuard::global().call(
            &state.windows_mcp,
            Some(state.browser.broker()),
            &payload.tool,
            payload.arguments,
            aqua_mcp_adapters::WindowsCaller::Api,
        )
    })
    .await;
    match result {
        Ok(text) => tool_call_response(text),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/browser-mcp/status", get(browser_mcp_status_endpoint))
        .route("/api/v1/browser-mcp/health", get(browser_mcp_health_endpoint))
        .route("/api/v1/browser-mcp/ensure", post(browser_mcp_ensure_endpoint))
        .route("/api/v1/browser-mcp/setup", post(browser_mcp_setup_endpoint))
        .route("/api/v1/browser-mcp/start", post(browser_mcp_start_endpoint))
        .route("/api/v1/browser-mcp/stop", post(browser_mcp_stop_endpoint))
        .route("/api/v1/browser-mcp/tools", get(browser_mcp_tools_endpoint))
        .route("/api/v1/browser-mcp/call", post(browser_mcp_call_endpoint))
        .route("/api/v1/browser-mcp/agent", post(browser_mcp_agent_endpoint))
        .route("/api/v1/windows-mcp/status", get(windows_mcp_status_endpoint))
        .route("/api/v1/windows-mcp/start", post(windows_mcp_start_endpoint))
        .route("/api/v1/windows-mcp/stop", post(windows_mcp_stop_endpoint))
        .route("/api/v1/windows-mcp/tools", get(windows_mcp_tools_endpoint))
        .route("/api/v1/windows-mcp/call", post(windows_mcp_call_endpoint))
}
