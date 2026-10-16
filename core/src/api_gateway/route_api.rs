//! Questions about routing and runs, answered without running the model or any tool: what the router would do with a request, the
//! agent runs that were recorded (`data/agent_runs.db`), and the trace of one.
//!
//! `route/explain` only looks at the text it is sent, so the API token is enough for it. The run records hold what the person asked
//! and what the tools returned, so reading them needs the UI token, like the conversation and the memory (a client with the API token
//! can start runs, and must not be able to read the person's history with it). They exist so the router can be tested with plain HTTP
//! calls and so a person can find out why Aqua did, or did not do, something.

use super::auth::{ui_scope_required, Scope};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .merge(super::attachments_api::routes())
        .route("/api/v1/route/explain", post(route_explain))
        .route("/api/v1/openapi.json", get(openapi_document))
        .route("/api/v1/runs", get(list_runs))
        .route("/api/v1/runs/:run_id", get(get_run))
        .route("/api/v1/runs/:run_id/trace", get(run_trace))
        .route("/api/v1/runs/:run_id/undo", post(undo_run))
        .route("/api/v1/runs/:run_id/diff", get(run_diff))
        .route("/api/v1/runs/tool-errors", get(tool_errors))
        .route("/api/v1/skills/proposals", get(skill_proposals))
        .route("/api/v1/skills/proposals/draft", post(skill_draft))
        .route("/api/v1/skills/proposals/:id/approve", post(skill_approve))
        .route("/api/v1/skills/proposals/:id/reject", post(skill_reject))
        .route("/api/v1/skills/:name/versions", get(skill_versions))
        .route("/api/v1/skills/:name/rollback", post(skill_rollback))
        .route("/api/v1/vault", get(vault_status))
        .route("/api/v1/vault/search", get(vault_search))
        .route("/api/v1/vault/folders", post(vault_add_folder))
        .route("/api/v1/vault/folders/remove", post(vault_remove_folder))
        .route("/api/v1/vault/index", post(vault_index))
        .route("/api/v1/vault/pause", post(vault_pause))
        .route("/api/v1/vault/forget", post(vault_forget))
        .route("/api/v1/runs/:run_id/safety", get(run_safety))
        .route("/api/v1/runs/:run_id/proof", get(run_proof))
        .route("/api/v1/audit/key", get(audit_key_info))
        .route("/api/v1/dictation/settings", get(dictation_settings).put(dictation_settings_put))
        .route("/api/v1/dictation/clean", post(dictation_clean))
        .route("/api/v1/queue", get(queue_list))
        .route("/api/v1/queue/:job_id/cancel", post(queue_cancel))
        .route("/api/v1/jobs", get(jobs_list))
        .route("/api/v1/jobs/:job_id/cancel", post(jobs_cancel))
        .route("/api/v1/config/status", get(config_status))
        .route("/api/v1/config/reload", post(config_reload))
        .route("/api/v1/flags", get(flags_list))
        .route("/api/v1/tools/providers", get(tool_providers_status))
        .route("/api/v1/tools/mcp/servers", get(mcp_servers_list))
        .route("/api/v1/tools/mcp/servers/:id/trust", post(mcp_server_trust))
        .route("/api/v1/tools/mcp/servers/:id/untrust", post(mcp_server_untrust))
        .route("/api/v1/memory/facts", get(memory_list).post(memory_add).delete(memory_clear))
        .route("/api/v1/memory/facts/:id", axum::routing::delete(memory_delete))
        .route("/api/v1/memory/export", get(memory_export))
        .route("/api/v1/memory/incognito", post(memory_incognito))
        .route("/api/v1/memory/proposals", get(memory_proposals))
        .route("/api/v1/memory/proposals/:id/accept", post(memory_proposal_accept))
        .route("/api/v1/memory/proposals/:id/reject", post(memory_proposal_reject))
        .route("/api/v1/memory/dream", post(memory_dream))
        .route("/api/v1/projects", get(list_projects))
        .route("/api/v1/projects/trust", post(trust_project))
        .route("/api/v1/projects/profile", get(project_profile_view))
        .route("/api/v1/projects/pack", get(project_pack_view))
        .route("/api/v1/projects/pack/trust", post(project_pack_trust))
        .route("/api/v1/projects/pack/untrust", post(project_pack_untrust))
        .route("/api/v1/commands", get(commands_list))
        .route("/api/v1/agents", get(agents_list))
        .route("/api/v1/metrics", get(metrics_text))
        .route("/api/v1/metrics/summary", get(metrics_summary))
        .route("/api/v1/conversation", get(get_conversation))
        .route("/api/v1/conversation/clear", post(clear_conversation))
}

/// The description of this API as an OpenAPI 3 document: every route, its methods, what it does and which token it needs. Generated from the
/// router's source by a test, so it cannot drift. Request and response bodies are not described yet.
async fn openapi_document() -> Response {
    Json(super::openapi::spec()).into_response()
}

#[derive(Deserialize)]
struct RouteExplainRequest {
    text: String,
    /// The request came by voice: a voice request may only look at the screen and open apps by name.
    #[serde(default)]
    voice: bool,
}

/// The route, the signals behind it and the tools the agent would be shown. It uses the rules only, so it can differ from the
/// live gate when the neural check would move a close call.
async fn route_explain(Json(body): Json<RouteExplainRequest>) -> Response {
    use crate::laya_validator::{fallback_decision, goal_is_coding_or_files, looks_actionable, looks_live_info, needs_web_lookup, GateRoute};
    let text = body.text.trim();
    if text.is_empty() || text.chars().count() > 4_000 {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "text must be 1 to 4000 characters" }))).into_response();
    }
    let decision = fallback_decision(text, true, "disabled");
    let packs = crate::agent_tools::route(text);
    // Tools are shown only to the agent: a request answered in chat or refused sees none, whatever words it contains.
    let tools_shown: Vec<String> = match (decision.route, crate::agent_tools::host()) {
        (GateRoute::McpAgent, Some(h)) => crate::agent_tools::tools_for_goal(h, text).into_iter().map(|t| t.name).collect(),
        _ => Vec::new(),
    };
    let coding = goal_is_coding_or_files(text);
    let live = looks_live_info(text);
    let web = needs_web_lookup(text);
    let reason = match decision.route {
        GateRoute::Refuse => format!("refused: {}", decision.source),
        GateRoute::McpAgent if coding => "a request to work on code or files".to_string(),
        GateRoute::McpAgent if live => "live information that has to be fetched".to_string(),
        GateRoute::McpAgent if web => "a question that has to be looked up".to_string(),
        GateRoute::McpAgent => "sounds like a task on this computer or the web".to_string(),
        GateRoute::Chat => "no task in it: answered in chat".to_string(),
    };
    Json(json!({
        "route": decision.route.as_str(),
        "reason": reason,
        "source": decision.source,
        "voice": body.voice,
        "signals": { "coding_or_files": coding, "live_info": live, "web_lookup": web, "actionable": looks_actionable(text) },
        "packs": packs,
        "tools_shown": tools_shown,
        "router_v2": crate::agent_tools::router_v2(),
    }))
    .into_response()
}

fn not_recording() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "agent runs are not being recorded (the run database could not be opened)" }))).into_response()
}

fn unknown_run(id: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no run called {id}") }))).into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    status: Option<String>,
    /// `1`: only the runs that stopped and can be gone on from (see `POST /api/v1/runs/{id}/resume`).
    resumable: Option<String>,
}

