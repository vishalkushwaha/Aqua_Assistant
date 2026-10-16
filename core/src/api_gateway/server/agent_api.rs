//! The agent request (card OA3-06, moved out of `server.rs`): `POST /mcp/agent`, the work it starts (`agent_request_work`, shared with the runs that detach from a connection) and `POST /commands/{name}/run`, which starts a custom command through the same entry.

use super::*;

/// The chat model only knows its training data, which ends long before today. Give it the real date
/// and tell it plainly that it cannot see live data, so it says so instead of quoting last year's
/// figures as if they were current.
pub(crate) fn grounded_chat_prompt(goal: &str) -> String {
    let today = chrono::Local::now().format("%A, %d %B %Y");
    format!(
        "Today's date is {today}. Answer from what you know, in plain English. Do not invent today's prices, news, weather, or scores. If the question needs those, say you do not have today's figure.

{goal}"
    )
}

#[derive(Deserialize)]
pub struct UnifiedMcpAgentRequest {
    pub goal: String,
    #[serde(default = "default_agent_max_steps")]
    pub max_steps: u32,
    /// Start Browser MCP if not already running.
    #[serde(default = "default_true")]
    pub use_browser: bool,
    /// Start Windows-MCP if not already running.
    #[serde(default = "default_true")]
    pub use_windows: bool,
    /// The caller says this is a task, not a conversation: a request the gate would answer in chat goes to the agent. A refusal
    /// stays a refusal.
    #[serde(default)]
    pub force_agent: bool,
    /// Run it in the background: the answer is at once, with a job number (`GET /api/v1/queue` says how it goes). A background run waits for the
    /// person's own requests at each step, takes a longer clock, and is refused at once where it would have to ask.
    #[serde(default)]
    pub background: bool,
    /// A key for this start, for a background run: the same key with the same request starts nothing new (the answer is the job started the first time, with `duplicate: true`), so a
    /// retry after a timeout cannot start the work twice. One key, one request: the same key with another request is refused (422). Remembered for a day, in memory.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// What kind of task this is, when the caller knows: `coding`, `files`, `web` or `desktop` (card OA4-01). Without it the goal's words decide (a request to work on code that names a source
    /// file or a word of code is `coding`). What is said wins: `coding` gives the run the coding prompt, 24 rounds, read before write, a plan and a question, and the check of its answer
    /// against a test, a build or a lint run; `coding` and `files` also count as `force_agent` (the caller says this is a task). Any other word is refused with 400.
    #[serde(default)]
    pub profile: Option<String>,
    /// The run may call only these tools (patterns such as `fs.read` or `git.*`; at most 40, each at most 80 characters): the tool host refuses every other call with the refusal a loaded skill's list
    /// gives, and the list of tools the model is shown holds only these. It can only narrow what the run may do (card OA11-02).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// The packs of tools the run is shown (`files`, `terminal`, `git`, ...; at most 8), instead of the ones the words of its goal pick (card OA11-02).
    #[serde(default)]
    pub forced_packs: Vec<String>,
    /// Set in the core by a custom command that ran steps (never read from a request): the goal holds text a program printed, so the run starts as one that has read text from outside.
    #[serde(skip)]
    pub tainted_start: bool,
    /// The session the run is put in: a name of 1 to 64 letters, digits and `_ . : -` (card OA10-04, UX-10). A coding run of a session is told what the earlier runs of it came to when
    /// `AQUA_CODING_SESSIONS` is on. Without it the run is in a session of its own, or in the session of the run it goes on from.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The agent the run is (card OA11-04, `AQUA_AGENTS=on`): `build` (the agent as it is, the same as no agent), `plan`, `review`, or the name of an agent of yours (`data/agents/<name>.md`) or of a
    /// project whose `.aqua` text you trusted. The run then has only the tools the agent names (the host refuses every other), the agent's words as notes after the rules, its packs and its limit
    /// of rounds and seconds. It can only be given fewer powers than the run would have. A name that is no agent ends the run at once with the names there are. Refused with 409 while
    /// agents are off.
    #[serde(default)]
    pub agent: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The agent a request asks the run to be (card OA11-04): `None` for no agent in particular (nothing, or `build`, which is the run as it always was). `Err` is the answer to give: a name that is
/// not a name (400), or agents that are off (409, so that a request that asks for an agent's narrowing does not quietly run without it).
pub(in crate::api_gateway) fn requested_agent(agent: Option<&str>) -> Result<Option<String>, axum::response::Response> {
    let Some(name) = agent.map(|a| a.trim().to_ascii_lowercase()).filter(|a| !a.is_empty() && a != "build") else { return Ok(None) };
    if name.len() > 32 || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err(api_error(axum::http::StatusCode::BAD_REQUEST, "agent is the name of an agent: 1 to 32 of a-z, 0-9, - and _"));
    }
    if !crate::agents::enabled() {
        return Err((axum::http::StatusCode::CONFLICT, Json(serde_json::json!({ "error": "agents are off: set AQUA_AGENTS=on and restart Aqua to ask for one", "enabled": false }))).into_response());
    }
    Ok(Some(name))
}

