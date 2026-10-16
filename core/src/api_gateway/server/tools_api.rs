//! The tool host as routes (card OA3-06, moved out of `server.rs`): the list of tools, the call of one (`/tools/call`), the policy, full access, the ledger of calls and the undo store (`/tools/*`, `/permissions/full-access`).

use super::*;

// --- Tool host ---

async fn tools_list_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let tools: Vec<serde_json::Value> = state
        .tools
        .manifests()
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "signature": m.signature(),
                "description": m.description,
                "classes": m.classes,
                "default_decision": m.default_decision,
                "timeout_default_ms": m.timeout_default_ms,
                "timeout_max_ms": m.timeout_max_ms,
                "input_schema": m.input_schema,
            })
        })
        .collect();
    Json(serde_json::json!({ "count": tools.len(), "tools": tools }))
}

#[derive(Deserialize, Default)]
struct TrashPurgeRequest {
    /// Empty the whole undo store.
    #[serde(default)]
    all: bool,
    /// Otherwise remove what is older than this many days (default 30) and, oldest first, what is over `max_bytes` (default 2 GiB).
    #[serde(default)]
    max_age_days: Option<u64>,
    #[serde(default)]
    max_bytes: Option<u64>,
}

/// Clean the undo store (what `fs.delete` and overwrites move aside) now. It changes what can be undone, so it needs the UI token.
async fn tools_trash_purge_endpoint(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<TrashPurgeRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(trash) = crate::tool_host::TRASH.get().cloned() else {
        return api_error(axum::http::StatusCode::NOT_FOUND, "the file tools are off, so there is no undo store");
    };
    let done = tokio::task::spawn_blocking(move || {
        use aqua_native_tools::fs::trash::{DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_BYTES};
        let removed = if body.all { trash.purge_all() } else { trash.enforce_limits(body.max_age_days.unwrap_or(DEFAULT_MAX_AGE_DAYS), body.max_bytes.unwrap_or(DEFAULT_MAX_BYTES), None) };
        (removed, trash.usage())
    })
    .await;
    match done {
        Ok((removed, left)) => Json(serde_json::json!({ "removed_entries": removed.entries, "freed_bytes": removed.bytes, "remaining_entries": left.entries, "remaining_bytes": left.bytes })).into_response(),
        Err(_) => api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "the clean-up did not finish"),
    }
}

async fn tools_status_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "mode": state.tools.policy().mode(),
        "tool_count": state.tools.manifests().len(),
        // Tools that exist but cannot run on this machine right now, and why (missing program, broken config).
        "unavailable": state.tools.catalog().into_iter().filter_map(|(m, why)| why.map(|w| serde_json::json!({ "tool": m.name, "why": w }))).collect::<Vec<_>>(),
        "audit_path": state.tools.audit().path().display().to_string(),
    }))
}

/// What "full access" for the project is doing right now: the state of each layer, what it still asks, what it never allows.
async fn full_access_get_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let saved = crate::full_access::load(&crate::runtime::data_dir());
    let guard = crate::tool_host::PROJECT_GUARD.get().map(|g| g.as_ref());
    Json(crate::full_access::status(&state.tools, guard, &saved, Vec::new()))
}

#[derive(Debug, Deserialize)]
pub struct FullAccessRequest {
    pub enabled: bool,
}

/// Switch full access on or off. Only the UI token may: the agent and API clients cannot grant themselves access.
async fn full_access_put_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(body): Json<FullAccessRequest>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let guard = crate::tool_host::PROJECT_GUARD.get().map(|g| g.as_ref());
    match crate::full_access::set(&state.tools, guard, &crate::runtime::data_dir(), &crate::runtime::workspace_root(), body.enabled) {
        Ok(status) => Json(status).into_response(),
        Err(e @ crate::full_access::SetError::Locked) => (axum::http::StatusCode::CONFLICT, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn tools_policy_get_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.tools.policy().snapshot())
}

/// Replace the tool policy. Only the UI token may do this; the file is written first, so what the
/// host enforces is what is on disk after a restart.
async fn tools_policy_put_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(file): Json<aqua_tool_host::PolicyFile>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if let Err(reason) = file.check() {
        return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": reason }))).into_response();
    }
    let path = crate::tool_host::policy_path(&crate::runtime::data_dir());
    let written = serde_json::to_string_pretty(&file)
        .map_err(|e| e.to_string())
        .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
    if let Err(e) = written {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("could not save the policy: {e}") }))).into_response();
    }
    state.tools.policy().apply(&file);
    Json(state.tools.policy().snapshot()).into_response()
}

#[derive(Debug, Deserialize)]
pub struct AuditRecentQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only the lines of this agent run.
    #[serde(default)]
    pub run: Option<String>,
}