/// The newest runs first, 50 by default (500 at most). `status` narrows to one state (`running`, `completed`, `failed`,
/// `cancelled`, `interrupted`, `waiting_user`, `expired`); `resumable=1` lists the runs that stopped and can be gone on from, which nothing has gone on from yet
/// (a run in the background that Aqua's restart stopped is offered here, never started).
async fn list_runs(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ListQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let found = if matches!(q.resumable.as_deref(), Some("1" | "true" | "yes")) { store.list_resumable(limit) } else { store.list(limit, q.offset.unwrap_or(0), q.status.as_deref().filter(|s| !s.is_empty())) };
    match found {
        Ok(runs) => Json(json!({ "runs": runs })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

/// One run with all its steps: what it was asked, which tools it called (arguments redacted), what each returned and what ended it.
async fn get_run(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    match store.get(&run_id) {
        Ok(Some(run)) => Json(run).into_response(),
        Ok(None) => unknown_run(&run_id),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

/// The run as spans (`invoke_agent`, `plan`, `execute_tool`), and `steps`: its tool calls in order.
async fn run_trace(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    match store.get(&run_id) {
        Ok(Some(run)) => Json(crate::agent_runs::trace::trace(&run, crate::agent_runs::now_ms())).into_response(),
        Ok(None) => unknown_run(&run_id),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize, Default)]
struct UndoRequest {
    /// Only say what would be put back.
    #[serde(default)]
    dry_run: bool,
    /// Put back only the changes from this step of the run on (the first file-changing call of the run is step 1). Absent: all of them.
    #[serde(default)]
    from_step: Option<u32>,
    /// What to put back (card OA7-03): `files` (what the file tools changed), `commands` (what commands changed: their journal and the run's checkpoint) or `all` (the default).
    #[serde(default)]
    scope: Option<String>,
}

/// The tools a run used whose effects no undo reaches: every one that is not a file tool (those leave records) and not a read, such as a
/// command, a commit, a network call or a click. Browser and desktop tools are looked up by the host's name for them.
pub fn not_undoable(tools: &[String], manifests: &[aqua_tool_host::ToolManifest]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tools {
        let canonical = aqua_mcp_adapters::browser::provider::host_name_for_wire(t)
            .or_else(|| aqua_mcp_adapters::windows_mcp::provider::host_name_for_wire(t))
            .unwrap_or(t.as_str());
        let read_only = manifests.iter().find(|m| m.name == canonical).is_some_and(|m| m.annotations.read_only) || t == "local_find";
        if !read_only && !canonical.starts_with("fs.") && !out.contains(t) {
            out.push(t.clone());
        }
    }
    out.sort();
    out
}

/// The person's dictation settings (`data/dictation.json`): the cleanup level, own words, replacements, snippets. A file that cannot be read is
/// said (`error`) and the defaults are shown, so dictation still works. The UI token: the settings hold the person's own words.
async fn dictation_settings(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::dictation::load(&crate::runtime::data_dir()) {
        Ok(s) => Json(json!({ "settings": s })).into_response(),
        Err(e) => Json(json!({ "settings": crate::dictation::Settings::default(), "error": e })).into_response(),
    }
}

/// Saves the dictation settings (checked first: a wrong one is refused with the reason and nothing is written).
async fn dictation_settings_put(axum::Extension(scope): axum::Extension<Scope>, Json(settings): Json<crate::dictation::Settings>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::dictation::save(&crate::runtime::data_dir(), &settings) {
        Ok(()) => Json(json!({ "settings": settings })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct DictationCleanRequest {
    text: String,
}

/// Cleans a transcript with the person's dictation settings and says what was done. Nothing is run and nothing is kept.
async fn dictation_clean(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<DictationCleanRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if body.text.chars().count() > 20_000 {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "text is at most 20000 characters" }))).into_response();
    }
    let settings = crate::dictation::load(&crate::runtime::data_dir()).unwrap_or_default();
    // The medium level starts the model's program for a moment, so it is not run on the async thread.
    let text = body.text.clone();
    let cleaned = match tokio::task::spawn_blocking({
        let settings = settings.clone();
        move || crate::dictation::clean_for(&text, &settings, &crate::runtime::data_dir())
    })
    .await
    {
        Ok(c) => c,
        Err(_) => crate::dictation::clean_with(&body.text, &settings),
    };
    Json(cleaned).into_response()
}

#[derive(Deserialize)]
struct ToolErrorsQuery {
    days: Option<u32>,
}

/// What went wrong with tools lately, by tool and by kind (arguments the tool did not accept, the world not as expected, the tool or its service failing,
/// the person or the policy saying no), and how many runs ended because the model's reply could not be read. From the run records. The UI token.
async fn tool_errors(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ToolErrorsQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    let days = q.days.unwrap_or(7).clamp(1, 90);
    let since = crate::agent_runs::now_ms() - i64::from(days) * 24 * 3600 * 1000;
    match store.calls_since(since) {
        Ok((calls, reasons)) => {
            let (tally, total) = crate::agent_runs::taxonomy::tally(calls.iter().map(|(t, o)| (t.as_str(), o.as_str())));
            let parse_failures = reasons.iter().filter(|r| crate::agent_runs::taxonomy::is_parse_failure(r)).count();
            Json(json!({ "days": days, "calls": total, "errors_by_tool": tally, "planner_reply_unreadable": parse_failures, "runs": reasons.len() })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

/// The skills Aqua proposes after a task that went well, waiting for the person to read, change and approve them. The UI token.
async fn skill_proposals(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(json!({ "proposals": crate::skills_inbox::list(&crate::runtime::data_dir()) })).into_response()
}

#[derive(Deserialize)]
struct DraftRequest {
    run_id: String,
    /// Draft from a run that read text from outside too (it is quarantined otherwise).
    #[serde(default)]
    force: bool,
}

/// Drafts a skill from a finished run, by template, and puts it in the inbox. A run that read text from outside is not proposed unless `force` is set.
async fn skill_draft(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<DraftRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    // What the drafter reads of the run is read in one place, which the draft that is made by itself after a good run (card R6-16) uses too.
    let input = match crate::skills_inbox::input_from_run(store, &body.run_id) {
        Ok(Some(input)) => input,
        Ok(None) => return unknown_run(&body.run_id),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };
    match crate::skills_inbox::draft(&input, body.force) {
        Ok(p) => match crate::skills_inbox::add(&crate::runtime::data_dir(), p, crate::agent_runs::now_ms()) {
            Ok(p) => (StatusCode::CREATED, Json(json!({ "proposal": p }))).into_response(),
            Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
        },
        Err(refusal) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": refusal.message() }))).into_response(),
    }
}

/// The person approves a proposal (with their changes): it becomes a skill and can be loaded at once (the skills are read again). A skill can only narrow what a run may do.
async fn skill_approve(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>, body: Option<Json<crate::skills_inbox::Edits>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let edits = body.map(|Json(b)| b).unwrap_or_default();
    match crate::skills_inbox::approve(&crate::runtime::data_dir(), &id, &edits) {
        Ok((name, version)) => {
            // The skills are read again, so the first skill a person approves is there for the next call of `skill.load` (card OA11-01, task EX-07); it used to wait for a restart.
            crate::skills::reload(&crate::runtime::data_dir());
            Json(json!({ "skill": name, "version": version, "note": "it can be loaded now" })).into_response()
        }
        // The name is one of the person's own skills: nothing was written, and the answer says how to go on (another name, or `replace: true`).
        Err(e) if crate::skills_inbox::is_collision(&e) => (StatusCode::CONFLICT, Json(json!({ "error": e, "collision": true }))).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": e }))).into_response(),
    }
}

async fn skill_reject(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::skills_inbox::reject(&crate::runtime::data_dir(), &id) {
        Ok(()) => Json(json!({ "rejected": id })).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

async fn skill_versions(axum::Extension(scope): axum::Extension<Scope>, Path(name): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(json!({ "skill": name, "versions": crate::skills_inbox::versions(&crate::runtime::data_dir(), &name) })).into_response()
}

#[derive(Deserialize)]
struct RollbackRequest {
    version: usize,
}

/// Puts an earlier version of a skill back (the one it replaces is kept too).
async fn skill_rollback(axum::Extension(scope): axum::Extension<Scope>, Path(name): Path<String>, Json(body): Json<RollbackRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') || name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "a skill name is a-z, 0-9, - and _" }))).into_response();
    }
    match crate::skills_inbox::rollback(&crate::runtime::data_dir(), &name, body.version) {
        Ok(()) => {
            crate::skills::reload(&crate::runtime::data_dir());
            Json(json!({ "skill": name, "restored": body.version })).into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": e }))).into_response(),
    }
}

fn no_vault() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the vault is not open" }))).into_response()
}

/// The vault: which folders are read, how much is in it, whether it is paused or being read now, and how the last reading went. The UI token.
async fn vault_status(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    Json(json!({ "stats": v.stats(), "indexing": crate::vault::indexing(), "last_index": crate::vault::last_index() })).into_response()
}

#[derive(Deserialize)]
struct VaultSearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn vault_search(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<VaultSearchQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    Json(json!({ "passages": v.search(&q.q, q.limit.unwrap_or(5)), "note": "a keyword search: there is no embedding model on this machine yet" })).into_response()
}

#[derive(Deserialize)]
struct VaultFolderRequest {
    path: String,
}

/// Adds a folder to read (nothing is read until it is indexed). Secrets by name, links and large files are never read.
async fn vault_add_folder(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<VaultFolderRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    match v.add_folder(std::path::Path::new(&body.path)) {
        Ok(shown) => Json(json!({ "folder": shown })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// Takes a folder off the list and forgets everything read from it, for real (the file is overwritten and compacted).
async fn vault_remove_folder(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<VaultFolderRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    let v = v.clone();
    match tokio::task::spawn_blocking(move || v.remove_folder(&body.path)).await {
        // The documents and the folder are gone even when the file could not be compacted; that is said, not turned into a failure.
        Ok(Ok(removed)) => Json(json!({ "forgotten_documents": removed.documents, "compact_error": removed.compact_error })).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the vault task stopped unexpectedly" }))).into_response(),
    }
}

/// Reads the folders in the background, between the person's requests.
async fn vault_index(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    if crate::vault::index_in_background(v.clone(), || crate::queue::foreground_active() == 0) {
        (StatusCode::ACCEPTED, Json(json!({ "indexing": true }))).into_response()
    } else {
        (StatusCode::CONFLICT, Json(json!({ "error": "the vault is being read already" }))).into_response()
    }
}

#[derive(Deserialize)]
struct VaultPauseRequest {
    paused: bool,
}

async fn vault_pause(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<VaultPauseRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    match v.set_paused(body.paused) {
        Ok(()) => Json(json!({ "paused": body.paused })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct VaultForgetRequest {
    /// Must be exactly "forget everything": this cannot be undone.
    confirm: String,
}

async fn vault_forget(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<VaultForgetRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if body.confirm != "forget everything" {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "say \"forget everything\" in confirm: this cannot be undone" }))).into_response();
    }
    let Some(v) = crate::vault::global() else { return no_vault() };
    let v = v.clone();
    match tokio::task::spawn_blocking(move || v.forget_all()).await {
        Ok(Ok(n)) => Json(json!({ "forgotten_documents": n })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the vault task stopped unexpectedly" }))).into_response(),
    }
}

/// What one run could read, change and send, in plain words and as data, with when it came to hold each part: text from outside, the person's own
/// data, and a way to change or send. Worked out from the run's record. The UI token: it shows what the run did.
async fn run_safety(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    let run = match store.get(&run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return unknown_run(&run_id),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };
    let host = crate::agent_tools::host().cloned();
    let safety = crate::agent_runs::safety::assess(&run, store.marks_detail(&run_id).ok().flatten(), &|t| crate::agent_tools::can_change_something(host.as_deref(), t), &|t| crate::agent_tools::can_send_something(host.as_deref(), t));
    let page = crate::agent_runs::safety::one_page(&run, &safety, None);
    Json(json!({ "safety": safety, "page": page })).into_response()
}

/// The ledger lines of one run with the signed tree head each is under and the proof that it is in that head's batch: a bundle that somebody who has
/// only the public key (see `GET /api/v1/audit/key`) can check, with `aqua-cli verify-bundle`. The ledger is sealed first so that the newest lines are
/// covered. The answer also says whether this install's own check passes. The UI token.
async fn run_proof(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(host) = crate::agent_tools::host().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the tool host is not running" }))).into_response();
    };
    let id = run_id.clone();
    let done = tokio::task::spawn_blocking(move || {
        let keys = crate::audit_key::known_public_keys(&crate::runtime::data_dir());
        let proof = aqua_tool_host::proof::run_proof(host.audit(), &id, &keys);
        let report = aqua_tool_host::verify_bundle(&proof, &keys);
        (proof, report.all_good(), report.problems)
    })
    .await;
    let Ok((proof, good, problems)) = done else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the proof stopped unexpectedly" }))).into_response();
    };
    if proof.lines.is_empty() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "the ledger holds no line of this run" }))).into_response();
    }
    Json(json!({ "proof": proof, "checks_out_here": good, "problems": problems })).into_response()
}

/// The public key that signs the ledger's tree heads, and the earlier ones: what a person writes down somewhere Aqua cannot reach, and what somebody
/// who is shown a run checks it against. The UI token.
async fn audit_key_info(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let keys = crate::audit_key::known_keys(&crate::runtime::data_dir());
    Json(json!({ "current": keys.last(), "all": keys, "note": "Keep the key id and the public key somewhere Aqua cannot reach." })).into_response()
}

/// The background runs, newest first, and whether a person is being served now (a background run waits for that at each step). The UI token:
/// goals and answers are what the person asked for.
async fn queue_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let (waiting, running, foreground) = crate::queue::counts();
    // `depth` says it in one place: how many wait, whether the run that is going is paused for the person, how many places are free, how many ends are yet to be told.
    Json(json!({ "jobs": crate::queue::list(), "waiting": waiting, "running": running, "foreground_active": foreground, "max_waiting": crate::queue::MAX_WAITING, "depth": crate::queue::depth() })).into_response()
}

/// Takes a waiting background run out, or tells a running one to stop at its next step.
async fn queue_cancel(axum::Extension(scope): axum::Extension<Scope>, Path(job_id): Path<u64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::queue::cancel(job_id) {
        Ok(state) => Json(json!({ "job_id": job_id, "state": state })).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// The background jobs of the core (the flow scheduler, the clock that wakes flow runs, the history prune, the workflow scheduler, the self-repair watch, the run watchdog, the skill tidy ...) with
/// their kind, state, start, last sign of life, what they promise and whether they are stuck (no sign of life for five times what they promised); the ones that ended say how and why. Names,
/// kinds and times only: never what a job works on. The UI token.
async fn jobs_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let jobs = crate::jobs::list();
    let count = |state: &str| jobs.iter().filter(|j| j.state == state).count();
    Json(json!({
        "jobs": jobs,
        "running": count("running") + count("stopping"),
        "stuck": jobs.iter().filter(|j| j.stuck).count(),
        "stuck_after_beats": crate::jobs::STUCK_AFTER_BEATS,
        "now_ms": crate::jobs::now_ms(),
    }))
    .into_response()
}

/// Tells one background job to stop at its next look (it does not kill it: a job inside a long call ends when the call returns). It answers at once with the job as it is then, `stopping`. A job that is
/// stopped by hand stays stopped until Aqua restarts. The UI token.
async fn jobs_cancel(axum::Extension(scope): axum::Extension<Scope>, Path(job_id): Path<u64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::jobs::cancel(job_id) {
        Ok(job) => Json(json!({ "job": job })).into_response(),
        Err(crate::jobs::CancelError::Unknown) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no job has the number {job_id}") }))).into_response(),
        Err(crate::jobs::CancelError::AlreadyOver(job)) => (StatusCode::CONFLICT, Json(json!({ "error": "the job has ended already", "job": job }))).into_response(),
    }
}

/// Puts back what one finished run changed in files: the app's "Undo this run" (the UI token, because it writes). It goes through the tool
/// host's `fs.undo_run`, so it asks like any write and is in the ledger. The answer also lists what the run did that no undo can reach.
async fn undo_run(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, body: Option<Json<UndoRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let (dry_run, from_step, undo_scope) = body.map(|Json(b)| (b.dry_run, b.from_step.filter(|s| *s > 0), b.scope.filter(|s| !s.is_empty()))).unwrap_or((false, None, None));
    if let Some(s) = undo_scope.as_deref() {
        if !matches!(s, "all" | "files" | "commands") {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "scope must be \"all\", \"files\" or \"commands\"" }))).into_response();
        }
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    let run = match store.get(&run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return unknown_run(&run_id),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    };
    if matches!(run["status"].as_str(), Some("running" | "waiting_user")) {
        return (StatusCode::CONFLICT, Json(json!({ "error": "the run is still going: stop it first, then undo it" }))).into_response();
    }
    let Some(host) = crate::agent_tools::host().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the tool host is not running" }))).into_response();
    };
    let tools: Vec<String> = run["steps"].as_array().map(|s| s.iter().filter(|x| x["kind"] == "tool").filter_map(|x| x["tool"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let manifests = host.manifests();
    let id = run_id.clone();
    let done = tokio::task::spawn_blocking(move || {
        let mut ctx = aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Ui);
        ctx.agent_run_id = String::new();
        let mut arguments = json!({ "run_id": id, "dry_run": dry_run });
        if let Some(step) = from_step {
            arguments["from_step"] = json!(step);
        }
        if let Some(scope) = undo_scope {
            arguments["scope"] = json!(scope);
        }
        host.call_blocking(aqua_tool_host::ToolRequest { tool: "fs.undo_run".into(), arguments, context: ctx })
    })
    .await;
    let Ok(result) = done else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the undo stopped unexpectedly" }))).into_response();
    };
    let status = if result.ok { StatusCode::OK } else { StatusCode::UNPROCESSABLE_ENTITY };
    // A card that a spoken "undo the last run" put on the screen is done with once the undo has been pressed (card OA10-06).
    if result.ok && !dry_run {
        crate::coding_voice::staged().forget_run(&run_id);
    }
    let body = json!({
        "run_id": run_id,
        "undo": serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
        "not_undoable": not_undoable(&tools, &manifests),
        "note": "Commands, commits and network calls cannot be taken back: the tools listed in not_undoable were used by this run. The files a command changed are put back too when the run has a record of them (the journal of commands, or the run's checkpoint with AQUA_SNAPSHOT=on); nothing else a command did is.",
    });
    (status, Json(body)).into_response()
}

/// What one finished run changed, whatever changed it (card OA7-03, ED-11): the files of its checkpoint (`AQUA_SNAPSHOT=on`) with the patch of each (100 files and 200 lines each at most,
/// masked like any tool result), and the commands the journal looked at (`AQUA_TERM_JOURNAL=on`) with the files each changed. A run with neither says so (`checkpoint` is null and `commands`
/// is empty). The UI token: the answer holds the contents of the person's files.
async fn run_diff(axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = crate::agent_runs::store() else { return not_recording() };
    match store.get(&run_id) {
        Ok(Some(_)) => {}
        Ok(None) => return unknown_run(&run_id),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
    let id = run_id.clone();
    match tokio::task::spawn_blocking(move || run_diff_body(&id)).await {
        Ok(body) => Json(body).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the diff stopped unexpectedly" }))).into_response(),
    }
}

/// The body of `GET /api/v1/runs/{id}/diff`: see [`run_diff`].
pub fn run_diff_body(run_id: &str) -> serde_json::Value {
    // What goes out is masked as the text of a tool result is: a file can hold a key.
    let mask = |text: &str| crate::agent_tools::host().map(|h| h.redact_text(text)).unwrap_or_else(|| aqua_tool_host::secret_patterns::mask(text));
    run_diff_json(run_id, crate::tool_providers::checkpoints().as_deref(), crate::tool_host::TRASH.get().map(|t| t.as_ref()), crate::tool_providers::journal().as_deref(), &mask)
}

/// What a run changed, as `GET /api/v1/runs/{id}/diff` answers it: from the run's checkpoint (when it has a finished one) and from the commands the journal recorded for it, every patch
/// through `mask`. The things it reads are given, so that a test can make its own.
pub fn run_diff_json(
    run_id: &str,
    checkpoints: Option<&aqua_native_tools::git::checkpoint::Checkpoints>,
    trash: Option<&aqua_native_tools::fs::Trash>,
    journal: Option<&aqua_native_tools::git::journal::Journal>,
    mask: &dyn Fn(&str) -> String,
) -> serde_json::Value {
    use aqua_native_tools::git::checkpoint::DIFF_FILES;
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let listed = |changes: &[aqua_native_tools::git::capture::Change], max: usize| changes.iter().take(max).map(|c| json!({ "path": c.path, "status": c.status.to_string() })).collect::<Vec<_>>();
    let mut checkpoint = serde_json::Value::Null;
    let mut problem: Option<String> = None;
    if let Some(cp) = checkpoints {
        match cp.diff_of_run(run_id, &stopped) {
            Some(Ok(diff)) => {
                let files: Vec<serde_json::Value> = diff.patches.iter().map(|p| json!({ "path": p.path, "status": p.status.to_string(), "patch": mask(&p.patch), "truncated": p.truncated, "binary": p.binary })).collect();
                checkpoint = json!({
                    "root": aqua_native_tools::fs::guard::display(&diff.root),
                    "changed": diff.changes.len(),
                    "paths": listed(&diff.changes, 500),
                    "files": files,
                    "more_files": diff.changes.len().saturating_sub(DIFF_FILES),
                });
            }
            Some(Err(e)) => problem = Some(e.message),
            None => {}
        }
    }
    let mut commands: Vec<serde_json::Value> = Vec::new();
    if let (Some(trash), Some(journal)) = (trash, journal) {
        for entry in trash.run_entries(run_id).iter().filter(|e| e.reason == "command") {
            let Some(record) = aqua_native_tools::git::journal::Record::from_entry(entry) else { continue };
            let (files, error) = match journal.changes_of(&record, &stopped) {
                Ok(changes) => (listed(&changes, 50), None),
                Err(e) => (Vec::new(), Some(e.message)),
            };
            commands.push(json!({ "call_id": record.id, "command": record.label, "changed": record.changed, "files": files, "error": error }));
        }
    }
    json!({
        "run_id": run_id,
        "checkpoints_on": crate::flags::is_on(&crate::flags::SNAPSHOT),
        "checkpoint": checkpoint,
        "commands": commands,
        "error": problem,
    })
}

/// The state of every config file the person edits by hand (`data/tools/*.json`, `hooks.json`, `mcp.json`): what is on disk now, where a mistake is (line and column), what a broken file turns
/// off, whether the core read it as it is now, and a hint for a key that is not known. Paths, states and positions only: never what a file holds. The UI token.
async fn config_status(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // Sixteen small files are read; the answer does not wait on the runtime's other work.
    let data_dir = crate::runtime::data_dir();
    match tokio::task::spawn_blocking(move || crate::config_files::status_json(&data_dir)).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("the config files could not be checked: {e}") }))).into_response(),
    }
}

