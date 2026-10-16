//! HTTP API for scheduled flows and their notifications: `/api/v1/flows/schedules/*`,
//! `/api/v1/flows/:id/schedules`, `/api/v1/flows/notifications/*`.
//!
//! Reading needs the API token. Creating, changing, running now and deciding a missed run need the UI
//! token: a schedule makes a flow run by itself, so a script holding only the API token must not plant one.

use super::auth::{ui_scope_required, Scope};
use super::flow_api::store as flow_store;
use crate::flows::model::current_owner;
use crate::flows::notify::{Notifier, NotifyStore};
use crate::flows::sched::model::{Misfire, NotifyPrefs, ScheduleInput, When};
use crate::flows::sched::store::SchedStore;
use crate::flows::schedule_text;
use crate::flows::zone::Zone;
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;

// The stores are the fields of `crate::flows::services::FlowServices` (card OA13-03, task AQ-12); the functions below are one call each to the stores of the process.

/// The process-wide schedule store (the flows database file).
pub fn sched_store() -> Arc<SchedStore> {
    crate::flows::services::global().schedules()
}

pub fn notify_store() -> Arc<NotifyStore> {
    crate::flows::services::global().notifications()
}

pub fn notifier() -> Arc<Notifier> {
    // The `voice` channel puts a due reminder in the queue the app's player empties (`announce_api.rs`).
    crate::flows::services::global().notifier()
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/flows/schedules", get(list_all))
        .route("/api/v1/flows/schedules/preview", post(preview))
        .route("/api/v1/flows/schedules/missed/:oid/:decision", post(resolve_missed))
        .route("/api/v1/flows/schedules/:sid", get(get_one).put(update))
        .route("/api/v1/flows/schedules/:sid/delete", post(delete))
        .route("/api/v1/flows/schedules/:sid/:action", post(action))
        .route("/api/v1/flows/schedules/:sid/occurrences", get(occurrences))
        .route("/api/v1/flows/schedules/:sid/campaign", get(campaign_view))
        .route("/api/v1/flows/schedules/:sid/why", post(why))
        .route("/api/v1/flows/:id/schedules", get(list_for_flow).post(create))
        .route("/api/v1/flows/notifications", get(notifications))
        .route("/api/v1/flows/notifications/read", post(mark_read))
        .route("/api/v1/flows/notifications/channels", get(channels))
        .route("/api/v1/flows/recovery/knowledge", get(knowledge))
        .route("/api/v1/flows/recovery/knowledge/reset", post(knowledge_reset))
        .route("/api/v1/flows/health", get(health_all))
        .route("/api/v1/flows/assistant", post(ask))
        .route("/api/v1/flows/:id/health", get(health_one))
        .route("/api/v1/flows/recovery/:id/rollback", post(rollback))
        .route("/api/v1/flows/runs/:rid/recovery", get(run_recovery))
        .route("/api/v1/flows/runs/:rid/recovery/apply", post(apply_recovery))
        .route("/api/v1/flows/:id/recoveries", get(flow_recoveries))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// A store fault is ours; anything else is something the person wrote or asked for.
fn fail(e: String) -> Response {
    if e.starts_with("flow store:") || e.starts_with("schedule store:") || e.starts_with("notify store:") {
        tracing::warn!("flow schedule api: {e}");
        error(StatusCode::INTERNAL_SERVER_ERROR, "the schedule store could not complete that request")
    } else if e.starts_with("no such") {
        error(StatusCode::NOT_FOUND, e)
    } else {
        error(StatusCode::BAD_REQUEST, e)
    }
}

fn scheduler_or_503() -> Result<Arc<crate::flows::sched::tick::Scheduler>, Response> {
    crate::flows::sched::scheduler().ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "the scheduler is not running"))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