/// The last calls the tool host recorded, newest first: what ran, who asked, what was decided, and how it was allowed. Arguments are as recorded (already masked); the hash
/// fields are left out. The UI token (card R6-12): the lines say what the person's agent did and with which data, and the route answered anybody holding the API token; `run`
/// keeps the lines of one run, and the file is read from its end, not whole.
async fn tools_audit_recent_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    axum::extract::Query(q): axum::extract::Query<AuditRecentQuery>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = state.tools.audit().recent(limit, q.run.as_deref().filter(|r| !r.is_empty()));
    Json(serde_json::json!({ "records": rows })).into_response()
}

/// Checks the chain of the ledger and, with it, the signed heads against the lines (card R6-12): the chain alone cannot tell a ledger that was rewritten consistently. `ok` is false,
/// with a 409, when anything that can be checked is wrong; `line` and `error` are the first break of the chain and the first problem, as before.
async fn tools_audit_verify_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let check = state.tools.audit().check(&crate::audit_key::known_public_keys(&crate::runtime::data_dir()));
    let mut body = serde_json::json!({
        "ok": check.problems.is_empty(),
        "records": check.lines,
        "signed_heads": check.heads,
        "covered_lines": check.covered_lines,
        "unchecked_heads": check.unchecked_heads,
        "unsealed_lines": check.unsealed_lines,
        "problems": check.problems,
        // Records that log rotation deleted: not a fault of the chain, but the person should be able to see that some of the history is gone.
        "notices": check.notices,
    });
    if check.problems.is_empty() {
        if check.lines == 0 {
            body["note"] = serde_json::json!("no calls recorded yet");
        }
        return Json(body).into_response();
    }
    body["line"] = serde_json::json!(check.broken_at_line.unwrap_or(0));
    body["error"] = serde_json::json!(check.problems.first().cloned().unwrap_or_default());
    (axum::http::StatusCode::CONFLICT, Json(body)).into_response()
}

#[derive(Deserialize)]
pub struct ToolCallRequest {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Call any registered tool. The body is always the standard result envelope; the status code
/// follows its error code.
async fn tools_call_endpoint(State(state): State<AppState>, Json(payload): Json<ToolCallRequest>) -> impl IntoResponse {
    use aqua_tool_host::{Caller, ErrorCode, RequestContext, ToolError, ToolRequest, ToolResult};
    // A call made with the API token has no run and nothing shows that it is the person's own request. The host holds it to the ordinary rules of the mode (the full-access arm does not apply to
    // `Caller::Api`) and as tainted, so a change it makes asks every time and no answer to it is kept (card OA1-04, SE-01). The `session_id` below is the caller's own label, and an answer
    // of "for this session" is keyed by the caller as well, so a script cannot name the agent's session and use its answers (SE-17).
    let mut context = RequestContext::new(Caller::Api);
    context.dry_run = payload.dry_run;
    context.timeout_ms = payload.timeout_ms;
    if let Some(s) = payload.session_id {
        // An identifier, not text: it is kept with every approval of the session and in every line of the audit log.
        if s.chars().count() > 128 {
            let error = ToolError::new(ErrorCode::InvalidArguments, "session_id is longer than 128 characters");
            let shown = aqua_tool_host::audit::clip_field(&payload.tool, aqua_tool_host::audit::MAX_ID_CHARS);
            return (axum::http::StatusCode::BAD_REQUEST, Json(ToolResult::failure(&shown, "", error, 0)));
        }
        context.session_id = s;
    }
    let result = state
        .tools
        .call(ToolRequest { tool: payload.tool, arguments: payload.arguments, context })
        .await;
    let status = match result.error.as_ref().map(|e| e.code) {
        None => axum::http::StatusCode::OK,
        Some(ErrorCode::InvalidArguments) => axum::http::StatusCode::BAD_REQUEST,
        Some(ErrorCode::NotFound) => axum::http::StatusCode::NOT_FOUND,
        Some(ErrorCode::PermissionDenied | ErrorCode::ApprovalDenied | ErrorCode::ApprovalTimeout | ErrorCode::ApprovalRequired | ErrorCode::NetworkBlocked | ErrorCode::SandboxViolation | ErrorCode::SecretBlocked) => {
            axum::http::StatusCode::FORBIDDEN
        }
        Some(ErrorCode::Timeout) => axum::http::StatusCode::GATEWAY_TIMEOUT,
        Some(ErrorCode::Unavailable) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        Some(_) => axum::http::StatusCode::BAD_GATEWAY,
    };
    (status, Json(result))
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/tools", get(tools_list_endpoint))
        .route("/api/v1/tools/status", get(tools_status_endpoint))
        .route("/api/v1/tools/trash/purge", post(tools_trash_purge_endpoint))
        .route("/api/v1/tools/policy", get(tools_policy_get_endpoint).put(tools_policy_put_endpoint))
        .route("/api/v1/permissions/full-access", get(full_access_get_endpoint).put(full_access_put_endpoint))
        .route("/api/v1/tools/call", post(tools_call_endpoint))
        .route("/api/v1/tools/audit/verify", get(tools_audit_verify_endpoint))
        .route("/api/v1/tools/audit/recent", get(tools_audit_recent_endpoint))
}
