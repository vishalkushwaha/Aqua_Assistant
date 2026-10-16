//! Answering a run's question, and starting a run that is not tied to a connection (card OA10-03; tasks AG-03 and UX-03 of the OpenCode audit).
//!
//! * `POST /api/v1/runs/{id}/answer` goes on with a run that ended with a question of the planner (`waiting_user`): the answer is `{"text": "..."}` (at most 500 characters) or
//!   `{"choice": n}` (one of the answers the question offered). **The UI token only.** An answer is text and never an approval: the host's own questions are answered at
//!   `POST /api/v1/browser/confirm`, with the same token, and nothing here reaches them. The run that goes on is a new run (`parent_run_id` is the one that asked) as the caller that
//!   asked (`agent_runs::answer`), it takes its turn on the model like any other, and what it comes to is the answer, with the question it asks next when it asks again.

//!
//! * `POST /api/v1/runs` starts a run that is not tied to the connection that asked for it (`run_host`): it answers at once (202) with the id the run will be recorded under, the run
//!   takes its turn on the model like any run the person waits for, and its questions are put to the person as they always were. The UI token or the API token; the API token's runs
//!   are its own to read, steer and end (`run_api`), and none of them can be approved with it. With `"agent": "plan"` the run is a **plan run** (card OA10-04, AG-06): it reads, changes nothing
//!   (the tool host refuses every call that is not a read, and the loop refuses the ones it knows change something), and ends `plan_ready` with its answer as the plan; with `"session_id"` it
//!   is put in the session of that name (card OA10-04, UX-10).
//!
//! * `POST /api/v1/runs/{id}/continue` starts a run that goes on from one that used its rounds or its time (`agent_runs::continuation`); `POST /api/v1/runs/{id}/plan/approve` starts the build from
//!   the plan a plan run wrote, and `POST /api/v1/runs/{id}/plan/revise {text}` asks for another plan. **The UI token only**: a plan is approved by the person, never by a script and never by voice.

use super::auth::{ui_scope_required, Scope};
use super::run_host::{self, Drive, StartError, Work};
use super::server::{agent_request_work, api_error, for_client, pipeline_failure, run_turn, take_turn, AgentWorkRequest, AppState};
use crate::agent_runs::answer::{self, Given, Refused};
use crate::agent_runs::{self, continuation, resume, AgentRunStore};
use crate::modules::browser_agent::{McpAgentResult, Resume, RunMode};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/runs", post(run_start_endpoint).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)))
        .route("/api/v1/runs/:run_id/answer", post(run_answer_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/runs/:run_id/continue", post(run_continue_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/runs/:run_id/plan/approve", post(plan_approve_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
        .route("/api/v1/runs/:run_id/plan/revise", post(plan_revise_endpoint).layer(axum::extract::DefaultBodyLimit::max(4 * 1024)))
}

/// The longest goal of a run: what the record keeps of a goal to go on from it.
const MAX_GOAL_CHARS: usize = agent_runs::store::GOAL_RESUME_CHARS;

/// The body of `POST /api/v1/runs`.
#[derive(Deserialize, Default, Debug)]
pub struct StartRequest {
    /// What to do, in the person's words.
    pub goal: String,
    /// The most planning rounds (10 by default; a coding run may have up to 24, any other up to 12).
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Which of the two outside tools the run may use: `{"browser": true, "desktop": true}` by default.
    #[serde(default)]
    pub tools: Option<ToolsRequest>,
    /// A task, not a conversation: a request the gate would answer in chat goes to the agent (true by default; a refusal stays a refusal).
    #[serde(default)]
    pub force_agent: Option<bool>,
    /// What kind of task this is, when the caller knows: `coding`, `files`, `web` or `desktop` (card OA4-01).
    #[serde(default)]
    pub profile: Option<String>,
    /// A key for this start: the same key with the same request starts nothing new (the answer is the run started the first time, with `duplicate: true`), so that a retry after a
    /// time-out cannot start the work twice. Remembered for a day, in memory.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// How long a question to the person about one of the run's calls may wait for an answer, in seconds (10 to 600; 60 by default). After it the call is refused, as a "no" is.
    #[serde(default)]
    pub approval_wait_secs: Option<u64>,
    /// The folder the run works in. It has to be a folder Aqua may already use: one that is not is refused (the person allows a folder in the app, `fs.allow_folder`), never added here.
    #[serde(default)]
    pub directory: Option<String>,
    /// `build` (the default) or `plan`: a plan run reads and writes the plan and changes nothing; the person approves the plan at `POST /api/v1/runs/{id}/plan/approve` before it is built (card OA10-04).
    #[serde(default)]
    pub agent: Option<String>,
    /// The session the run is put in: a name of 1 to 64 letters, digits and `_ . : -`. A run that goes on from another is in that run's session. With `AQUA_CODING_SESSIONS=on` a coding run of
    /// the session is told what the earlier runs of it came to (card OA10-04, UX-10 and AG-16).
    #[serde(default)]
    pub session_id: Option<String>,
    /// A pre-approval the person made in the app (`POST /api/v1/preapprovals`, card OA10-05): the run's project checks in `directory` then run without a question, as far as it says. It
    /// is given to this run only (a second start with the same id is refused), needs `directory` inside the folder it names, and needs the core to run with `AQUA_PREAPPROVALS=on`.
    /// Naming one is no way to make one: it adds nothing the person did not allow.
    #[serde(default)]
    pub preapproval_id: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
pub struct ToolsRequest {
    #[serde(default)]
    pub browser: Option<bool>,
    #[serde(default)]
    pub desktop: Option<bool>,
}

fn bad(message: &str) -> Response {
    api_error(StatusCode::BAD_REQUEST, message)
}

/// Is `directory` a folder Aqua may already use? The file guard decides, as it does for every file call; nothing is added to it here.
fn allowed_folder(directory: &str) -> Result<(), Response> {
    let Some(guard) = super::preapproval_api::the_guard() else {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "the file tools are not running, so no folder can be named"));
    };
    match guard.resolve(directory, aqua_native_tools::fs::Op::Read) {
        Ok(_) => Ok(()),
        Err(_) => Err(api_error(StatusCode::FORBIDDEN, "that folder is not one Aqua may use: allow it in the app first (nothing is added to the allowed folders by this request)")),
    }
}

/// Starts a run that is not tied to this connection and answers at once (202) with the id it will be recorded under, its place in the line and where to read it
/// (`GET /api/v1/runs/{id}/events`, `/stream`, `/wait`). The run waits for the model's turn behind the runs in front of it (the person's own requests queue behind it, as behind any run they
/// wait for), goes on if this client hangs up, may ask the person (a question is answered at `POST /api/v1/browser/confirm` with the UI token, never with the API token) and is ended at
/// `POST /api/v1/runs/{id}/interrupt`. With an `idempotency_key` a retry returns the run that was started the first time (200, `duplicate: true`). At most eight are queued or going.
async fn run_start_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<StartRequest>) -> Response {
    start_run(scope, body, move |plan| {
        let (requested, max_steps) = (plan.requested, plan.max_steps);
        let (use_browser, use_windows, force_agent) = (plan.use_browser, plan.use_windows, plan.force_agent);
        let (session, plan_run, agent) = (plan.session_id.clone(), plan.plan_run, plan.agent.clone());
        Arc::new(move |next_goal: &str| -> Work {
            let steps = max_steps.clamp(1, crate::modules::browser_agent::max_rounds_for_request(next_goal, requested));
            let options = if plan_run { plan_options() } else { Default::default() };
            let work = agent_request_work(&state, AgentWorkRequest { goal: next_goal.to_string(), max_steps: steps, use_browser, use_windows, force_agent, requested, options });
            // The agent the run is (card OA11-04): the loop reads it where the run begins.
            let agent = agent.clone();
            framed(move || crate::agents::with_requested(agent, work), session.clone(), plan_run, None)
        })
    })
}