async fn list_all() -> Response {
    let s = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || s.views(current_owner(), None)).await {
        Ok(Ok(v)) => Json(json!({ "schedules": v })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

async fn list_for_flow(Path(id): Path<i64>) -> Response {
    let s = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || s.views(current_owner(), Some(id))).await {
        Ok(Ok(v)) => Json(json!({ "schedules": v })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

async fn get_one(Path(sid): Path<i64>) -> Response {
    match sched_store().get(current_owner(), sid) {
        Ok(Some(s)) => Json(json!({ "schedule": s })).into_response(),
        Ok(None) => fail("no such schedule".into()),
        Err(e) => fail(e),
    }
}

/// What a person sends: either the exact form, or a sentence ("every weekday at 9 AM").
#[derive(Deserialize)]
struct CreateBody {
    #[serde(default)]
    name: String,
    /// "every day at 10 AM", "tomorrow at 3 pm". Used when `when` is not given.
    #[serde(default)]
    text: String,
    #[serde(default)]
    when: Option<When>,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    inputs: Map<String, Value>,
    #[serde(default)]
    misfire: Misfire,
    #[serde(default)]
    notify: NotifyPrefs,
    #[serde(default = "crate::flows::sched::model::yes")]
    enabled: bool,
    /// Make it a campaign: a last day, a number of runs, inputs that take turns.
    #[serde(default)]
    campaign: crate::flows::sched::model::Campaign,
}

/// Turn the body into the exact form. The sentence is read in the schedule's own time zone.
fn to_input(mut b: CreateBody, default_name: &str, at: i64) -> Result<(ScheduleInput, Option<String>), String> {
    let (when, summary) = match b.when {
        Some(w) => (w, None),
        None => {
            if b.text.trim().is_empty() {
                return Err("say when: give `when` or a sentence in `text`".into());
            }
            let zone = Zone::parse(&b.timezone)?;
            let naive = zone.naive_at(at).ok_or("could not work out the time now in that zone")?;
            let spec = schedule_text::parse(&b.text, naive).ok_or_else(|| format!("I could not read a time from \"{}\". Try \"every day at 10:00\", \"every Monday at 9 AM\", or \"tomorrow at 15:30\".", b.text.trim()))?;
            let when = match (spec.cron, spec.when) {
                (Some(cron), _) => When::Cron { cron },
                (None, Some(when)) => When::Once { when },
                _ => return Err("I could not read a time from that".into()),
            };
            // "for 4 weeks" and "8 times" in the sentence set the campaign's limits unless they were given outright.
            let lim = schedule_text::limits(&b.text, naive);
            if matches!(when, When::Cron { .. }) {
                if b.campaign.end_on.is_none() {
                    b.campaign.end_on = lim.end_on.clone();
                }
                if b.campaign.max_runs.is_none() {
                    b.campaign.max_runs = lim.max_runs;
                }
            }
            let summary = if lim.summary.is_empty() || !matches!(when, When::Cron { .. }) { spec.summary } else { format!("{}, {}", spec.summary, lim.summary) };
            (when, Some(summary))
        }
    };
    let name = if b.name.trim().is_empty() { default_name.to_string() } else { b.name };
    Ok((ScheduleInput { name, when, timezone: b.timezone, inputs: b.inputs, misfire: b.misfire, notify: b.notify, enabled: b.enabled, campaign: b.campaign }, summary))
}

async fn create(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>, Json(body): Json<CreateBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let flow = flow_store().get(current_owner(), id)?.ok_or("no such flow")?;
        let at = now();
        let (input, summary) = to_input(body, &format!("{} schedule", flow.def.name), at)?;
        input.validate(at)?;
        let s = sched_store().create(current_owner(), id, &input, at)?;
        Ok(json!({ "schedule": s, "summary": summary }))
    })
    .await;
    match out {
        Ok(Ok(v)) => (StatusCode::CREATED, Json(v)).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

async fn update(axum::Extension(scope): axum::Extension<Scope>, Path(sid): Path<i64>, Json(body): Json<CreateBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let old = sched_store().get(current_owner(), sid)?.ok_or("no such schedule")?;
        let at = now();
        let (input, summary) = to_input(body, &old.name, at)?;
        input.validate(at)?;
        let s = sched_store().update(current_owner(), sid, &input, at)?.ok_or("no such schedule")?;
        Ok(json!({ "schedule": s, "summary": summary }))
    })
    .await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

async fn delete(axum::Extension(scope): axum::Extension<Scope>, Path(sid): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match sched_store().delete(current_owner(), sid) {
        Ok(true) => Json(json!({ "deleted": true })).into_response(),
        Ok(false) => fail("no such schedule".into()),
        Err(e) => fail(e),
    }
}

/// `enable`, `disable` or `run-now`.
async fn action(axum::Extension(scope): axum::Extension<Scope>, Path((sid, action)): Path<(i64, String)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let sched = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let owner = current_owner();
        let at = now();
        match action.as_str() {
            "enable" | "disable" => {
                if action == "enable" {
                    // Enabling checks the schedule still makes sense (a passed one-time date, a bad zone).
                    let s = sched.store.get(owner, sid)?.ok_or("no such schedule")?;
                    let input = ScheduleInput { name: s.name, when: s.when, timezone: s.timezone, inputs: s.inputs, misfire: s.misfire, notify: s.notify, enabled: true, campaign: s.campaign };
                    input.validate(at)?;
                }
                let s = sched.store.set_enabled(owner, sid, action == "enable", at)?.ok_or("no such schedule")?;
                Ok(json!({ "schedule": s }))
            }
            "run-now" => {
                let run = sched.run_now(owner, sid, at)?;
                Ok(json!({ "run": crate::flows::run::model::Run { def: Default::default(), ..run } }))
            }
            other => Err(format!("no such action \"{other}\" (enable, disable, run-now)")),
        }
    })
    .await;
    match out {
        Ok(Ok(v)) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}

async fn occurrences(Path(sid): Path<i64>, Query(q): Query<LimitQuery>) -> Response {
    match sched_store().occurrences(current_owner(), sid, q.limit.unwrap_or(50).min(200)) {
        Ok(v) => Json(json!({ "occurrences": v })).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct WhyBody {
    /// The moment the person expected it to run (unix seconds).
    expected: i64,
}

/// "Why didn't my 10 AM workflow run?"
async fn why(Path(sid): Path<i64>, Json(b): Json<WhyBody>) -> Response {
    let sched = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || sched.why_not_run(current_owner(), sid, b.expected)).await {
        Ok(Ok(text)) => Json(json!({ "answer": text })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

async fn resolve_missed(axum::Extension(scope): axum::Extension<Scope>, Path((oid, decision)): Path<(i64, String)>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let sched = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || sched.resolve_missed(current_owner(), oid, &decision, now())).await {
        Ok(Ok(run)) => Json(json!({ "run": run.map(|r| crate::flows::run::model::Run { def: Default::default(), ..r }) })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}

/// Show the next few times a schedule would run, before saving it.
async fn preview(Json(body): Json<CreateBody>) -> Response {
    let at = now();
    let zone_text = body.timezone.clone();
    let (input, summary) = match to_input(body, "preview", at) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let (zone, first) = match input.validate(at) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let mut times = Vec::new();
    let mut next = first;
    while let Some(t) = next {
        times.push(t);
        if times.len() >= 5 {
            break;
        }
        next = match &input.when {
            When::Cron { cron } => crate::workflows::schedule::CronExpr::parse(cron).ok().and_then(|c| zone.next_cron(&c, t)),
            When::Once { .. } => None,
        };
    }
    let shown: Vec<Value> = times.iter().map(|t| json!({ "at": t, "text": zone.show(*t) })).collect();
    let _ = zone_text;
    Json(json!({ "summary": summary, "zone": zone.label(), "next": shown })).into_response()
}

#[derive(Deserialize)]
struct NotificationsQuery {
    #[serde(default)]
    unread: bool,
    limit: Option<u32>,
}

async fn notifications(Query(q): Query<NotificationsQuery>) -> Response {
    let store = notify_store();
    let owner = current_owner();
    match (store.list(owner, q.unread, q.limit.unwrap_or(50).min(200)), store.unread_count(owner)) {
        (Ok(list), Ok(unread)) => Json(json!({ "notifications": list, "unread": unread })).into_response(),
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

#[derive(Deserialize)]
struct ReadBody {
    ids: Vec<i64>,
}

async fn mark_read(axum::Extension(scope): axum::Extension<Scope>, Json(b): Json<ReadBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match notify_store().mark_read(current_owner(), &b.ids) {
        Ok(n) => Json(json!({ "marked": n })).into_response(),
        Err(e) => fail(e),
    }
}

async fn channels() -> Response {
    Json(json!({ "channels": notifier().channel_status() })).into_response()
}

// ---- recovery --------------------------------------------------------------------------------------------------------------

/// For each step that failed in a run: what kind of problem it was, and what could be done about it.
async fn run_recovery(Path(rid): Path<i64>) -> Response {
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let runs = super::flow_api::run_store();
        let run = runs.get_run(current_owner(), rid)?.ok_or("no such run")?;
        let nodes = runs.node_runs(rid)?;
        let mut steps = Vec::new();
        for n in nodes.iter().filter(|n| matches!(n.status, crate::flows::run::model::NodeStatus::Failed | crate::flows::run::model::NodeStatus::NeedsAction | crate::flows::run::model::NodeStatus::Interrupted)) {
            let Some(node) = run.def.node(&n.node_id) else { continue };
            let class = crate::flows::supervisor::classify(&n.error);
            steps.push(json!({
                "node_id": n.node_id, "node_name": node.name, "error": n.error,
                "class": class.key(), "label": class.label(), "explain": class.explain(),
                "guidance": crate::flows::supervisor::recovery::guidance(class),
                "suggestions": crate::flows::supervisor::recovery::suggest_with_error(&runs, class, node, &n.error),
            }));
        }
        let verdict = crate::flows::supervisor::judge(&run, &nodes, &run.def);
        Ok(json!({ "verdict": verdict, "steps": steps }))
    })
    .await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

#[derive(Deserialize)]
struct ApplyBody {
    node: String,
    key: String,
}

/// Apply a permanent fix the person approved. Needs the UI token.
async fn apply_recovery(axum::Extension(scope): axum::Extension<Scope>, Path(rid): Path<i64>, Json(b): Json<ApplyBody>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let runs = super::flow_api::run_store();
        let run = runs.get_run(current_owner(), rid)?.ok_or("no such run")?;
        let (flow, rec) = crate::flows::supervisor::recovery::apply(&flow_store(), &runs, current_owner(), &run, &b.node, &b.key, now())?;
        Ok(json!({ "version": flow.version, "recovery": rec }))
    })
    .await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

async fn rollback(axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let out = tokio::task::spawn_blocking(move || crate::flows::supervisor::recovery::rollback(&flow_store(), &super::flow_api::run_store(), current_owner(), id, now())).await;
    match out {
        Ok(Ok(flow)) => Json(json!({ "version": flow.version })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

async fn flow_recoveries(Path(id): Path<i64>) -> Response {
    match super::flow_api::run_store().recoveries(current_owner(), id, 50) {
        Ok(v) => Json(json!({ "recoveries": v })).into_response(),
        Err(e) => fail(e),
    }
}

/// What has been learned: for each fix and failure kind, how often it helped and whether it is trusted.
async fn knowledge() -> Response {
    match super::flow_api::run_store().kb_all() {
        Ok(v) => Json(json!({ "knowledge": v })).into_response(),
        Err(e) => fail(e),
    }
}

// ---- health, errors and asking -----------------------------------------------------------------------------------------------

/// Every flow's health and last two weeks, and the failures grouped by kind. All computed from the run history.
async fn health_all() -> Response {
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        use crate::flows::supervisor::health::*;
        let (runs, owner, at) = (super::flow_api::run_store(), current_owner(), now());
        let mut flows = Vec::new();
        let mut all = Vec::new();
        for f in flow_store().list(owner)? {
            let judged = judged_runs(&runs, owner, f.id, WINDOW)?;
            let h = health_of(&f.name, f.id, &judged);
            flows.push(json!({ "health": h, "history": history(&judged, at, 14) }));
            all.push((f.name, judged));
        }
        Ok(json!({ "flows": flows, "errors": error_groups(&all, at - 14 * 86_400) }))
    })
    .await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

async fn health_one(Path(id): Path<i64>) -> Response {
    let out = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        use crate::flows::supervisor::health::*;
        let (runs, owner) = (super::flow_api::run_store(), current_owner());
        let f = flow_store().get(owner, id)?.ok_or("no such flow")?;
        let judged = judged_runs(&runs, owner, id, WINDOW)?;
        Ok(json!({ "health": health_of(&f.def.name, id, &judged), "history": history(&judged, now(), 14) }))
    })
    .await;
    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

#[derive(Deserialize)]
struct AskBody {
    text: String,
}

/// "Did my scheduled workflow run?", "Fix the last error", "Check my workflows and fix what you can".
/// Reading needs the API token; it changes nothing that alters a flow (that is `recovery/apply`, with the UI token).
async fn ask(Json(b): Json<AskBody>) -> Response {
    let out = tokio::task::spawn_blocking(move || {
        let flows = flow_store();
        let runs = super::flow_api::run_store();
        let sched = sched_store();
        let scheduler = crate::flows::sched::scheduler();
        let ctx = crate::flows::supervisor::assistant::Ctx { flows: &flows, runs: &runs, sched: &sched, scheduler: scheduler.as_deref(), owner: current_owner(), now: now() };
        crate::flows::supervisor::assistant::answer(&ctx, &b.text)
    })
    .await;
    match out {
        Ok(Some(r)) => Json(json!({ "understood": true, "reply": r.text, "proposals": r.proposals, "did": r.did })).into_response(),
        Ok(None) => Json(json!({ "understood": false, "reply": "I can answer questions about your workflows: whether they ran, how healthy they are, what failed, and I can fix the last error or check everything. Try \"Did my scheduled workflow run?\", \"Fix the last error\" or \"Check my workflows and fix any problems you can\".", "proposals": [], "did": [] })).into_response(),
        Err(_) => fail("flow store: the request was interrupted".into()),
    }
}

#[derive(Deserialize, Default)]
struct ResetBody {
    /// One fix (`carry_on`), or omitted for everything that was learned.
    #[serde(default)]
    fix_key: Option<String>,
}

/// Forget what was learned about a fix. It starts again as a candidate and is not used unasked until it earns trust
/// again. Needs the UI token.
async fn knowledge_reset(axum::Extension(scope): axum::Extension<Scope>, body: Option<Json<ResetBody>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let key = body.and_then(|Json(b)| b.fix_key);
    match super::flow_api::run_store().kb_reset(key.as_deref()) {
        Ok(n) => Json(json!({ "removed": n })).into_response(),
        Err(e) => fail(e),
    }
}

/// How a campaign is going: runs started, how they ended, how many are still to come.
async fn campaign_view(Path(sid): Path<i64>) -> Response {
    let sched = match scheduler_or_503() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || sched.campaign(current_owner(), sid, now())).await {
        Ok(Ok(v)) => Json(json!({ "campaign": v })).into_response(),
        Ok(Err(e)) => fail(e),
        Err(_) => fail("schedule store: the request was interrupted".into()),
    }
}