/// The typed flags (`AQUA_*` switches the core reads in one place): name, kind, default, what each is now, what it does, and whether a promotion may set it. A value is what the code
/// reads it as (`on`, `off`, a choice, a number), never the text of the variable. The UI token.
async fn flags_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(json!({ "flags": crate::flags::views() })).into_response()
}

/// Reads the config files again while Aqua runs, for what cannot loosen anything (card OA11-03, task EX-13): `{"what": ["hooks", "skills", "mcp", "pack", "commands", "agents"]}` (none: all of them). A
/// hook that was running and is gone or changed waits for a restart; a server that is new or changed is not trusted; nothing is read again while a run is going on (it is done when the next run starts).
/// `policy.json`, `fs.json`, `term*.json`, `http.json` and the like are never read again. The UI token: it changes what Aqua checks and runs.
async fn config_reload(axum::Extension(scope): axum::Extension<Scope>, body: axum::body::Bytes) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    #[derive(Deserialize, Default)]
    struct Body {
        #[serde(default)]
        what: Vec<String>,
    }
    let asked: Body = if body.iter().all(|b| b.is_ascii_whitespace()) {
        Body::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("the body is {{\"what\": [..names..]}}: {e}") }))).into_response(),
        }
    };
    let data_dir = crate::runtime::data_dir();
    match tokio::task::spawn_blocking(move || crate::config_reload::reload(&data_dir, &asked.what)).await {
        Ok(Ok(items)) => Json(json!({ "results": items, "never_read_again": crate::config_reload::NOT_RELOADABLE, "waiting": crate::config_reload::pending_kinds() })).into_response(),
        Ok(Err(why)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the reload stopped unexpectedly" }))).into_response(),
    }
}