/// What a run that goes on from another is started with (a continuation, an approved plan, a plan to revise).
pub(crate) struct GoingOn {
    pub resume: Resume,
    /// What the planner is told, as data after its rules.
    pub block: String,
    /// `voice`, `api` or `agent`: the run has the powers the first had.
    pub caller: String,
}

/// The tools a plan run may call: reads and searches. The tool host refuses every other call of the run (`allowed_tools`, which can only narrow), whatever the model asks for.
const PLAN_TOOLS: &[&str] = &["fs.read", "fs.list", "fs.find", "fs.grep", "fs.stat", "fs.outline", "git.status", "git.diff", "git.log", "git.show", "project.profile"];

fn plan_options() -> crate::agent_runs::scope::RunOptions {
    crate::agent_runs::scope::RunOptions { allowed_tools: PLAN_TOOLS.iter().map(|t| t.to_string()).collect(), forced_packs: vec!["files".into(), "git".into()], tainted: false }
}

/// What a plan run is told, after the rules and before the task: a rule of Aqua's own (the words are the program's, not the task's).
pub(crate) const PLAN_RUN_RULE: &str = "THIS IS A PLANNING RUN (a rule of Aqua's, not part of the task): read what you need with the tools you have, change nothing and run nothing. When you know enough, give as your final answer the plan for the task: at most 5 short steps, one per line, numbered, each naming the file or the check it is about. The person reads the plan and decides whether it is built.";

/// `work` as the run of a session, in the mode it is for, going on from another run when `going` says so. The work is what `agent_request_work` made; this puts the thread-locals around it
/// that the loop reads (the session the run is in, the mode, what it goes on from and the words it is told).
fn framed(work: impl FnOnce() -> anyhow::Result<McpAgentResult> + Send + 'static, session: Option<String>, plan_run: bool, going: Option<GoingOn>) -> Work {
    Box::new(move || {
        agent_runs::with_session(session, || {
            let mode = if plan_run { RunMode::Plan } else { RunMode::Build };
            let run = move || crate::modules::browser_agent::with_run_mode(mode, work);
            match going {
                Some(g) => {
                    let block = if plan_run { format!("{PLAN_RUN_RULE}\n{}", g.block) } else { g.block };
                    let caller = crate::modules::browser_agent::windows_caller_named(&g.caller);
                    crate::modules::browser_agent::with_agent_context(Some(block), || crate::modules::browser_agent::with_agent_resume(Some(g.resume), || crate::modules::browser_agent::with_windows_caller(caller, run)))
                }
                None if plan_run => crate::modules::browser_agent::with_agent_context(Some(PLAN_RUN_RULE.to_string()), run),
                None => run(),
            }
        })
    })
}

/// The `MakeWork` of a run that goes on from another: the first run it makes (the goal as asked) goes on from it, the ones made for lines kept for after (a new request each) do not.
fn going_on_work(state: &AppState, plan: &StartPlan, going: GoingOn) -> run_host::MakeWork {
    let state = state.clone();
    let (requested, max_steps, force_agent) = (plan.requested, plan.max_steps, plan.force_agent);
    let (use_browser, use_windows) = (plan.use_browser, plan.use_windows);
    let (session, plan_run, agent) = (plan.session_id.clone(), plan.plan_run, plan.agent.clone());
    let going = std::sync::Mutex::new(Some(going));
    Arc::new(move |next_goal: &str| -> Work {
        let steps = max_steps.clamp(1, crate::modules::browser_agent::max_rounds_for_request(next_goal, requested));
        let options = if plan_run { plan_options() } else { Default::default() };
        let work = agent_request_work(&state, AgentWorkRequest { goal: next_goal.to_string(), max_steps: steps, use_browser, use_windows, force_agent, requested, options });
        let first = going.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let agent = agent.clone();
        framed(move || crate::agents::with_requested(agent, work), session.clone(), plan_run, first)
    })
}

/// What a start comes to once its body has been checked: what the work of each run of the chain is made from.
pub(crate) struct StartPlan {
    pub max_steps: u32,
    pub use_browser: bool,
    pub use_windows: bool,
    pub force_agent: bool,
    pub requested: Option<crate::modules::run_profile::RunProfile>,
    /// The session the request named (card OA10-04).
    pub session_id: Option<String>,
    /// A plan run (`"agent": "plan"`).
    pub plan_run: bool,
    /// The agent the run is, when the request named one besides `build` and `plan` (card OA11-04, `AQUA_AGENTS=on`).
    pub agent: Option<String>,
}

