//! What the person has said yes to and the questions that wait for an answer (card OA3-06, moved out of `server.rs`): `/consents`, the open questions of the browser layer and the tool host (`/browser/confirmations`) and the answer to one (`/browser/confirm`).

use super::*;

/// What the person has said yes to and what was refused lately (the UI token: it shows what Aqua may do without asking). The answers of
/// "approve for this session" that still apply, the sites allowed for good, and the last refused calls, newest first. Nothing here gives a
/// permission; it only shows them, and `DELETE /api/v1/consents/{id}` takes one back.
async fn consents_list_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let sites: Vec<serde_json::Value> = state.browser.allowed_sites().into_iter().map(|s| serde_json::json!({ "id": format!("site:{s}"), "site": s })).collect();
    Json(serde_json::json!({
        "tool_grants": state.tools.consents(),
        "sites": sites,
        "recent_denials": state.tools.recent_denials(),
        "note": "A session answer lapses after 15 minutes. A site stays allowed until it is taken back.",
    }))
    .into_response()
}

/// Takes one answer back: `site:<host>` for a site, or the id a session answer was listed with.
async fn consent_revoke_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let revoked = match id.strip_prefix("site:") {
        Some(site) => match state.browser.revoke_site(site) {
            Ok(found) => found,
            Err(e) => return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save the site list: {e:#}")),
        },
        None => state.tools.revoke_consent(&id),
    };
    if revoked {
        Json(serde_json::json!({ "revoked": id, "note": "the next call of that kind asks again" })).into_response()
    } else {
        api_error(axum::http::StatusCode::NOT_FOUND, "there is no such consent (it may have lapsed)")
    }
}

#[derive(Deserialize)]
pub struct ConfirmationsQuery {
    /// Only the questions about this agent run (a question of the browser's own, with no run, is not among them).
    #[serde(default)]
    pub run: Option<String>,
}

/// The questions waiting for an answer. The UI token (card R6-12): they show the exact action and its target, and answering them is the UI's alone; `run` keeps one run's.
async fn browser_confirmations_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    axum::extract::Query(q): axum::extract::Query<ConfirmationsQuery>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let mut pending = state.browser.broker().pending();
    if let Some(run) = q.run.as_deref().filter(|r| !r.is_empty()) {
        pending.retain(|p| p.run_id.as_deref() == Some(run));
    }
    Json(serde_json::json!({ "pending": pending })).into_response()
}

#[derive(Deserialize)]
pub struct BrowserConfirmRequest {
    pub request_id: String,
    /// The answer: `approve`, `approve_always` (an unknown site, or a tool question that may be remembered) or `deny`. `allow` and `allow_always` are the older names of
    /// the first two. Anything else is refused with the list of what is accepted.
    pub decision: String,
    /// With `deny` only (card OA10-03, AG-11): what to do instead, in the person's own words (one line, 300 characters at most). A run that is allowed to go on after a "no" shows the
    /// model these words as data from the person; a run that is not stops as it always did. It is never an approval, and with `approve` it is refused (400).
    #[serde(default)]
    pub reason: Option<String>,
    /// With `approve` only, and only for a question that offers it (its `run_grant` is set) while the core runs with `AQUA_EDIT_GRANT=on` (card OA6-05, ED-10, decision D-08): `"run"` says
    /// yes to this edit and to later edits of files in the same folder until the run ends. Anything else is a 400, and so is `run` with another answer or on a question that does not offer it.
    #[serde(default)]
    pub scope: Option<String>,
}

/// What an answer to a question comes to: the answer is one of a closed set of words (a spoken or mistyped one is refused, not guessed), `approve_always` is for a question that
/// may be remembered, and the answer says what became of the question: applied, too late (the question expired and the run was told no: nothing was done), answered already,
/// taken back, or never asked. `edit` (approve with other arguments) is not offered: the edited call would have to be judged again, and the question shows the call with its
/// secrets masked, so an edit made from it could run with the masks in it.
pub(crate) fn answer_confirmation(broker: &aqua_mcp_adapters::browser::ConfirmationBroker, request_id: &str, word: &str) -> (axum::http::StatusCode, serde_json::Value) {
    answer_confirmation_with_reason(broker, request_id, word, None)
}