/// What became of each family of tools when the host was built: registered (how many tools), off (and why: a switch, a broken config file, a missing program) or failed (and why). The UI token.
async fn tool_providers_status(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(crate::tool_providers::status_json()).into_response()
}

/// The servers the person added in `mcp.json`: what each runs (the exact command line), whether it is trusted, and its tools with what the server
/// wrote about them (for the person to read; the model never sees it). The UI token: it shows what programs the person has set up.
async fn mcp_servers_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match crate::tool_host::mcp_servers() {
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the tool host is not running" }))).into_response(),
        Some(Err(e)) => Json(json!({ "servers": [], "error": format!("mcp.json is wrong: {e}") })).into_response(),
        Some(Ok(m)) => {
            let views = m.views();
            let summary = aqua_mcp_adapters::user_servers::Summary::of(&views);
            Json(json!({ "servers": views, "summary": summary })).into_response()
        }
    }
}

/// The person trusts a server as it is written in `mcp.json`: it is started, its tools are recorded and added, and the trust is kept for this
/// entry. The UI token.
async fn mcp_server_trust(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(Ok(m)) = crate::tool_host::mcp_servers() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "there is no usable mcp.json" }))).into_response();
    };
    let m = m.clone();
    let name = id.clone();
    // The pack of a server of a repository is read again first: a server of a pack that the person does not trust any more is not there to be trusted (card OA11-03).
    match tokio::task::spawn_blocking(move || {
        crate::mcp_exposure::refresh_projects();
        m.trust(&name)
    })
    .await
    {
        Ok(Ok(())) => Json(json!({ "trusted": id })).into_response(),
        Ok(Err(why)) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": why }))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "the start stopped unexpectedly" }))).into_response(),
    }
}