/// The whole of a start, with what makes the work of a run given (so that a test gives a scripted one): checks the body, takes a place in the line, spawns the task that drives the run
/// and answers at once.
pub(crate) fn start_run(scope: Scope, body: StartRequest, make_work: impl FnOnce(&StartPlan) -> run_host::MakeWork) -> Response {
    let ui = scope == Scope::Ui;
    let goal = body.goal.trim().to_string();
    if goal.is_empty() {
        return bad("say what to do: goal is empty");
    }
    if goal.chars().count() > MAX_GOAL_CHARS {
        return bad(&format!("a goal is at most {MAX_GOAL_CHARS} characters"));
    }
    let requested = match crate::modules::run_profile::parse_requested(body.profile.as_deref()) {
        Ok(p) => p,
        Err(why) => return bad(&why),
    };
    // `build` is the run as it always was; `plan` reads and writes the plan and changes nothing (card OA10-04, AG-06).
    let (plan_run, agent) = match body.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        None | Some("build") => (false, None),
        Some("plan") => (true, None),
        // Another agent (card OA11-04): one of yours or a project's, when agents are on. With them off the answer is the one it always was.
        Some(other) if crate::agents::enabled() => match super::server::requested_agent(Some(other)) {
            Ok(a) => (false, a),
            Err(refused) => return refused,
        },
        Some(_) => return bad("agent is build or plan"),
    };
    // The session the run is put in: a name the client gave. The API token may name a session of its own (or a new one); the UI token any.
    let session_id = match super::sessions_api::named_session(body.session_id.as_deref(), ui) {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    // A plan run is about code, and it has no browser and no desktop: what it may do is read.
    let requested = if plan_run { Some(crate::modules::run_profile::RunProfile::Coding) } else { requested };
    if let Some(secs) = body.approval_wait_secs {
        if !(run_host::MIN_APPROVAL_WAIT..=run_host::MAX_APPROVAL_WAIT).contains(&secs) {
            return bad(&format!("approval_wait_secs is a number of seconds from {} to {}", run_host::MIN_APPROVAL_WAIT, run_host::MAX_APPROVAL_WAIT));
        }
    }
    let goal = match body.directory.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(directory) => {
            if let Err(refused) = allowed_folder(directory) {
                return refused;
            }
            // The run finds the project it works in from the folders its goal names.
            format!("Work in the folder {directory}. {goal}")
        }
        None => goal,
    };
    // A pre-approval of the person's (card OA10-05, WF-14) is looked at before the run takes its place in the line, and given to the run once it has one.
    let mut preapproval: Option<(String, String)> = None;
    if let Some(pid) = body.preapproval_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        if !aqua_tool_host::preapproval::enabled() {
            return api_error(StatusCode::CONFLICT, "pre-approvals are off: start the core with AQUA_PREAPPROVALS=on");
        }
        let Some(directory) = body.directory.as_deref().map(str::trim).filter(|d| !d.is_empty()) else {
            return bad("a pre-approval is for one folder: say which with directory");
        };
        let (folder, _) = match super::preapproval_api::resolve_folder(directory) {
            Ok(f) => f,
            Err(refused) => return refused,
        };
        let Some(host) = crate::agent_tools::host() else {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "the tool host is not running, so a pre-approval cannot be used");
        };
        if let Err(why) = host.preapprovals().check_attachable(pid, &folder) {
            return api_error(StatusCode::UNPROCESSABLE_ENTITY, &why);
        }
        preapproval = Some((pid.to_string(), folder));
    }
    let (use_browser, use_windows) = body.tools.map_or((true, true), |t| (t.browser.unwrap_or(true), t.desktop.unwrap_or(true)));
    let (use_browser, use_windows) = (use_browser && !plan_run, use_windows && !plan_run);
    let force_agent = body.force_agent.unwrap_or(true) || requested.is_some_and(|p| p.works_on_files());
    let max_steps = body.max_steps.unwrap_or(super::server::default_agent_max_steps());
    let origin = if ui { "ui" } else { "api" };
    let fingerprint = format!("{goal}|{max_steps}|{use_browser}|{use_windows}|{force_agent}|{}|{:?}|{plan_run}|{:?}", requested.map_or("", |p| p.as_str()), body.approval_wait_secs, session_id);
    let registered = match run_host::register(origin, &goal, body.idempotency_key.as_deref(), &fingerprint, body.approval_wait_secs) {
        Ok(r) => r,
        Err(why @ StartError::Full) => return api_error(StatusCode::TOO_MANY_REQUESTS, &why.message()),
        Err(why @ StartError::KeyMismatch) => return api_error(StatusCode::UNPROCESSABLE_ENTITY, &why.message()),
        Err(why @ StartError::BadKey) => return bad(&why.message()),
    };
    let (id, position, duplicate) = (registered.id.clone(), registered.position, registered.duplicate);
    if !duplicate {
        if let (Some((pid, folder)), Some(host)) = (preapproval, crate::agent_tools::host()) {
            // The run has its id now: the pre-approval is given to it before its first step. (Another start that took the pre-approval in between leaves this run without it: it asks.)
            if let Err(why) = host.preapprovals().attach(&pid, &id, &folder) {
                tracing::warn!("run {id} was not given the pre-approval it named: {why}");
            }
        }
        let make = make_work(&StartPlan { max_steps, use_browser, use_windows, force_agent, requested, session_id, plan_run, agent });
        tokio::spawn(run_host::drive(registered, goal, Drive { origin, approval_wait_secs: body.approval_wait_secs, make }));
    }
    let code = if duplicate { StatusCode::OK } else { StatusCode::ACCEPTED };
    // It runs at once only when nothing is in front of it: no detached run, and no turn that somebody is waiting for.
    let state_word = if position == 0 && !super::turn_control::is_running() && super::turn_control::waiting_count() == 0 { "running" } else { "queued" };
    (
        code,
        Json(json!({
            "run_id": id,
            "state": state_word,
            "position": position,
            "duplicate": duplicate,
            "approval_wait_secs": body.approval_wait_secs.unwrap_or(60),
            "stream": format!("/api/v1/runs/{id}/stream"),
            "events": format!("/api/v1/runs/{id}/events"),
            "wait": format!("/api/v1/runs/{id}/wait"),
            "interrupt": format!("/api/v1/runs/{id}/interrupt"),
        })),
    )
        .into_response()
}

#[derive(Deserialize, Default, Debug)]
pub struct ContinueRequest {
    /// The most planning rounds of the run that goes on (the cap of its kind, less what the chain has used, when it is more or absent).
    #[serde(default)]
    pub max_steps: Option<u32>,
}

/// Starts a run that goes on from one that used its rounds or its time (the UI token): the same task, with what the first kept (the checklist, what the person added, a line that says where it
/// stopped) and none of what it read. It is detached like `POST /api/v1/runs` (202, with the id of the new run), is in the same session, and a chain of such runs is at most 48 rounds and 30
/// minutes. A second request for the same run is the same run (`duplicate`).
async fn run_continue_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, body: Option<Json<ContinueRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store().cloned() else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "runs are not being recorded, so there is nothing to go on from");
    };
    let max_steps = body.and_then(|Json(b)| b.max_steps);
    continue_run(&store, &run_id, max_steps, move |plan, going| going_on_work(&state, plan, going))
}

/// How a refusal to go on is answered.
fn continue_refusal(why: &continuation::Refused) -> Response {
    let status = match why {
        continuation::Refused::NotFound => StatusCode::NOT_FOUND,
        continuation::Refused::NoGoal => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::CONFLICT,
    };
    let mut body = json!({ "error": why.message() });
    match why {
        continuation::Refused::AlreadyContinued(child) => body["continued_by"] = json!(child),
        continuation::Refused::ChainSpent { rounds, ms } => body["chain"] = json!({ "rounds": rounds, "max_rounds": continuation::CHAIN_MAX_ROUNDS, "ms": ms, "max_ms": continuation::CHAIN_MAX_MS }),
        _ => {}
    }
    (status, Json(body)).into_response()
}

/// The whole of a continuation, with what makes the work of the run given (so that a test gives a scripted one): looks at the record, and starts the run that goes on.
pub(crate) fn continue_run(store: &Arc<AgentRunStore>, run_id: &str, max_steps: Option<u32>, make: impl FnOnce(&StartPlan, GoingOn) -> run_host::MakeWork) -> Response {
    let plan = match continuation::plan(store, run_id, &crate::agent_runs::scope::is_active) {
        Ok(p) => p,
        Err(why) => return continue_refusal(&why),
    };
    let left = continuation::rounds_left(&plan, crate::modules::browser_agent::CODING_MAX_ROUNDS);
    let steps = max_steps.map_or(left, |m| m.clamp(1, left));
    let going = GoingOn {
        resume: Resume { parent_run_id: run_id.to_string(), checklist: plan.checklist.clone(), steered: plan.steered.clone(), ..Default::default() },
        block: continuation::context_block(&plan),
        caller: plan.caller.clone(),
    };
    // The same key for the same run: a second click is the run the first one started, not a second one.
    let request = StartRequest { goal: plan.goal.clone(), max_steps: Some(steps), profile: store.profile_of(run_id), idempotency_key: Some(format!("continue.{run_id}")), ..Default::default() };
    start_run(Scope::Ui, request, move |p| make(p, going))
}

/// The steps of a plan as the model wrote them, one a line: the numbers and bullets taken off, at most five of at most 80 characters.
pub(crate) fn plan_steps(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(|l| l.trim().trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | ' ')).trim().to_string()).filter(|l| !l.is_empty()).collect();
    crate::modules::planner::plan::clean(&lines)
}

/// What an answer about a plan says when the plan cannot be answered: the status and the words.
fn plan_refusal(state: &str, expired: bool) -> Response {
    match (state, expired) {
        (_, true) => api_error(StatusCode::GONE, "this plan is more than a day old: ask for a new one"),
        ("approved", _) => api_error(StatusCode::CONFLICT, "this plan was approved already"),
        ("revised", _) => api_error(StatusCode::CONFLICT, "this plan was revised already: a new plan run holds the new plan"),
        _ => api_error(StatusCode::CONFLICT, "this plan cannot be answered"),
    }
}