/// [`answer_confirmation`] for an answer that may come with words (card OA10-03, AG-11): `reason` is accepted with `deny` only (any other answer with words is a 400, so that words typed
/// beside a yes cannot be taken for an instruction), is made one line of at most 300 characters with its secrets masked, and is kept for the run only when `AQUA_DENY_FEEDBACK` is on
/// (the answer says so when it is not). The decision itself is exactly what it was.
pub(crate) fn answer_confirmation_with_reason(broker: &aqua_mcp_adapters::browser::ConfirmationBroker, request_id: &str, word: &str, reason: Option<&str>) -> (axum::http::StatusCode, serde_json::Value) {
    use aqua_mcp_adapters::browser::{ConfirmKind, Resolved};
    use axum::http::StatusCode;
    let choices = serde_json::json!(["approve", "approve_always", "deny"]);
    let decision = match word.trim().to_ascii_lowercase().as_str() {
        "approve" | "allow" => Decision::Allow,
        "approve_always" | "allow_always" => Decision::AllowAlways,
        "deny" | "reject" => Decision::Deny,
        "edit" => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "ok": false, "error": "editing a call before approving it is not offered: deny it, and say what to change", "choices": choices }),
            )
        }
        other => {
            let shown: String = other.chars().take(40).collect();
            return (StatusCode::BAD_REQUEST, serde_json::json!({ "ok": false, "error": format!("\"{shown}\" is not an answer: it is approve, approve_always or deny"), "choices": choices }));
        }
    };
    if decision == Decision::AllowAlways {
        if let Some(open) = broker.open_question(request_id) {
            if open.kind == ConfirmKind::Action || open.always_ask {
                return (
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "ok": false, "error": "this question is asked every time, so it cannot be approved for good: approve it once or deny it", "choices": ["approve", "deny"] }),
                );
            }
        }
    }
    // Words typed with the answer: with a no only, one line, masked, short; and kept for the run only when the setting is on.
    let said = reason.map(|r| crate::conversation::one_line(r, aqua_tool_host::MAX_PERSON_SAID_CHARS)).filter(|r| !r.is_empty());
    if said.is_some() && decision != Decision::Deny {
        return (StatusCode::BAD_REQUEST, serde_json::json!({ "ok": false, "error": "a reason goes with deny only: words typed beside a yes are not an instruction", "choices": choices }));
    }
    let keep = said.as_deref().filter(|_| crate::flags::is_on(&crate::flags::DENY_FEEDBACK));
    let resolved = broker.resolve_with_reason(request_id, decision, keep);
    match resolved {
        Resolved::Applied if said.is_some() => (
            StatusCode::OK,
            serde_json::json!({ "ok": true, "answer": "deny", "reason_given_to_the_run": keep.is_some(), "note": if keep.is_some() { "the run is told what you said" } else { "what you said was not kept: AQUA_DENY_FEEDBACK is off, so the run stops as it always did" } }),
        ),
        Resolved::Applied => (StatusCode::OK, serde_json::json!({ "ok": true, "answer": if decision == Decision::Deny { "deny" } else if decision == Decision::AllowAlways { "approve_always" } else { "approve" } })),
        other => not_applied(other),
    }
}

/// What an answer says when it was not applied: too late, answered already, taken back, or never asked (the one place these four are worded).
fn not_applied(resolved: aqua_mcp_adapters::browser::Resolved) -> (axum::http::StatusCode, serde_json::Value) {
    use aqua_mcp_adapters::browser::Resolved;
    use axum::http::StatusCode;
    match resolved {
        Resolved::Applied => (StatusCode::OK, serde_json::json!({ "ok": true })),
        Resolved::Expired { at_unix_ms } => (
            StatusCode::GONE,
            serde_json::json!({ "ok": false, "expired": true, "expired_unix_ms": at_unix_ms, "error": "this question expired: nobody answered in time, so the run was told no and nothing was done. Ask again if you still want it" }),
        ),
        Resolved::AlreadyAnswered(_) => (StatusCode::CONFLICT, serde_json::json!({ "ok": false, "error": "this question was answered already" })),
        Resolved::Withdrawn => (StatusCode::GONE, serde_json::json!({ "ok": false, "error": "the run that asked was stopped, so there is nothing to answer" })),
        Resolved::Unknown => (StatusCode::NOT_FOUND, serde_json::json!({ "ok": false, "error": "there is no such question (it may be from before a restart)" })),
    }
}