/// The person stops trusting a server: its calls are refused, it is stopped and its tools leave the host. The UI token.
async fn mcp_server_untrust(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(Ok(m)) = crate::tool_host::mcp_servers() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "there is no usable mcp.json" }))).into_response();
    };
    match m.untrust(&id) {
        Ok(()) => Json(json!({ "untrusted": id })).into_response(),
        Err(why) => (StatusCode::NOT_FOUND, Json(json!({ "error": why }))).into_response(),
    }
}

/// What Aqua remembers of the last few turns (see `conversation`): kept in memory only, never written to disk. It holds what the person
/// said, so it needs the UI token, not the API token.
async fn get_conversation(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(crate::conversation::snapshot()).into_response()
}

/// Forget it: the turns, what they were about, and a question that was waiting for an answer.
async fn clear_conversation(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // What the coding sessions that are still open kept of the work (the names of files, the notes) is forgotten with it, for good (card OA10-04, CX-08): the person asked to be forgotten. (Before
    // the conversation is cleared: clearing it closes the open sessions, which keeps their records.)
    let sessions_forgotten = crate::agent_runs::store().and_then(|s| s.forget_open_coding_sessions().ok()).unwrap_or(0);
    crate::conversation::clear();
    Json(json!({ "cleared": true, "coding_sessions_forgotten": sessions_forgotten })).into_response()
}

/// Counters, gauges and histograms in the Prometheus text format: turns by route, tool calls by what was decided, questions asked, planner
/// latency and tokens (read, cached, written), model loads, errors. Names and numbers only, never what was asked or answered, so the API
/// token is enough.
async fn metrics_text() -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], crate::metrics::render()).into_response()
}

/// The few numbers a diagnostics page shows live: planner latency (p50, p90), how much of each prompt came from the server's cache.
async fn metrics_summary() -> Response {
    Json(crate::metrics::summary()).into_response()
}

/// The folders the person has trusted for their own project notes (`AQUA.md` or `AGENTS.md`), and whether each file is still the text that was
/// trusted. UI token: it is the person's decision, not the API's.
async fn list_projects(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    Json(json!({ "trusted": crate::project_instructions::list(&crate::runtime::data_dir()) })).into_response()
}

#[derive(Deserialize)]
struct TrustRequest {
    folder: String,
    /// `false` withdraws the trust.
    #[serde(default = "yes")]
    trust: bool,
}

fn yes() -> bool {
    true
}