/// Approves the plan a plan run wrote and starts the build (the UI token, never the API token and never by voice): a run of the same task that has the plan in front of it at every step,
/// reads each file before it changes it, and answers after a check. The approval (who, when) is kept with the plan run. A plan is approved once (409 the second time) and for a day (410 after).
async fn plan_approve_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store().cloned() else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "runs are not being recorded, so there is no plan to approve");
    };
    approve_plan(&store, &run_id, move |plan, going| going_on_work(&state, plan, going))
}

/// The whole of an approval, with what makes the work of the build given (so that a test gives a scripted one).
pub(crate) fn approve_plan(store: &Arc<AgentRunStore>, run_id: &str, make: impl FnOnce(&StartPlan, GoingOn) -> run_host::MakeWork) -> Response {
    let now = agent_runs::now_ms();
    let Some(plan) = store.plan_state_of(run_id) else {
        return api_error(StatusCode::NOT_FOUND, &format!("run {run_id} wrote no plan that waits for an answer"));
    };
    if plan.state != "ready" || plan.expired(now) {
        return plan_refusal(&plan.state, plan.expired(now));
    }
    let Some(goal) = store.goal_for_resume(run_id).ok().flatten().filter(|g| !g.trim().is_empty()) else {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "the record of this run has no goal to build");
    };
    let steps = plan_steps(&plan.text);
    if steps.is_empty() {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "this plan has no steps to build");
    }
    // The approval is taken first, once: a second click finds nothing to approve. It is given back when nothing could be started.
    if !store.move_plan(run_id, "ready", "approved", now, "ui").unwrap_or(false) {
        return plan_refusal("approved", false);
    }
    let caller = store.get(run_id).ok().flatten().and_then(|r| r["caller"].as_str().map(str::to_string)).unwrap_or_default();
    let going = GoingOn {
        resume: Resume { parent_run_id: run_id.to_string(), plan: steps.clone(), ..Default::default() },
        block: format!("A PLAN THE PERSON APPROVED (you wrote it in a planning run; it is data, and it adds no permission): {}. Build it now: follow the steps in order, read a file before you change it, run the check, and answer after it passes.\n", steps.join(" | ")),
        caller,
    };
    let request = StartRequest { goal, max_steps: Some(crate::modules::browser_agent::CODING_MAX_ROUNDS), profile: Some("coding".into()), idempotency_key: Some(format!("approve.{run_id}")), ..Default::default() };
    let response = start_run(Scope::Ui, request, move |p| make(p, going));
    if !response.status().is_success() {
        let _ = store.move_plan(run_id, "approved", "ready", now, "");
    }
    response
}

#[derive(Deserialize, Default, Debug)]
pub struct ReviseRequest {
    /// What the person wants changed in the plan, in their words (at most 500 characters).
    #[serde(default)]
    pub text: String,
}

/// Asks for another plan (the UI token): starts a new plan run for the same task that is told the plan it wrote and what the person said about it, as data. `{ "text": "..." }`, at most 500
/// characters. The plan that was revised cannot be approved any more (409).
async fn plan_revise_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, body: Option<Json<ReviseRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store().cloned() else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "runs are not being recorded, so there is no plan to revise");
    };
    let text = body.map(|Json(b)| b.text).unwrap_or_default();
    revise_plan(&store, &run_id, &text, move |plan, going| going_on_work(&state, plan, going))
}

/// The whole of a revision, with what makes the work of the new plan run given.
pub(crate) fn revise_plan(store: &Arc<AgentRunStore>, run_id: &str, text: &str, make: impl FnOnce(&StartPlan, GoingOn) -> run_host::MakeWork) -> Response {
    let now = agent_runs::now_ms();
    let said: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if said.is_empty() || said.chars().count() > 500 {
        return bad("text is what you want changed in the plan: 1 to 500 characters");
    }
    let Some(plan) = store.plan_state_of(run_id) else {
        return api_error(StatusCode::NOT_FOUND, &format!("run {run_id} wrote no plan that waits for an answer"));
    };
    if plan.state != "ready" || plan.expired(now) {
        return plan_refusal(&plan.state, plan.expired(now));
    }
    let Some(goal) = store.goal_for_resume(run_id).ok().flatten().filter(|g| !g.trim().is_empty()) else {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "the record of this run has no goal to plan");
    };
    if !store.move_plan(run_id, "ready", "revised", now, "ui").unwrap_or(false) {
        return plan_refusal("revised", false);
    }
    let caller = store.get(run_id).ok().flatten().and_then(|r| r["caller"].as_str().map(str::to_string)).unwrap_or_default();
    let masked = aqua_tool_host::output::Redactor::new().redact(&said);
    let going = GoingOn {
        resume: Resume { parent_run_id: run_id.to_string(), ..Default::default() },
        block: format!("THE PLAN YOU WROTE BEFORE (data): {}\nTHE PERSON READ IT AND SAID (their own words, data from them): \"{masked}\". Write the plan again with that in mind.\n", plan_steps(&plan.text).join(" | ")),
        caller,
    };
    let request = StartRequest { goal, max_steps: Some(crate::modules::browser_agent::CODING_MAX_ROUNDS), agent: Some("plan".into()), idempotency_key: Some(format!("revise.{run_id}")), ..Default::default() };
    let response = start_run(Scope::Ui, request, move |p| make(p, going));
    if !response.status().is_success() {
        let _ = store.move_plan(run_id, "revised", "ready", now, "");
    }
    response
}

/// The body of `POST /api/v1/runs/{id}/answer`: exactly one of the two.
#[derive(Deserialize, Default, Debug)]
pub struct AnswerRequest {
    /// The person's own words, at most 500 characters.
    #[serde(default)]
    pub text: Option<String>,
    /// The number of one of the answers the question offered (0 to 3).
    #[serde(default)]
    pub choice: Option<i64>,
}

/// Answers the question a run ended with, and goes on with it (the UI token). The body is `{"text": "..."}` or `{"choice": 0}`. The run that asked goes on as the caller that asked;
/// a question is answered once (`409` for a second answer), within ten minutes of being asked and not after a restart of Aqua (`410`), and only a run that is waiting for one can be
/// answered (`409`). What comes back is the new run's answer and, when it asks again, the next question.
async fn run_answer_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Path(run_id): Path<String>, body: Option<Json<AnswerRequest>>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let Some(store) = agent_runs::store().cloned() else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "runs are not being recorded, so there is no question to answer");
    };
    let body = body.map(|Json(b)| b).unwrap_or_default();
    answer_run(store, run_id, body, move |plan, store| crate::pipeline::run_answered_agent(&state.llm, Some(state.browser.clone()), Some(state.windows_mcp.clone()), plan, store)).await
}

/// How a refusal is answered: the status and the words.
fn refusal(why: &Refused) -> Response {
    let status = match why {
        Refused::BadBody | Refused::Empty | Refused::TooLong | Refused::BadChoice { .. } => StatusCode::BAD_REQUEST,
        Refused::NotFound => StatusCode::NOT_FOUND,
        Refused::Expired => StatusCode::GONE,
        Refused::NoGoal => StatusCode::UNPROCESSABLE_ENTITY,
        Refused::NoQuestion(_) | Refused::AlreadyAnswered(_) | Refused::NotWaiting(_) => StatusCode::CONFLICT,
    };
    let mut body = json!({ "error": why.message() });
    if let Refused::AlreadyAnswered(Some(child)) = why {
        body["continued_by"] = json!(child);
    }
    (status, Json(body)).into_response()
}