/// What a request says about the run it starts, as the options the agent tools take (card OA11-02). `Err` is the answer to give: a list that is too long or a pattern that is not a name.
pub(super) fn run_options_of(allowed: &[String], packs: &[String], tainted: bool) -> Result<crate::agent_runs::scope::RunOptions, String> {
    if allowed.len() > 40 || allowed.iter().any(|a| a.is_empty() || a.len() > 80 || a.chars().any(|c| c.is_control() || c.is_whitespace())) {
        return Err("allowed_tools holds at most 40 tool names or patterns (like fs.read or git.*), each 1 to 80 characters with no spaces".into());
    }
    if packs.len() > 8 || packs.iter().any(|p| p.is_empty() || p.len() > 40) {
        return Err("forced_packs holds at most 8 pack names (files, terminal, git, ...)".into());
    }
    Ok(crate::agent_runs::scope::RunOptions { allowed_tools: allowed.to_vec(), forced_packs: packs.to_vec(), tainted })
}

async fn mcp_unified_agent_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<UnifiedMcpAgentRequest>,
) -> impl IntoResponse {
    // What the caller says the task is (card OA4-01): a word that is not one of the four is refused before anything starts.
    let requested = match crate::modules::run_profile::parse_requested(payload.profile.as_deref()) {
        Ok(p) => p,
        Err(why) => return api_error(axum::http::StatusCode::BAD_REQUEST, &why),
    };
    let run_options = match run_options_of(&payload.allowed_tools, &payload.forced_packs, payload.tainted_start) {
        Ok(o) => o,
        Err(why) => return api_error(axum::http::StatusCode::BAD_REQUEST, &why),
    };
    // The session the caller names (card OA10-04): a name that is not one is refused, and the API token may only name a session of its own.
    let session = match super::super::sessions_api::named_session(payload.session_id.as_deref(), scope == Scope::Ui) {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    // The agent the request asks the run to be (card OA11-04): refused before anything starts.
    let agent = match requested_agent(payload.agent.as_deref()) {
        Ok(a) => a,
        Err(refused) => return refused,
    };
    let goal = payload.goal;
    let max_steps = payload.max_steps.clamp(1, crate::modules::browser_agent::max_rounds_for_request(&goal, requested));
    let use_browser = payload.use_browser;
    let use_windows = payload.use_windows;
    // A task of code or files, said by the caller, is a task: it is not answered in chat.
    let force_agent = payload.force_agent || requested.is_some_and(|p| p.works_on_files());
    let background = payload.background;
    let idempotency_key = payload.idempotency_key;
    if idempotency_key.is_some() && !background {
        return api_error(axum::http::StatusCode::BAD_REQUEST, "an idempotency key is for a background run (background: true): a run in front answers in the same call");
    }
    let job_goal = goal.clone();
    let options_key = format!("{:?}|{:?}", run_options.allowed_tools, run_options.forced_packs);

    let work = agent_request_work(&state, AgentWorkRequest { goal, max_steps, use_browser, use_windows, force_agent, requested, options: run_options });

    // The run it starts is recorded with who asked for it (the events of a run asked for with the API token can be read with the API token), also when it waits in the queue.
    let ui = scope == Scope::Ui;
    let work = move || crate::agent_runs::with_origin(ui, || crate::agent_runs::with_session(session, || crate::agents::with_requested(agent, work)));

    if background {
        // No turn is taken: the person's own requests go first, and this run waits for them at each of its steps (see `queue`).
        // What the request was, for a key: the same key is the same request only when all of this is the same.
        let fingerprint = format!("{job_goal}|{max_steps}|{use_browser}|{use_windows}|{force_agent}|{}|{options_key}", requested.map_or("", |p| p.as_str()));
        // Its end is told to the person at their next turn (`announce`).
        let submitted = crate::queue::submit_keyed(idempotency_key.as_deref(), &fingerprint, true, &job_goal, move || match work() {
            Ok(r) => crate::queue::Outcome { run_id: r.run_id, answer: r.final_answer, stopped_reason: r.stopped_reason },
            Err(e) => crate::queue::Outcome { run_id: None, answer: format!("{e:#}"), stopped_reason: "error".into() },
        });
        return match submitted {
            Ok(s) => {
                let state = crate::queue::job_state(s.id).and_then(|st| serde_json::to_value(st).ok()).unwrap_or_else(|| serde_json::json!(if s.place == 0 { "running" } else { "queued" }));
                let code = if s.duplicate { axum::http::StatusCode::OK } else { axum::http::StatusCode::ACCEPTED };
                (code, Json(serde_json::json!({ "job_id": s.id, "state": state, "place": s.place, "duplicate": s.duplicate, "status": "GET /api/v1/queue" }))).into_response()
            }
            Err(e @ crate::queue::SubmitError::Full(_)) => api_error(axum::http::StatusCode::TOO_MANY_REQUESTS, &e.to_string()),
            Err(e @ crate::queue::SubmitError::KeyMismatch(_)) => api_error(axum::http::StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
            Err(e @ crate::queue::SubmitError::BadKey(_)) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
            Err(e) => api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }

    // An agent task takes its turn in the queue like every other task, and Stop (or hanging up) ends it at its next step.
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let result = run_turn(move || work().map_err(|e| format!("{e:#}"))).await;

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

/// What an agent request is made of, once its body has been read and checked.
pub(in crate::api_gateway) struct AgentWorkRequest {
    pub goal: String,
    pub max_steps: u32,
    pub use_browser: bool,
    pub use_windows: bool,
    /// The caller says this is a task: a request the gate would answer in chat goes to the agent.
    pub force_agent: bool,
    pub requested: Option<crate::modules::run_profile::RunProfile>,
    /// What a custom command says about the run: the tools it may call, the packs it is shown, whether its goal holds text a program printed (card OA11-02). Empty for any other request.
    pub options: crate::agent_runs::scope::RunOptions,
}

/// The work of one agent request, as a closure for a thread that has the model's turn: the weather and the gate first, then the agent loop (card OA10-03 took it out of
/// `mcp_unified_agent_endpoint`, whose body it was word for word, so that a run detached from any connection does the same work).
pub(in crate::api_gateway) fn agent_request_work(state: &AppState, request: AgentWorkRequest) -> impl FnOnce() -> anyhow::Result<McpAgentResult> + Send + 'static {
    let llm = state.llm.clone();
    let browser_mcp = state.browser.clone();
    let windows_mcp = state.windows_mcp.clone();
    let laya = state.laya.clone();
    let AgentWorkRequest { goal, max_steps, use_browser, use_windows, force_agent, requested, options: run_options } = request;
    move || -> anyhow::Result<McpAgentResult> {
        if let Some(answer) = crate::live_weather::handle_request(&goal) {
            return Ok(McpAgentResult {
                final_answer: answer,
                steps: vec![],
                tool_rounds: 0,
                stopped_reason: "weather".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
        }
        let mcp_available = use_browser || use_windows;
        let decision = laya.decide_with_timeout(
            &goal,
            mcp_available,
            LayaGate::gate_timeout(),
            |_| {},
        );
        let decision = crate::laya_validator::refine_route(decision, &goal, &llm, mcp_available);
        let decision = if force_agent { decision.forced_to_agent(mcp_available) } else { decision };
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
                let browser = if use_browser {
                    Some(browser_mcp)
                } else {
                    None
                };
                let windows = if use_windows {
                    Some(windows_mcp)
                } else {
                    None
                };
                let hint = decision.surface.agent_prompt_line();
                let out = crate::agent_runs::scope::with_run_options(run_options, || crate::modules::run_profile::with_requested_profile(requested, || run_mcp_agent_shared(&llm, browser, windows, &goal, max_steps, hint)))?;
                if out.tool_rounds > 0 {
                    laya.feedback_route(&goal, GateRoute::McpAgent);
                }
                Ok(out.with_gate(decision))
            }
        }
    }
}

#[derive(Deserialize)]
pub struct CommandRunRequest {
    /// The words after the name of the command (`src/lib.rs "two words"`): put where `$1`..`$9` and `$ARGUMENTS` stand.
    #[serde(default)]
    pub arguments: String,
    /// The project folder the command is started in: the steps run there, the goal names it, and the pack of its project (when trusted) adds its commands.
    #[serde(default)]
    pub folder: Option<String>,
    /// Run it in the background, as `POST /api/v1/mcp/agent` does.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub use_browser: Option<bool>,
    #[serde(default)]
    pub use_windows: Option<bool>,
}

/// Who the steps of a command are run as. The steps of a command of the person's own (`data/commands`, a skill of theirs) are the person's, so they are the caller of the route's token:
/// the person at the app (the UI token) is not asked about what their own command runs under the ordinary rules, and a script (the API token) is asked as ever. The steps of a command that
/// comes from a repository (its `.aqua` pack, trusted only by the hash of its files) are a stranger's text, which the person has not read as a program: they run as the API token's caller, whichever token
/// started the command, so that every one that changes anything asks every time and full access does not apply (`tool_host::policy`), as for a script. (Under Developer mode or full access a program
/// that is not an interpreter would otherwise have run unasked with the person's authority.)
pub fn step_caller(scope: Scope, source: crate::commands::Source) -> aqua_tool_host::Caller {
    if scope == Scope::Ui && !source.is_from_a_repository() {
        aqua_tool_host::Caller::Ui
    } else {
        aqua_tool_host::Caller::Api
    }
}

/// The steps of a command, as calls of the tool host made by the caller of the command: each asks like any command, is masked and is written in the audit ledger under the command's own run.
struct HostSteps {
    host: std::sync::Arc<aqua_tool_host::ToolHost>,
    caller: aqua_tool_host::Caller,
    run: String,
    cwd: Option<String>,
}

impl crate::commands::StepRunner for HostSteps {
    fn run(&mut self, _index: usize, command: &str) -> Result<String, String> {
        let (tool, args) = crate::commands::step_call(command, self.cwd.as_deref())?;
        crate::agent_tools::call_step(&self.host, tool, args, self.caller, &self.run)
    }
}

/// `POST /api/v1/commands/:name/run {arguments, folder?, background?}` (card OA11-02, task EX-08): starts a custom command. Its words are put in, its steps are run through the tool host (each asks like
/// any command; a refusal ends the command and says which step), and then the agent is started with the command's goal, only the tools the command allows and the packs it names, through the same
/// entry as `POST /api/v1/mcp/agent`. Nothing is run while a command is only listed or expanded. Off unless `AQUA_COMMANDS=on`.
async fn commands_run_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(body): Json<CommandRunRequest>,
) -> Response {
    if !crate::flags::is_on(&crate::flags::COMMANDS) {
        return (axum::http::StatusCode::CONFLICT, Json(serde_json::json!({ "error": "custom commands are off: set AQUA_COMMANDS=on and restart Aqua to use them", "enabled": false }))).into_response();
    }
    let (commands, folder) = match super::super::route_api::commands_for(body.folder.as_deref()) {
        Ok(found) => found,
        Err(r) => return r,
    };
    let Some(cmd) = commands.get(&name).cloned() else {
        return api_error(axum::http::StatusCode::NOT_FOUND, &format!("there is no command called {name}"));
    };
    // The agent the command runs as (card OA11-04, its `agent:` key): refused before any step is run, because a command that names an agent means to be held to it, and one that ran without
    // it (agents are off) would have more powers than its author gave it.
    let agent = match requested_agent(cmd.agent.as_deref()) {
        Ok(a) => a,
        Err(refused) => return refused,
    };
    let caller = step_caller(scope, cmd.source);
    // (A real path from `canonicalize` has the extended-length prefix of Windows in front, which the terminal's folder guard reads as a network path: it is taken off.)
    let folder_text = folder.as_ref().map(|f| aqua_native_tools::term::custom::plain_path(f));
    let host = state.tools.clone();
    let (words, cwd) = (body.arguments.clone(), folder_text.clone());
    let prepared = tokio::task::spawn_blocking(move || {
        // The steps are calls of a run of their own, so that the ledger can say what the command asked of the host before the agent began.
        let run = crate::agent_runs::scope::RunScope::begin(Some(host.clone()));
        run.describe("command", caller.as_str());
        let mut steps = HostSteps { host: host.clone(), caller, run: run.id().to_string(), cwd: cwd.clone() };
        crate::commands::prepare(&cmd, &words, &mut steps, cwd.as_deref())
    })
    .await;
    let prepared = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(why)) => return (axum::http::StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({ "error": why, "command": name }))).into_response(),
        Err(e) => return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("the command could not be prepared: {e}")),
    };
    // A command that narrows the run has no use for the browser or the desktop unless the person asks for them.
    let narrowed = !prepared.allowed_tools.is_empty();
    let request = UnifiedMcpAgentRequest {
        goal: prepared.goal,
        max_steps: body.max_steps.unwrap_or_else(default_agent_max_steps),
        use_browser: body.use_browser.unwrap_or(!narrowed),
        use_windows: body.use_windows.unwrap_or(!narrowed),
        force_agent: true,
        background: body.background,
        idempotency_key: None,
        profile: None,
        allowed_tools: prepared.allowed_tools,
        forced_packs: prepared.forced_packs,
        tainted_start: prepared.steps_run > 0,
        session_id: None,
        agent,
    };
    mcp_unified_agent_endpoint(State(state), axum::Extension(scope), Json(request)).await.into_response()
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/mcp/agent", post(mcp_unified_agent_endpoint))
        .route("/api/v1/commands/:name/run", post(commands_run_endpoint))
}