/// Trust (or stop trusting) the notes file in a folder inside the folders Aqua works in. The trust is for the file as it is now.
async fn trust_project(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<TrustRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let roots: Vec<std::path::PathBuf> = match crate::tool_host::PROJECT_GUARD.get() {
        Some(g) => g.roots().into_iter().map(|(_, p, _)| p).collect(),
        None => vec![crate::runtime::workspace_root()],
    };
    let raw = body.folder.trim();
    let candidate = if let Some(rest) = raw.strip_prefix("@ws/").or_else(|| raw.strip_prefix("@ws\\")) { roots.first().map(|r| r.join(rest)) } else { Some(std::path::PathBuf::from(raw)) };
    let Some(dir) = candidate.filter(|d| d.is_dir() && roots.iter().any(|r| aqua_native_tools::fs::guard::within(d, r))) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response();
    };
    let data = crate::runtime::data_dir();
    let result = if body.trust { crate::project_instructions::trust(&data, &dir).map(|_| true) } else { crate::project_instructions::untrust(&data, &dir).map(|_| false) };
    match result {
        Ok(trusted) => Json(json!({ "trusted": trusted, "list": crate::project_instructions::list(&data) })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

/// What a project is and how it is checked, and a draft of the note it could keep for a coding agent (`AQUA.md`), made from its manifests (card OA4-05, task CU-11): `?folder=` names a folder of the project (the first folder Aqua works in when it is left out), the answer is the profile of the project that holds it (its languages, package manager, test runner and the command of each check), and `markdown` is the draft. The UI token. It reads the manifests and writes nothing: the person reads the draft, saves it where it belongs and trusts that file.
async fn project_profile_view(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<PackQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let roots: Vec<std::path::PathBuf> = pack_roots().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let raw = q.folder.unwrap_or_default();
    let raw = raw.trim();
    let candidate = if raw.is_empty() {
        roots.first().cloned()
    } else if let Some(rest) = raw.strip_prefix("@ws/").or_else(|| raw.strip_prefix("@ws\\")) {
        roots.first().map(|r| r.join(rest))
    } else {
        Some(std::path::PathBuf::from(raw))
    };
    let dir = candidate.and_then(|c| std::fs::canonicalize(c).ok()).filter(|d| d.is_dir());
    let limit = dir.as_ref().and_then(|d| roots.iter().find(|r| aqua_native_tools::fs::guard::within(d, r)));
    let (Some(dir), Some(limit)) = (dir.as_ref(), limit) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response();
    };
    let root = aqua_native_tools::code::nearest_root(dir, limit).unwrap_or_else(|| dir.clone());
    let profile = aqua_native_tools::code::detect(&root);
    Json(json!({ "folder": aqua_native_tools::fs::guard::display(&root), "profile": serde_json::to_value(&profile).unwrap_or(serde_json::Value::Null), "markdown": profile.draft_markdown() })).into_response()
}

// ---- a project's `.aqua` pack: seen and trusted by the person (card OA11-01, task EX-06) -------------------------------------------------------------

#[derive(Deserialize)]
struct PackQuery {
    folder: Option<String>,
}

/// The folders Aqua works in (the workspace first), as a goal's project is looked for in them.
fn pack_roots() -> Vec<std::path::PathBuf> {
    match crate::tool_host::PROJECT_GUARD.get() {
        Some(g) => g.roots().into_iter().map(|(_, p, _)| p).collect(),
        None => vec![crate::runtime::workspace_root()],
    }
}

/// The project folder that holds the `.aqua` the folder `raw` is in or below: the folder must exist and be inside the folders Aqua works in (compared as real paths, so a `..` or a link
/// leads nowhere). `Err` is the answer to give.
fn pack_of_folder(raw: &str) -> Result<(std::path::PathBuf, crate::project_pack::Pack), Response> {
    let roots: Vec<std::path::PathBuf> = pack_roots().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let raw = raw.trim();
    let candidate = if let Some(rest) = raw.strip_prefix("@ws/").or_else(|| raw.strip_prefix("@ws\\")) { roots.first().map(|r| r.join(rest)) } else { Some(std::path::PathBuf::from(raw)) };
    let dir = candidate.and_then(|c| std::fs::canonicalize(c).ok()).filter(|d| d.is_dir());
    let limit = dir.as_ref().and_then(|d| roots.iter().find(|r| aqua_native_tools::fs::guard::within(d, r)));
    let (Some(dir), Some(limit)) = (dir.as_ref(), limit) else {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response());
    };
    let found = crate::project_pack::find(dir, limit);
    match found.root {
        Some(root) => {
            let pack = crate::project_pack::scan(&root.join(crate::project_pack::DIR));
            Ok((root, pack))
        }
        None => Err((StatusCode::NOT_FOUND, Json(json!({ "error": "there is no .aqua folder at or above that folder", "warnings": found.warnings }))).into_response()),
    }
}

fn packs_are_off() -> Response {
    (StatusCode::CONFLICT, Json(json!({ "error": "project packs are off: set AQUA_PROJECT_PACK=on and restart Aqua to use them", "enabled": false }))).into_response()
}

/// A project's `.aqua` pack: for each of its two kinds (text: skills, commands, agents; code: hooks, MCP servers, formatters, tools) whether the person trusts it, the files with their
/// sizes and hashes, the digest a yes is tied to, and for the **code** the whole text of every file, so that what is trusted is what was read. Nothing of the pack is used before it is
/// trusted. Without `folder`: the packs the person has trusted and whether each still is. The UI token (the API token cannot trust, and has no reason to look).
async fn project_pack_view(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<PackQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let data = crate::runtime::data_dir();
    let key = crate::tool_host::pack_trust_key(false);
    let Some(folder) = q.folder.filter(|f| !f.trim().is_empty()) else {
        return Json(json!({ "enabled": crate::project_context::enabled(), "trusted": crate::project_pack::list(&data, key.as_deref()) })).into_response();
    };
    if !crate::project_context::enabled() {
        return Json(json!({ "enabled": false, "note": "project packs are off: set AQUA_PROJECT_PACK=on and restart Aqua to use them" })).into_response();
    }
    match pack_of_folder(&folder) {
        Ok((root, pack)) => {
            let standings = crate::project_pack::standings(&data, key.as_deref(), &root, &pack);
            let mut v = crate::project_pack::view(&root, &pack, &standings);
            v["enabled"] = json!(true);
            v["key"] = json!(key.is_some());
            Json(v).into_response()
        }
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct PackTrustRequest {
    folder: String,
    /// `"text"`, `"code"`; empty or missing is every kind the pack has.
    #[serde(default)]
    kinds: Vec<String>,
    /// The digest of each kind as the person was shown it: when the files are not these any more, nothing is trusted.
    #[serde(default)]
    expect: std::collections::BTreeMap<String, String>,
}

fn pack_kinds(words: &[String]) -> Result<Vec<crate::project_pack::Kind>, Response> {
    words
        .iter()
        .map(|w| crate::project_pack::Kind::parse(w).ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("kinds are text and code, not {w:?}") }))).into_response()))
        .collect()
}