/// The whole of an answer, with what runs the agent given (so that a test gives a scripted one): checks the body, looks at the record, takes the one right to answer, takes the model's
/// turn, takes the answer, and goes on with it. The run is `run`'s: it is given the plan and the record, and gives back what the new run came to.
pub(crate) async fn answer_run<F>(store: std::sync::Arc<AgentRunStore>, run_id: String, body: AnswerRequest, run: F) -> Response
where
    F: FnOnce(&answer::Plan, &AgentRunStore) -> anyhow::Result<McpAgentResult> + Send + 'static,
{
    let given = match Given::from_body(body.text.as_deref(), body.choice) {
        Ok(g) => g,
        Err(why) => return refusal(&why),
    };
    // Two requests for the same run at once (a second click, a retry after a time-out) are not both served: the second is told so.
    let Some(claim) = resume::claim(&run_id) else {
        return api_error(StatusCode::CONFLICT, "an answer to this run is being served: wait for it");
    };
    let plan = match answer::plan(&store, &run_id, &given, agent_runs::now_ms()) {
        Ok(p) => p,
        Err(why) => return refusal(&why),
    };
    // The model's turn comes first and the answer is taken after it: a request that finds Aqua busy takes nothing, and the question waits as it did.
    let turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    match store.mark_answered(&run_id, agent_runs::now_ms(), "ui") {
        Ok(true) => {}
        Ok(false) => return refusal(&Refused::AlreadyAnswered(store.child_of(&run_id).ok().flatten())),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("the answer could not be written down: {e}")),
    }
    let started = std::time::Instant::now();
    let (work_store, work_plan) = (store.clone(), plan.clone());
    let outcome = run_turn(move || {
        for_client(true, || {
            // The question no longer waits in the person's conversation either: what they say next is not a second answer to it.
            crate::conversation::clear_pending_for(&work_plan.run_id);
            run(&work_plan, &work_store).map_err(|e| format!("{e:#}"))
        })
    })
    .await;
    drop(turn);
    drop(claim);
    match outcome {
        Ok(result) => {
            let continued_by = result.run_id.clone().or_else(|| store.child_of(&run_id).ok().flatten());
            // The next question as the record kept it (masked and cut), not as the planner wrote it.
            let question = continued_by.as_deref().and_then(|id| store.question_of(id).ok().flatten()).filter(|_| result.stopped_reason == "waiting_user").map(|q| {
                let mut v = q.view();
                v["run_id"] = json!(q.run_id);
                v
            });
            Json(json!({
                "run_id": run_id,
                "continued_by": continued_by,
                "answer": result.final_answer,
                "stopped_reason": result.stopped_reason,
                "tool_rounds": result.tool_rounds,
                "question": question,
                "latency_ms": started.elapsed().as_millis() as u64,
            }))
            .into_response()
        }
        Err(e) => {
            // A run that never began (no model, no turn) took nothing: the question waits for the person to try again.
            if store.child_of(&run_id).ok().flatten().is_none() {
                let _ = store.unmark_answered(&run_id);
            }
            pipeline_failure(&e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runs::store::{status, NewRun, RunEnd};
    use axum::body::to_bytes;

    fn store(name: &str) -> std::sync::Arc<AgentRunStore> {
        let dir = std::env::temp_dir().join(format!("aqua_answer_api_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::sync::Arc::new(AgentRunStore::open(dir.join("agent_runs.db")).unwrap())
    }

    /// A run of `caller` that ended with a question just now.
    fn asked(s: &AgentRunStore, id: &str, caller: &str, choices: &[&str]) {
        let now = agent_runs::now_ms();
        s.create_run(&NewRun { id: id.into(), started_at: now - 50, goal_redacted: "tidy".into(), goal_full: "tidy up the notes folder".into(), caller: caller.into(), route: "agent".into(), parent_run_id: None }).unwrap();
        s.set_question(id, "Which of the two files?", &choices.iter().map(|c| c.to_string()).collect::<Vec<_>>(), 1, caller, now).unwrap();
        s.end_run(id, now, &RunEnd { status: status::WAITING_USER.into(), stopped_reason: "waiting_user".into(), ..Default::default() }).unwrap();
    }

    fn ended(answer: &str, run_id: &str) -> McpAgentResult {
        McpAgentResult { final_answer: answer.into(), steps: vec![], tool_rounds: 2, stopped_reason: "final".into(), gate: None, run_id: Some(run_id.into()), question: None, choices: Vec::new(), sources: Vec::new() }
    }

    async fn read(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn text(t: &str) -> AnswerRequest {
        AnswerRequest { text: Some(t.into()), choice: None }
    }

    #[tokio::test]
    async fn a_question_that_waits_is_answered_and_the_new_run_is_the_one_that_asked_as_the_caller_that_asked() {
        let s = store("ok");
        asked(&s, "run_ok", "agent", &["a.rs", "b.rs"]);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_in = seen.clone();
        let (code, body) = read(
            answer_run(s.clone(), "run_ok".into(), text("the second one"), move |plan, store| {
                // The production way of going on, with the agent loop given as a script: what the run is started with, and as whom.
                crate::pipeline::run_answered_agent_with(plan, store, &|goal, rounds| {
                    *seen_in.lock().unwrap() = Some((goal.to_string(), rounds, crate::modules::browser_agent::windows_caller_now(), crate::modules::browser_agent::questions_asked_before()));
                    s_child(store, "run_ok_2", "run_ok");
                    Ok(ended("Done with b.rs.", "run_ok_2"))
                })
            })
            .await,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!((body["answer"].as_str(), body["continued_by"].as_str(), body["stopped_reason"].as_str(), body["run_id"].as_str()), (Some("Done with b.rs."), Some("run_ok_2"), Some("final"), Some("run_ok")));
        assert!(body["question"].is_null());
        let (goal, rounds, caller, asked_before) = seen.lock().unwrap().take().expect("the run was started");
        assert_eq!(goal, "tidy up the notes folder", "the same task");
        assert_eq!(caller, aqua_mcp_adapters::WindowsCaller::Agent, "an agent's run does not become the voice");
        assert_eq!(rounds, crate::modules::browser_agent::CODING_MAX_ROUNDS, "and it does not get the voice's six rounds");
        assert_eq!(asked_before, 1, "the task has asked one question already");
        assert_eq!(s.question_of("run_ok").unwrap().unwrap().answered_by, "ui");
    }

    #[tokio::test]
    async fn the_voice_run_that_asked_goes_on_as_the_voice_with_its_six_rounds() {
        let s = store("voice");
        asked(&s, "run_v", "voice", &[]);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_in = seen.clone();
        let (code, _) = read(
            answer_run(s.clone(), "run_v".into(), text("yes"), move |plan, store| {
                crate::pipeline::run_answered_agent_with(plan, store, &|_, rounds| {
                    *seen_in.lock().unwrap() = Some((rounds, crate::modules::browser_agent::windows_caller_now()));
                    Ok(ended("ok", "run_v_2"))
                })
            })
            .await,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(seen.lock().unwrap().take(), Some((6, aqua_mcp_adapters::WindowsCaller::Voice)));
    }

    /// A record of the run that goes on, as the loop makes it.
    fn s_child(s: &AgentRunStore, id: &str, parent: &str) {
        s.create_run(&NewRun { id: id.into(), started_at: agent_runs::now_ms(), goal_full: "tidy up the notes folder".into(), parent_run_id: Some(parent.into()), ..Default::default() }).unwrap();
    }

    #[tokio::test]
    async fn the_number_of_a_choice_is_the_answer_and_a_number_that_was_not_offered_is_refused_before_anything_starts() {
        let s = store("choice");
        asked(&s, "run_c", "api", &["a.rs", "b.rs"]);
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = started.clone();
        let (code, body) = read(answer_run(s.clone(), "run_c".into(), AnswerRequest { text: None, choice: Some(3) }, move |_, _| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ended("x", "r"))
        })
        .await)
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("offered 2"), "{body}");
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst) && s.question_of("run_c").unwrap().unwrap().answered_at.is_none(), "nothing was started or taken");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen_in = seen.clone();
        let (code, _) = read(answer_run(s.clone(), "run_c".into(), AnswerRequest { text: None, choice: Some(1) }, move |plan, _| {
            *seen_in.lock().unwrap() = plan.text.clone();
            Ok(ended("ok", "run_c_2"))
        })
        .await)
        .await;
        assert_eq!((code, seen.lock().unwrap().as_str()), (StatusCode::OK, "b.rs"));
    }

    #[tokio::test]
    async fn a_second_answer_is_a_conflict_an_old_question_is_gone_and_a_body_with_both_or_neither_is_refused() {
        let s = store("refused");
        asked(&s, "run_r", "voice", &[]);
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let go = |ran: &std::sync::Arc<std::sync::atomic::AtomicUsize>| {
            let ran = ran.clone();
            move |_: &answer::Plan, _: &AgentRunStore| {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ended("done", "run_r_2"))
            }
        };
        assert_eq!(read(answer_run(s.clone(), "run_r".into(), text("yes"), go(&ran)).await).await.0, StatusCode::OK);
        let (code, body) = read(answer_run(s.clone(), "run_r".into(), text("yes again"), go(&ran)).await).await;
        assert_eq!(code, StatusCode::CONFLICT, "{body}");
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1, "the second answer started nothing");
        // Unknown run, a run that asked nothing, a question nobody answered in time, both fields, neither, an empty text, a text that is too long.
        assert_eq!(read(answer_run(s.clone(), "nobody".into(), text("x"), go(&ran)).await).await.0, StatusCode::NOT_FOUND);
        s.create_run(&NewRun { id: "plain".into(), started_at: 1, goal_full: "g".into(), ..Default::default() }).unwrap();
        assert_eq!(read(answer_run(s.clone(), "plain".into(), text("x"), go(&ran)).await).await.0, StatusCode::CONFLICT);
        s.create_run(&NewRun { id: "old".into(), started_at: 1, goal_full: "g".into(), caller: "api".into(), ..Default::default() }).unwrap();
        s.set_question("old", "Which?", &[], 1, "api", 1).unwrap();
        s.end_run("old", 2, &RunEnd { status: status::WAITING_USER.into(), stopped_reason: "waiting_user".into(), ..Default::default() }).unwrap();
        assert_eq!(read(answer_run(s.clone(), "old".into(), text("x"), go(&ran)).await).await.0, StatusCode::GONE);
        for bad in [AnswerRequest { text: Some("a".into()), choice: Some(0) }, AnswerRequest::default(), text("   "), text(&"x".repeat(501))] {
            assert_eq!(read(answer_run(s.clone(), "run_r".into(), bad, go(&ran)).await).await.0, StatusCode::BAD_REQUEST);
        }
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1, "nothing but the first answer started a run");
    }

    #[tokio::test]
    async fn a_run_that_asks_again_tells_the_next_question_and_a_run_that_could_not_start_gives_the_answer_back() {
        let s = store("again");
        asked(&s, "run_a", "api", &[]);
        let (code, body) = read(
            answer_run(s.clone(), "run_a".into(), text("b.rs"), |_, store| {
                // The run that goes on asks the second question of its task.
                s_child(store, "run_a_2", "run_a");
                store.set_question("run_a_2", "And which folder?", &["src".to_string()], 2, "api", agent_runs::now_ms()).unwrap();
                Ok(McpAgentResult { final_answer: "And which folder?".into(), steps: vec![], tool_rounds: 1, stopped_reason: "waiting_user".into(), gate: None, run_id: Some("run_a_2".into()), question: Some("And which folder?".into()), choices: vec!["src".into()], sources: Vec::new() })
            })
            .await,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!((body["question"]["text"].as_str(), body["question"]["run_id"].as_str(), body["question"]["asked"].as_u64()), (Some("And which folder?"), Some("run_a_2"), Some(2)));
        assert_eq!(body["question"]["choices"], json!(["src"]));
        // The model is not there: the run never began, so the question waits and can be answered again.
        asked(&s, "run_b", "api", &[]);
        let (code, _) = read(answer_run(s.clone(), "run_b".into(), text("x"), |_, _| Err(anyhow::anyhow!("the local language model is not available"))).await).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(s.question_of("run_b").unwrap().unwrap().answered_at.is_none(), "the answer was given back");
        assert_eq!(read(answer_run(s.clone(), "run_b".into(), text("x"), |_, _| Ok(ended("done", "run_b_2"))).await).await.0, StatusCode::OK);
    }

    // ---- starting a run (card OA10-03, UX-03) -----------------------------------------------------------------------------------

    /// The work of a run that ends at once, and how many times it was made.
    fn scripted_work(made: &Arc<std::sync::atomic::AtomicUsize>) -> run_host::MakeWork {
        let made = made.clone();
        Arc::new(move |_goal: &str| -> Work {
            made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(|| Ok(ended("Tidied.", "run_not_recorded")))
        })
    }

    fn start(goal: &str) -> StartRequest {
        StartRequest { goal: goal.into(), ..Default::default() }
    }

    #[tokio::test]
    async fn a_run_is_started_at_once_with_the_id_it_will_have_and_a_retry_with_the_same_key_starts_nothing_twice() {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = StartRequest { idempotency_key: Some("key.oa10-03.start".into()), approval_wait_secs: Some(120), ..start("tidy up the notes folder") };
        let (code, first) = read(start_run(Scope::Api, body, |_| scripted_work(&made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{first}");
        let id = first["run_id"].as_str().expect("the id the run will be recorded under").to_string();
        assert!(id.starts_with("run_"), "{id}");
        assert_eq!((first["duplicate"].clone(), first["approval_wait_secs"].clone()), (json!(false), json!(120)));
        assert_eq!(first["stream"], format!("/api/v1/runs/{id}/stream"));
        assert_eq!(first["wait"], format!("/api/v1/runs/{id}/wait"));
        // The run is the API token's: the registry says so.
        assert_eq!(run_host::lookup(&id).map(|v| v.origin), Some("api"));
        // A retry that did not hear the answer: the same run, 200, and nothing was made again.
        let body = StartRequest { idempotency_key: Some("key.oa10-03.start".into()), approval_wait_secs: Some(120), ..start("tidy up the notes folder") };
        let (code, again) = read(start_run(Scope::Api, body, |_| scripted_work(&made))).await;
        assert_eq!((code, again["run_id"].clone(), again["duplicate"].clone()), (StatusCode::OK, json!(id), json!(true)), "{again}");
        // The same key for another request is refused; a run of the app's own is the UI's.
        let other = StartRequest { idempotency_key: Some("key.oa10-03.start".into()), ..start("something else entirely") };
        assert_eq!(read(start_run(Scope::Api, other, |_| scripted_work(&made))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        let (code, app) = read(start_run(Scope::Ui, start("write the changelog"), |_| scripted_work(&made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{app}");
        assert_eq!(run_host::lookup(app["run_id"].as_str().unwrap()).map(|v| v.origin), Some("ui"));
        // Both runs end (the scripted work answers at once), and the work was made once for each.
        for run in [id.as_str(), app["run_id"].as_str().unwrap()] {
            for _ in 0..500 {
                if run_host::lookup(run).is_some_and(|v| v.state == run_host::State::Ended) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert_eq!(run_host::lookup(run).and_then(|v| v.ended).map(|e| e.outcome), Some("done"), "{run}");
        }
        assert_eq!(made.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_start_that_is_wrong_is_refused_before_a_place_in_the_line_is_taken() {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refused = [
            (start("   "), "an empty goal"),
            (start(&"x".repeat(MAX_GOAL_CHARS + 1)), "a goal that is too long"),
            (StartRequest { profile: Some("poetry".into()), ..start("do it") }, "a kind of task nobody knows"),
            (StartRequest { approval_wait_secs: Some(5), ..start("do it") }, "a wait that is too short"),
            (StartRequest { approval_wait_secs: Some(601), ..start("do it") }, "a wait that is too long"),
            (StartRequest { idempotency_key: Some("not a key!".into()), ..start("do it") }, "a key that is not one"),
        ];
        for (body, what) in refused {
            let (code, answer) = read(start_run(Scope::Api, body, |_| scripted_work(&made))).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{what}: {answer}");
        }
        // A folder that Aqua may not use is refused, and nothing adds it to the folders it may.
        let (code, answer) = read(start_run(Scope::Api, StartRequest { directory: Some("Z:\\no\\such\\place\\that\\is\\allowed".into()), ..start("do it") }, |_| scripted_work(&made))).await;
        assert!(matches!(code, StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE), "{code} {answer}");
        assert_eq!(made.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing was made for any of them");
    }

    // ---- continuing a run, and the plan of a plan run (card OA10-04: WF-12, AG-06) --------------------------------------------------------------------------------------------

    /// Waits until a detached run has ended. A test that starts one and returns would drop the runtime that drives it, and the run would stay queued in the registry of the process,
    /// where every run registered after it waits for it.
    async fn until_ended(run_id: &str) {
        for _ in 0..500 {
            if run_host::lookup(run_id).is_some_and(|v| v.state == run_host::State::Ended) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the run never ended: {run_id}");
    }

    /// What `make` was given for a run that goes on: the run it goes on from, the plan, the block, whether it is a plan run.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Vec<String>, String, bool, Vec<(String, String)>, Vec<String>, String)>>>;

    fn recording(seen: &Seen, made: &Arc<std::sync::atomic::AtomicUsize>) -> impl FnOnce(&StartPlan, GoingOn) -> run_host::MakeWork {
        let (seen, made) = (seen.clone(), made.clone());
        move |plan, going| {
            seen.lock().unwrap().push((going.resume.parent_run_id.clone(), going.resume.plan.clone(), going.block.clone(), plan.plan_run, going.resume.checklist.clone(), going.resume.steered.clone(), going.caller.clone()));
            scripted_work(&made)
        }
    }

    fn a_run_that_stopped(s: &AgentRunStore, id: &str, state: &str, reason: &str, rounds: u32) {
        s.create_run(&NewRun { id: id.into(), started_at: agent_runs::now_ms() - 60_000, goal_redacted: "short".into(), goal_full: "rename the struct in twelve files and run the tests".into(), caller: "agent".into(), route: "agent".into(), parent_run_id: None }).unwrap();
        s.set_profile(id, "coding").unwrap();
        s.end_run(id, agent_runs::now_ms() - 30_000, &RunEnd { status: state.into(), stopped_reason: reason.into(), rounds, ..Default::default() }).unwrap();
    }

    #[tokio::test]
    async fn a_run_that_used_its_rounds_is_continued_by_a_detached_run_that_has_what_it_kept_and_a_second_request_is_the_same_run() {
        let s = store("continue");
        a_run_that_stopped(&s, "cont_1", "failed", "max_steps", 24);
        s.set_coding_record("cont_1", r#"{"objective":"rename the struct","files_read":["a.rs"],"files_changed":["a.rs"],"last_check":null,"checks":[],"edits_since_check":true,"blocked":null,"next":"run the project's check and then answer"}"#, 5).unwrap();
        s.set_checklist("cont_1", &[("rename in a.rs".into(), "done".into()), ("rename in b.rs".into(), "doing".into())]).unwrap();
        s.set_steered("cont_1", &["also update the docs".into()]).unwrap();
        let (seen, made) = (Seen::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let (code, started) = read(continue_run(&s, "cont_1", None, recording(&seen, &made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{started}");
        assert!(started["run_id"].as_str().is_some_and(|id| id.starts_with("run_")), "{started}");
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        let (parent, plan, block, plan_run, checklist, steered, caller) = &got[0];
        assert_eq!((parent.as_str(), plan.len(), *plan_run, caller.as_str()), ("cont_1", 0, false, "agent"));
        assert!(block.contains("Where I stopped: Objective: rename the struct") && block.contains("read a file again before you edit it"), "{block}");
        assert_eq!(checklist, &vec![("rename in a.rs".to_string(), "done".to_string()), ("rename in b.rs".to_string(), "doing".to_string())]);
        assert_eq!(steered, &vec!["also update the docs".to_string()]);
        // A second click is the run the first started, and the work was made once.
        let (code, again) = read(continue_run(&s, "cont_1", None, recording(&seen, &made))).await;
        assert_eq!((code, again["run_id"].clone(), again["duplicate"].clone()), (StatusCode::OK, started["run_id"].clone(), json!(true)), "{again}");
        until_ended(started["run_id"].as_str().unwrap()).await;
        // Refusals: unknown, a run that finished, a run that was stopped by the person, a run that is still going.
        assert_eq!(read(continue_run(&s, "nobody", None, recording(&seen, &made))).await.0, StatusCode::NOT_FOUND);
        a_run_that_stopped(&s, "cont_done", "completed", "final", 3);
        assert_eq!(read(continue_run(&s, "cont_done", None, recording(&seen, &made))).await.0, StatusCode::CONFLICT);
        a_run_that_stopped(&s, "cont_stopped", "cancelled", "cancelled", 3);
        assert_eq!(read(continue_run(&s, "cont_stopped", None, recording(&seen, &made))).await.0, StatusCode::CONFLICT);
        // A chain that has used its 48 rounds is refused, with the numbers.
        a_run_that_stopped(&s, "chain_1", "failed", "max_steps", 24);
        s.create_run(&NewRun { id: "chain_2".into(), started_at: agent_runs::now_ms() - 20_000, goal_full: "g".into(), parent_run_id: Some("chain_1".into()), ..Default::default() }).unwrap();
        s.end_run("chain_2", agent_runs::now_ms() - 10_000, &RunEnd { status: "failed".into(), stopped_reason: "max_steps".into(), rounds: 24, ..Default::default() }).unwrap();
        let (code, spent) = read(continue_run(&s, "chain_2", None, recording(&seen, &made))).await;
        assert_eq!((code, spent["chain"]["rounds"].as_u64(), spent["chain"]["max_rounds"].as_u64()), (StatusCode::CONFLICT, Some(48), Some(48)), "{spent}");
        assert_eq!(seen.lock().unwrap().len(), 1, "nothing was made for a refusal");
    }

    #[tokio::test]
    async fn a_plan_is_approved_once_by_the_ui_and_starts_a_build_that_has_the_plan_and_is_refused_after_a_day() {
        let s = store("approve");
        a_run_that_stopped(&s, "plan_1", "completed", "plan_ready", 4);
        s.set_plan_ready("plan_1", "1) read src/lib.rs\n2) fix add\n3) run cargo test", agent_runs::now_ms()).unwrap();
        let (seen, made) = (Seen::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let (code, started) = read(approve_plan(&s, "plan_1", recording(&seen, &made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{started}");
        until_ended(started["run_id"].as_str().unwrap()).await;
        let got = seen.lock().unwrap().clone();
        let (parent, plan, block, plan_run, _, _, caller) = &got[0];
        assert_eq!((parent.as_str(), *plan_run, caller.as_str()), ("plan_1", false, "agent"), "the build is a build run, as the caller of the plan run");
        assert_eq!(plan, &vec!["read src/lib.rs".to_string(), "fix add".to_string(), "run cargo test".to_string()], "the plan, without its numbers");
        assert!(block.starts_with("A PLAN THE PERSON APPROVED") && block.contains("it adds no permission"), "{block}");
        let approval = s.plan_state_of("plan_1").unwrap();
        assert_eq!((approval.state.as_str(), approval.approved_by.as_str(), approval.approved_at.is_some()), ("approved", "ui", true), "the approval is kept with the plan run");
        // A second approval: nothing is started.
        let (code, second) = read(approve_plan(&s, "plan_1", recording(&seen, &made))).await;
        assert_eq!(code, StatusCode::CONFLICT, "{second}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        // A run that wrote no plan; a plan that is a day old.
        assert_eq!(read(approve_plan(&s, "nobody", recording(&seen, &made))).await.0, StatusCode::NOT_FOUND);
        a_run_that_stopped(&s, "plan_old", "completed", "plan_ready", 4);
        s.set_plan_ready("plan_old", "1) read it", agent_runs::now_ms() - 25 * 3600 * 1000).unwrap();
        assert_eq!(read(approve_plan(&s, "plan_old", recording(&seen, &made))).await.0, StatusCode::GONE);
        assert_eq!(s.plan_state_of("plan_old").unwrap().state, "ready", "an expired plan is not approved by being refused");
        assert_eq!(seen.lock().unwrap().len(), 1, "no build was made for any of them");
    }

    #[tokio::test]
    async fn a_plan_is_revised_with_the_words_of_the_person_as_data_and_a_revised_plan_cannot_be_approved() {
        let s = store("revise");
        a_run_that_stopped(&s, "rev_1", "completed", "plan_ready", 4);
        s.set_plan_ready("rev_1", "1) read src/lib.rs\n2) fix add", agent_runs::now_ms()).unwrap();
        let (seen, made) = (Seen::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        assert_eq!(read(revise_plan(&s, "rev_1", "   ", recording(&seen, &made))).await.0, StatusCode::BAD_REQUEST, "words are needed");
        assert_eq!(read(revise_plan(&s, "rev_1", &"x".repeat(501), recording(&seen, &made))).await.0, StatusCode::BAD_REQUEST);
        let (code, started) = read(revise_plan(&s, "rev_1", "use b.rs instead, password=hunter2secret99", recording(&seen, &made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{started}");
        until_ended(started["run_id"].as_str().unwrap()).await;
        let got = seen.lock().unwrap().clone();
        let (parent, _, block, plan_run, _, _, _) = &got[0];
        assert_eq!((parent.as_str(), *plan_run), ("rev_1", true), "another plan run");
        assert!(block.contains("THE PERSON READ IT AND SAID") && block.contains("use b.rs instead") && block.contains("read src/lib.rs | fix add"), "{block}");
        assert!(!block.contains("hunter2secret99"), "{block}");
        assert_eq!(s.plan_state_of("rev_1").unwrap().state, "revised");
        assert_eq!(read(approve_plan(&s, "rev_1", recording(&seen, &made))).await.0, StatusCode::CONFLICT, "a revised plan is not the plan to build");
        assert_eq!(read(revise_plan(&s, "rev_1", "again", recording(&seen, &made))).await.0, StatusCode::CONFLICT, "revised once");
    }

    #[tokio::test]
    async fn a_plan_run_is_started_with_agent_plan_in_a_session_and_the_api_token_may_only_name_a_session_of_its_own() {
        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let planned = Arc::new(std::sync::Mutex::new(Vec::<(bool, bool, bool, Option<String>)>::new()));
        let make = |planned: &Arc<std::sync::Mutex<Vec<(bool, bool, bool, Option<String>)>>>, made: &Arc<std::sync::atomic::AtomicUsize>| {
            let (planned, made) = (planned.clone(), made.clone());
            move |p: &StartPlan| {
                planned.lock().unwrap().push((p.plan_run, p.use_browser, p.use_windows, p.session_id.clone()));
                scripted_work(&made)
            }
        };
        let body = StartRequest { agent: Some("plan".into()), session_id: Some("oa1004-plan-session".into()), ..start("Plan the rename of the parser struct in rust_ok_then_bug") };
        let (code, started) = read(start_run(Scope::Ui, body, make(&planned, &made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{started}");
        until_ended(started["run_id"].as_str().unwrap()).await;
        assert_eq!(planned.lock().unwrap().clone(), vec![(true, false, false, Some("oa1004-plan-session".to_string()))], "a plan run has no browser and no desktop, and the session of the request");
        // Words that are not a kind of run, and a session name that is not one.
        for bad_body in [StartRequest { agent: Some("destroy".into()), ..start("do it") }, StartRequest { session_id: Some("../../x".into()), ..start("do it") }] {
            assert_eq!(read(start_run(Scope::Ui, bad_body, make(&planned, &made))).await.0, StatusCode::BAD_REQUEST);
        }
        // The API token may name a session that is new or its own, and a session that holds a run of the person's is `404` to it.
        if let Some(store) = agent_runs::store() {
            store.create_run(&NewRun { id: "oa1004_person_run".into(), started_at: 1, goal_full: "g".into(), ..Default::default() }).unwrap();
            store.set_run_session("oa1004_person_run", "oa1004-person-session", "").unwrap();
            let theirs = StartRequest { session_id: Some("oa1004-person-session".into()), ..start("do it") };
            assert_eq!(read(start_run(Scope::Api, theirs, make(&planned, &made))).await.0, StatusCode::NOT_FOUND);
        }
        let fresh = StartRequest { session_id: Some("oa1004-brand-new".into()), ..start("do it") };
        let (code, fresh_started) = read(start_run(Scope::Api, fresh, make(&planned, &made))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{fresh_started}");
        until_ended(fresh_started["run_id"].as_str().unwrap()).await;
        // The steps of a plan are the lines of the plan without their numbers.
        assert_eq!(plan_steps("1) read a.rs\n2. fix it\n- run the check\n\n  \n* answer"), vec!["read a.rs", "fix it", "run the check", "answer"]);
        assert_eq!(plan_steps(&"step\n".repeat(9)).len(), 1, "a step is not given twice, and five at most");
    }
}