/// [`answer_confirmation_with_reason`] for an answer that may carry the run option (card OA6-05, ED-10, decision D-08): `scope: "run"`, with `approve`, on a question that offers it. The route
/// that calls this needs the UI token, so the API token and a voice request cannot make an edit grant; and the question offers the choice only while the core runs with `AQUA_EDIT_GRANT=on`, for a
/// plain file edit of a run with someone to ask that has read nothing from outside the project (`ToolHost`). Without a scope the answer is exactly what it was.
pub(crate) fn answer_confirmation_scoped(
    broker: &aqua_mcp_adapters::browser::ConfirmationBroker,
    request_id: &str,
    word: &str,
    reason: Option<&str>,
    scope: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    use aqua_mcp_adapters::browser::Resolved;
    use axum::http::StatusCode;
    let Some(scope) = scope.map(str::trim).filter(|s| !s.is_empty()) else {
        return answer_confirmation_with_reason(broker, request_id, word, reason);
    };
    if !scope.eq_ignore_ascii_case("run") {
        return (StatusCode::BAD_REQUEST, serde_json::json!({ "ok": false, "error": "scope is \"run\", or leave it out", "choices": ["run"] }));
    }
    if !matches!(word.trim().to_ascii_lowercase().as_str(), "approve" | "allow") {
        return (StatusCode::BAD_REQUEST, serde_json::json!({ "ok": false, "error": "the run option goes with approve only", "choices": ["approve"] }));
    }
    if reason.is_some_and(|r| !r.trim().is_empty()) {
        return (StatusCode::BAD_REQUEST, serde_json::json!({ "ok": false, "error": "a reason goes with deny only: words typed beside a yes are not an instruction" }));
    }
    if !aqua_tool_host::run_grant::enabled() {
        return (StatusCode::CONFLICT, serde_json::json!({ "ok": false, "error": "the edit grant is off: start the core with AQUA_EDIT_GRANT=on, or answer without a scope" }));
    }
    match broker.open_question(request_id) {
        // Not open (answered, expired, taken back, never asked): nothing is applied, and the answer says which. (A question that is not open cannot be opened again, so the `Deny` that asks is
        // never delivered to anybody; it is the one answer that could not loosen anything if it were.)
        None => not_applied(broker.resolve_typed(request_id, Decision::Deny)),
        Some(open) if open.run_grant.is_none() => (
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "ok": false, "error": "this question does not offer the run option: answer it with approve and no scope, or deny it", "choices": ["approve", "deny"] }),
        ),
        Some(_) => match broker.resolve_for_the_run(request_id) {
            Resolved::Applied => (
                StatusCode::OK,
                serde_json::json!({ "ok": true, "answer": "approve", "scope": "run", "note": "this edit goes ahead, and later edits of files in the same folder go ahead without asking until this run ends or it reads anything from outside the project" }),
            ),
            other => not_applied(other),
        },
    }
}

async fn browser_confirm_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<BrowserConfirmRequest>,
) -> Response {
    // Only the UI answers prompts. A caller with just the API token cannot approve its own action.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // An answer with words goes the way that keeps them (card OA10-03, AG-11); one without is answered exactly as it always was. An answer with a scope (card OA6-05) takes its own way.
    let (code, body) = match (payload.reason.as_deref(), payload.scope.as_deref()) {
        (reason, Some(scope)) => answer_confirmation_scoped(state.browser.broker(), &payload.request_id, &payload.decision, reason, Some(scope)),
        (Some(words), None) => answer_confirmation_with_reason(state.browser.broker(), &payload.request_id, &payload.decision, Some(words)),
        (None, None) => answer_confirmation(state.browser.broker(), &payload.request_id, &payload.decision),
    };
    (code, Json(body)).into_response()
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/consents", get(consents_list_endpoint))
        .route("/api/v1/consents/:id", axum::routing::delete(consent_revoke_endpoint))
        .route("/api/v1/browser/confirmations", get(browser_confirmations_endpoint))
        .route("/api/v1/browser/confirm", post(browser_confirm_endpoint))
}