/// The person trusts the files of a pack, **by kind**, as they are now. The UI token only: the API token and the voice cannot give it. Trusting the code kind does not start anything: an
/// MCP server of the pack still needs its own trust, and nothing runs but hooks (which can only tighten). Written in the audit ledger.
async fn project_pack_trust(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<PackTrustRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if !crate::project_context::enabled() {
        return packs_are_off();
    }
    let (root, pack) = match pack_of_folder(&body.folder) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let kinds = match pack_kinds(&body.kinds) {
        Ok(k) if k.is_empty() => crate::project_pack::Kind::ALL.into_iter().filter(|k| !pack.of_kind(*k).is_empty()).collect(),
        Ok(k) => k,
        Err(r) => return r,
    };
    let Some(key) = crate::tool_host::pack_trust_key(true) else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "there is no key to sign the record of trust with and none could be made (the credential store cannot be used), so nothing can be trusted" }))).into_response();
    };
    let data = crate::runtime::data_dir();
    let result = crate::project_pack::trust(&data, &key, &root, &pack, &kinds, &body.expect);
    let words = kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(",");
    aqua_tool_host::record_direct("ui", "pack.trust", &format!("{} [{words}]", root.display()), result.is_ok(), 0);
    match result {
        Ok(()) => {
            let standings = crate::project_pack::standings(&data, Some(&key), &root, &pack);
            Json(json!({ "trusted": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(), "pack": crate::project_pack::view(&root, &pack, &standings) })).into_response()
        }
        Err(crate::project_pack::TrustError::Changed(e)) => (StatusCode::CONFLICT, Json(json!({ "error": e, "changed": true }))).into_response(),
        Err(crate::project_pack::TrustError::Invalid(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Err(crate::project_pack::TrustError::Record(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct PackUntrustRequest {
    folder: String,
    #[serde(default)]
    kinds: Vec<String>,
}

/// The person stops trusting a pack (or one kind of it). Needs no key and is allowed whatever `AQUA_PROJECT_PACK` says: withdrawing trust is never refused. The UI token.
async fn project_pack_untrust(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<PackUntrustRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let roots: Vec<std::path::PathBuf> = pack_roots().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let dir = std::fs::canonicalize(body.folder.trim()).ok().filter(|d| d.is_dir() && roots.iter().any(|r| aqua_native_tools::fs::guard::within(d, r)));
    let Some(dir) = dir else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response();
    };
    let kinds = match pack_kinds(&body.kinds) {
        Ok(k) => k,
        Err(r) => return r,
    };
    let result = crate::project_pack::untrust(&crate::runtime::data_dir(), &dir, &kinds);
    aqua_tool_host::record_direct("ui", "pack.untrust", &dir.display().to_string(), result.is_ok(), 0);
    match result {
        Ok(had) => Json(json!({ "untrusted": had })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

// ---- custom commands (card OA11-02, task EX-08) -------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CommandsQuery {
    folder: Option<String>,
}

/// The project folder a command is started in: it must exist and be inside the folders Aqua works in (compared as real paths). `(the folder, the root it is in)`.
fn folder_in_roots(raw: &str) -> Result<(std::path::PathBuf, std::path::PathBuf), Response> {
    let roots: Vec<std::path::PathBuf> = pack_roots().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    let raw = raw.trim();
    let candidate = if let Some(rest) = raw.strip_prefix("@ws/").or_else(|| raw.strip_prefix("@ws\\")) { roots.first().map(|r| r.join(rest)) } else { Some(std::path::PathBuf::from(raw)) };
    let dir = candidate.and_then(|c| std::fs::canonicalize(c).ok()).filter(|d| d.is_dir());
    match dir {
        Some(d) => match roots.iter().find(|r| aqua_native_tools::fs::guard::within(&d, r)) {
            Some(limit) => Ok((d, limit.clone())),
            None => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response()),
        },
        None => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "the folder must be an existing folder inside the folders Aqua works in" }))).into_response()),
    }
}

/// The commands there are for a folder (or for none): the person's own (`data/commands`), those of the folder's project when its `.aqua` text is trusted (and `AQUA_PROJECT_PACK` is on), and the
/// skills the person may start themselves; the person's own win over the project's. Also the folder as a real path, when one was given.
pub(super) fn commands_for(folder: Option<&str>) -> Result<(crate::commands::Commands, Option<std::path::PathBuf>), Response> {
    use crate::commands::{Commands, SkillEntry};
    let data = crate::runtime::data_dir();
    let mut commands = Commands::load(&crate::commands::commands_dir(&data));
    let mut skills = crate::skills::installed();
    let mut dir = None;
    if let Some(raw) = folder.map(str::trim).filter(|f| !f.is_empty()) {
        let (d, limit) = folder_in_roots(raw)?;
        if crate::project_context::enabled() {
            let key = crate::tool_host::pack_trust_key(false);
            let context = crate::project_context::ProjectContext::resolve(&data, key.as_deref(), &d, &limit);
            let files = context.text_files();
            if !files.is_empty() {
                commands = commands.merged(&Commands::from_pack(&files));
                let project_skills = crate::skills::Skills::from_pack(&files);
                skills = Some(std::sync::Arc::new(match &skills {
                    Some(own) => own.merged(&project_skills),
                    None => project_skills,
                }));
            }
        }
        dir = Some(d);
    }
    // The skills that ship with Aqua (card OA12-01), when they are on: the person's own and the project's skills of the same name win.
    if crate::skills::shipped::enabled() {
        let shipped = crate::skills::shipped::all();
        skills = Some(std::sync::Arc::new(match &skills {
            Some(set) => set.merged_shipped(&shipped),
            None => crate::skills::Skills::none().merged_shipped(&shipped),
        }));
    }
    if let Some(skills) = &skills {
        let entries: Vec<SkillEntry<'_>> = skills
            .names()
            .into_iter()
            .filter_map(|n| {
                let (s, m) = (skills.get(n)?, skills.meta_of(n)?);
                Some(SkillEntry { name: &s.name, description: &s.description, argument_hint: m.argument_hint.as_deref(), allowed_tools: &s.allowed_tools, body: &s.body, user_invocable: m.user_invocable, from_project: m.source == crate::skills::SkillSource::Pack })
            })
            .collect();
        commands = commands.with_skills(&entries);
    }
    Ok((commands, dir))
}

/// `GET /api/v1/commands?folder=`: the commands that can be started, with what each says of itself (never its text): name, description, the hint for its words, where it came from, the tools it
/// allows and the packs it names. Either token. Off unless `AQUA_COMMANDS=on` (then it says so and lists none).
async fn commands_list(Query(q): Query<CommandsQuery>) -> Response {
    if !crate::flags::is_on(&crate::flags::COMMANDS) {
        return Json(json!({ "enabled": false, "commands": [], "note": "custom commands are off: set AQUA_COMMANDS=on and restart Aqua to use them" })).into_response();
    }
    match commands_for(q.folder.as_deref()) {
        Ok((commands, _)) => Json(json!({
            "enabled": true,
            "commands": commands.list().iter().map(|c| c.view()).collect::<Vec<_>>(),
            "problems": commands.problems,
            "warnings": commands.warnings,
        }))
        .into_response(),
        Err(r) => r,
    }
}

// ---- agent definitions (card OA11-04, tasks AG-05 and EX-12) ---------------------------------------------------------------------------

/// The agents there are for a folder (or for none): the built-in ones, the person's own (`data/agents`), and those of the folder's project when its `.aqua` text is trusted (and
/// `AQUA_PROJECT_PACK` is on); the built-in names are reserved and the person's own win over the project's. Also the folder as a real path, when one was given.
pub(super) fn agents_for(folder: Option<&str>) -> Result<(crate::agents::Agents, Option<std::path::PathBuf>), Response> {
    let data = crate::runtime::data_dir();
    let mut agents = crate::agents::Agents::builtin().merged(&crate::agents::Agents::load(&crate::agents::agents_dir(&data)));
    let mut dir = None;
    if let Some(raw) = folder.map(str::trim).filter(|f| !f.is_empty()) {
        let (d, limit) = folder_in_roots(raw)?;
        if crate::project_context::enabled() {
            let key = crate::tool_host::pack_trust_key(false);
            let files = crate::project_context::ProjectContext::resolve(&data, key.as_deref(), &d, &limit).text_files();
            if !files.is_empty() {
                agents = agents.merged(&crate::agents::Agents::from_pack(&files));
            }
        }
        dir = Some(d);
    }
    Ok((agents, dir))
}

/// `GET /api/v1/agents?folder=`: the agents that a run can be asked to be (`agent` in a request) or start as a helper (`agent.task`), with what each says of itself, never the words of its instructions:
/// name, description, mode, where it came from, the tools it allows, its packs and its limits. Either token. Off unless `AQUA_AGENTS=on` (then it says so and lists none).
async fn agents_list(Query(q): Query<CommandsQuery>) -> Response {
    if !crate::agents::enabled() {
        return Json(json!({ "enabled": false, "agents": [], "note": "agents are off: set AQUA_AGENTS=on and restart Aqua to use them" })).into_response();
    }
    match agents_for(q.folder.as_deref()) {
        Ok((agents, _)) => Json(json!({
            "enabled": true,
            "agents": agents.list().iter().map(|a| a.view()).collect::<Vec<_>>(),
            "problems": agents.problems,
            "warnings": agents.warnings,
        }))
        .into_response(),
        Err(r) => r,
    }
}

// ---- what the person asked Aqua to remember: the app's view of it (UI token: it is theirs, not the API's) ------------------------------

fn memory_store() -> Result<std::sync::Arc<crate::memory::user_facts::FactStore>, Response> {
    crate::memory::user_facts::store().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "the memory could not be opened" }))).into_response())
}

/// Everything remembered (private facts included: it is the person's own list), and whether Aqua is incognito.
async fn memory_list(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match memory_store() {
        Ok(s) => Json(s.export()).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct AddFact {
    text: String,
}

async fn memory_add(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<AddFact>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let s = match memory_store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    match s.remember(&body.text, "ui", now) {
        Ok(f) => Json(json!({ "fact": f })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn memory_delete(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match memory_store() {
        Ok(s) if s.forget(id) => Json(json!({ "deleted": id })).into_response(),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no fact {id}") }))).into_response(),
        Err(r) => r,
    }
}

/// Deletes everything remembered. The only door to it: Aqua will not do it because a sentence said so.
async fn memory_clear(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match memory_store() {
        Ok(s) => Json(json!({ "deleted": s.clear() })).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct ProposalsQuery {
    status: Option<String>,
}

/// What the memory suggests (the same thing said twice, a date that says tomorrow) and the corrections that wait to be remembered: for the person to look at. Nothing here is a fact
/// until it is accepted. `status` is `pending` (the default), `accepted`, `rejected` or `all`. The UI token: the suggestions hold the words of what was remembered.
async fn memory_proposals(axum::Extension(scope): axum::Extension<Scope>, Query(q): Query<ProposalsQuery>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let s = match memory_store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let status = match q.status.as_deref().unwrap_or("pending") {
        "all" => None,
        "pending" => Some("pending"),
        "accepted" => Some("accepted"),
        "rejected" => Some("rejected"),
        other => return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("status is pending, accepted, rejected or all, not {other}") }))).into_response(),
    };
    Json(json!({ "proposals": s.proposals(status, 100), "pending": s.proposals(Some("pending"), 100).len() })).into_response()
}

/// The person's answer to one suggestion, in the app: it is their own write (the fact it makes is from the app). It checks the UI token itself, and so does each of the two
/// handlers below in its own body: the OpenAPI generator reads the token a route needs from the handler's body, and through this helper alone it listed both routes as API-token
/// routes (card R6-22).
fn decide_memory_proposal(scope: Scope, id: i64, accept: bool) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let s = match memory_store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    match s.decide_proposal(id, accept, "ui", now) {
        Ok(p) => Json(json!({ "proposal": p })).into_response(),
        Err(e) if e.starts_with("there is no suggestion") => (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
        Err(e) if e.contains("was already") => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": e }))).into_response(),
    }
}

/// Yes to a suggestion: a merge forgets the fact that another already says, a date change writes the real date in the fact, a lesson is remembered. The UI token.
async fn memory_proposal_accept(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    decide_memory_proposal(scope, id, true)
}

/// No to a suggestion: it changes nothing and is not made again. The UI token.
async fn memory_proposal_reject(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    decide_memory_proposal(scope, id, false)
}

/// Looks through what was remembered now (the nightly flow `memory_dream` does the same through the tool `memory.consolidate`) and puts what it finds in the queue. It changes no
/// fact. The UI token.
async fn memory_dream(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let s = match memory_store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let report = crate::memory::user_facts::dream::consolidate(&s, now);
    Json(json!({ "summary": report.summary(), "report": report })).into_response()
}

async fn memory_export(axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match memory_store() {
        Ok(s) => (StatusCode::OK, [(axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"aqua_memory.json\"")], Json(s.export())).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct Incognito {
    on: bool,
}

async fn memory_incognito(axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<Incognito>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match memory_store() {
        Ok(s) => match s.set_incognito(body.on) {
            Ok(()) => Json(json!({ "incognito": s.incognito() })).into_response(),
            // Not saved: the answer says so (and what incognito is now), instead of an "ok" that is not true.
            Err(why) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": why, "incognito": s.incognito() }))).into_response(),
        },
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn get(rt: &tokio::runtime::Runtime, scope: Scope, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = routes::<()>().layer(axum::Extension(scope));
        let req = Request::builder().method(Method::GET).uri(uri).header(header::ACCEPT, "application/json").body(Body::empty()).unwrap();
        rt.block_on(async {
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
        })
    }

    #[test]
    fn the_config_the_provider_and_the_flag_routes_answer_the_ui_token_and_refuse_the_api_token() {
        let dir = std::env::temp_dir().join(format!("aqua_route_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools").join("fs.json"), "{\n  // my folders\n  \"roots\": [] \"limits\": {}\n}").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        crate::runtime::data_tripwire::with_data_dir(&dir, || {
            for uri in ["/api/v1/config/status", "/api/v1/flags", "/api/v1/tools/providers"] {
                assert_eq!(get(&rt, Scope::Api, uri).0, StatusCode::FORBIDDEN, "{uri}: the API token is not the app's");
            }
            let (status, body) = get(&rt, Scope::Ui, "/api/v1/config/status");
            assert_eq!(status, StatusCode::OK);
            let files = body["files"].as_array().expect("files");
            assert_eq!(files.len(), 17);
            let fs = files.iter().find(|f| f["name"] == "fs.json").unwrap();
            assert_eq!((fs["state"].as_str(), fs["error"]["kind"].as_str(), fs["error"]["line"].as_u64(), fs["error"]["column"].as_u64()), (Some("broken"), Some("syntax"), Some(3), Some(15)), "{fs}");
            assert_eq!(fs["effect"], "fs.* tools are off until it is fixed");
            assert!(!body.to_string().contains("my folders"), "a comment of the file is not in the answer");
            assert_eq!(body["broken"], 1);

            let (status, body) = crate::flags::with_overrides(&[("AQUA_PLAN", Some("on")), ("AQUA_ROUTER_V2", None)], || get(&rt, Scope::Ui, "/api/v1/flags"));
            assert_eq!(status, StatusCode::OK);
            let flags = body["flags"].as_array().expect("flags");
            let flag = |name: &str| flags.iter().find(|f| f["name"] == name).unwrap_or_else(|| panic!("{name} is not listed"));
            assert_eq!((flag("AQUA_PLAN")["value"].as_str(), flag("AQUA_PLAN")["set"].as_bool(), flag("AQUA_PLAN")["promotable"].as_bool()), (Some("on"), Some(true), Some(true)));
            assert_eq!((flag("AQUA_ROUTER_V2")["value"].as_str(), flag("AQUA_ROUTER_V2")["default"].as_str()), (Some("on"), Some("on")));
            assert_eq!(flag("AQUA_CLIPBOARD_TOOLS")["reach"], true);

            let (status, body) = get(&rt, Scope::Ui, "/api/v1/tools/providers");
            assert_eq!(status, StatusCode::OK);
            assert!(body["providers"].is_array() && body["off"].is_number(), "{body}");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
