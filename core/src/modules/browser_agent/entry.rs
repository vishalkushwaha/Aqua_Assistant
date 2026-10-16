//! The ways into the loop: a run with the locks of the two MCP clients held only while a tool runs, a run with a report of each step, a run with its identity and its record, and the two older names for them (card OA3-05, move only).

use super::*;

/// Preferred entry: Arc locks released while the LLM thinks.
/// `surface_hint` is an optional one-line preference (desktop vs browser), from Laya.
/// `progress` receives each step as it happens (for live UI / SSE).
pub fn run_mcp_agent_shared(
    llm: &LlamaRouterEngine,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
    surface_hint: Option<&str>,
) -> Result<McpAgentResult> {
    run_mcp_agent_shared_with_progress(llm, browser, windows, goal, max_steps, surface_hint, None)
}

pub fn run_mcp_agent_shared_with_progress(
    llm: &LlamaRouterEngine,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
    surface_hint: Option<&str>,
    progress: Option<std::sync::mpsc::Sender<AgentStep>>,
) -> Result<McpAgentResult> {
    let goal = goal.trim();
    if goal.is_empty() {
        bail!("empty agent goal");
    }
    // A request by voice, or typed at the orb, may not click, type or submit (the guard refuses those calls). One that is plainly about
    // that is answered now: planning it took 1 to 4 rounds of the model (123 to 234 seconds in the log) before the refusal came.
    if windows_caller_now() == WindowsCaller::Voice && crate::spoken_request::needs_clicks_or_typing(goal) {
        info!("voice request that needs a click or typing, answered without planning");
        return Ok(McpAgentResult { final_answer: VOICE_CANNOT_ACT.to_string(), steps: vec![], tool_rounds: 0, stopped_reason: "voice_cannot_act".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }
    // A plain weather question is already answered by the weather service. The admin panel calls this agent directly, so without
    // this check it plans a browser search (minutes) even though the same question on the voice path is instant.
    if let Some(answer) = crate::live_weather::handle_request(goal) {
        info!("weather answered without the agent");
        return Ok(McpAgentResult { final_answer: answer, steps: vec![], tool_rounds: 0, stopped_reason: "weather".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }
    // Memory wraps the run: relevant experience goes into the prompt, the outcome is verified from
    // the run's own evidence and queued for learning. Any failure in there is contained.
    // What kind of task this is, is decided here once (card OA4-01): the experience that is looked for, the loop and the check of the answer all read the one profile.
    super::super::run_profile::with_requested_profile(super::super::run_profile::requested().or_else(|| inherited_profile_in(crate::agent_runs::store().map(|s| s.as_ref()))), || {
        super::super::run_profile::scope(goal, || {
            super::super::agent_experience::run_with_experience(goal, |exp| {
                run_mcp_agent_core(llm, browser.clone(), windows.clone(), goal, max_steps, surface_hint, progress.clone(), exp)
            })
        })
    })
}

/// One agent run, from its identity to its record. The loop itself is [`run_mcp_agent_inner`].
#[allow(clippy::too_many_arguments)]
fn run_mcp_agent_core(
    llm: &LlamaRouterEngine,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
    surface_hint: Option<&str>,
    progress: Option<std::sync::mpsc::Sender<AgentStep>>,
    exp: &mut super::super::agent_experience::ExperienceUse,
) -> Result<McpAgentResult> {
    run_mcp_agent_core_with(&crate::modules::planner::LlamaPlanner(llm), &AgentLearning::from_workspace(), crate::agent_runs::store().cloned(), browser, windows, goal, max_steps, surface_hint, progress, exp)
}

/// [`run_mcp_agent_core`] with what it uses of the outside world given: the planner (the model), the place the run learns in, and the run record. The core gives the
/// model, the workspace and its own record; a test gives a scripted planner and a record of its own, and everything else is the production path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_mcp_agent_core_with(
    planner: &dyn crate::modules::planner::Planner,
    learning: &AgentLearning,
    store: Option<Arc<crate::agent_runs::AgentRunStore>>,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
    surface_hint: Option<&str>,
    progress: Option<std::sync::mpsc::Sender<AgentStep>>,
    exp: &mut super::super::agent_experience::ExperienceUse,
) -> Result<McpAgentResult> {
    let goal = goal.trim();
    if goal.is_empty() {
        bail!("empty agent goal");
    }
    // A browser named in the request ("in Chrome go to Facebook") is the one that is driven. When it is not installed the run says so and uses no other.
    if let (Some(b), Some(channel)) = (&browser, aqua_mcp_adapters::browser::requested_channel(goal)) {
        match b.use_channel(channel) {
            Ok(true) => info!("the request names {}: the browser is now {}", channel.as_arg(), channel.label()),
            Ok(false) => {}
            Err(e) => {
                return Ok(McpAgentResult {
                    final_answer: format!("{e:#}, so I did not use another browser. Install it, or tell me to use the browser that is installed."),
                    steps: Vec::new(),
                    tool_rounds: 0,
                    stopped_reason: "browser_unavailable".into(),
                    gate: None,
                    run_id: None,
                    question: None,
                    choices: Vec::new(),
                    sources: Vec::new(),
                });
            }
        }
    }
    // Each run has its own identity in the tool host: taint (untrusted output seen) is per run and is
    // forgotten when the run ends, however it ends.
    let run = crate::agent_runs::scope::RunScope::begin(crate::agent_tools::host().cloned());
    // The project this run works in, found from the goal; with `AQUA_PROJECT_PACK=on` its `.aqua` pack is read now, once, and what the person trusts of it is the pack of this run (card OA11-01).
    crate::project_context::begin_run(run.id(), goal, &project_roots(), &crate::runtime::data_dir(), &|| crate::tool_host::pack_trust_key(false));
    // A reload of the config files that was asked for while a run was going on is done now that this one is the only run (card OA11-03), and the MCP servers of this project's pack (the ones the
    // person trusted as code) are listed, as are the other projects' that have some (a pack that is not trusted any more has none).
    crate::config_reload::at_run_start();
    crate::mcp_exposure::run_started(run.id());
    let caller = windows_caller_name(windows_caller_now());
    // The record is written as the loop goes (a tool call before the tool runs), and closed here whatever way the loop ends.
    let parent = with_resume(|r| r.map(|r| r.parent_run_id.clone())).filter(|p| !p.is_empty());
    let inherited = inherited_profile_in(store.as_deref());
    take_from_parent_run_in(run.id(), store.as_deref());
    let route = if crate::queue::in_background() { "background" } else { "agent" };
    run.describe(route, caller);
    // The agent the request asked this run to be (card OA11-04, `AQUA_AGENTS`): the host refuses the tools its definition does not name, and its words, packs and limits are the run's. A name that is
    // no agent ends the run here, before anything is recorded, with the names there are.
    if let Err(why) = child::bind_primary(run.id()) {
        return Ok(McpAgentResult { final_answer: why, steps: Vec::new(), tool_rounds: 0, stopped_reason: "unknown_agent".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }
    // The person's hooks for the start of a run (`UserPromptSubmit`, `RunStart`): a refusal ends it here, before anything is recorded or asked of the model.
    if let Some(refusal) = crate::agent_tools::hooks_at_run_start(run.id(), goal, route, caller) {
        return Ok(McpAgentResult { final_answer: refusal, steps: Vec::new(), tool_rounds: 0, stopped_reason: "denied_by_hook".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }
    let store_for_session = store.clone();
    let recording = store.and_then(|s| crate::agent_runs::Recording::begin_in(s, run.id(), goal, caller, route, parent.as_deref()));
    // What the run starts with (the marks it took from the one it goes on from) is written down at once, not only when its first call ends.
    crate::agent_runs::note_marks();
    // What the run reads on the web is kept as numbered sources for its answer to cite and for the program to check it against.
    crate::sources::begin(run.id(), goal);
    // The first run of a session that was forked from a run of another is told what that run was asked and answered (card OA10-04, UX-10): data, after the rules, with the conversation.
    let context_before = agent_context();
    if let (Some(_), Some(s)) = (&recording, &store_for_session) {
        if let Some(fork) = crate::agent_runs::current_session_id().and_then(|session| s.fork_context(&session)) {
            set_agent_context(Some(match &context_before { Some(before) => format!("{before}\n{fork}"), None => fork }));
        }
    }
    // The profile of the run is decided here when no entry decided it before (a test, a caller of this function), and written to the record: a run that goes on from this one has it.
    let mut out = crate::modules::run_profile::with_requested_profile(crate::modules::run_profile::requested().or(inherited), || {
        crate::modules::run_profile::scope(goal, || {
            crate::agent_runs::note_profile(crate::modules::run_profile::profile_of(goal).as_str());
            // The coding session of the project (card OA10-04, `AQUA_CODING_SESSIONS`): what earlier runs of it came to is told to this one.
            if let (Some(_), Some(s)) = (&recording, &store_for_session) {
                session_run::attach(s, run.id(), goal);
            }
            run_mcp_agent_inner(planner, learning, browser, windows, goal, max_steps, surface_hint, progress, exp)
        })
    });
    set_agent_context(context_before);
    crate::sources::end(run.id());
    // A plan run that ended with its answer ended with a plan: the person is to say whether it is built (card OA10-04, AG-06).
    if plan_run() {
        if let Ok(r) = out.as_mut() {
            if r.stopped_reason == "final" && recording.is_some() {
                r.stopped_reason = "plan_ready".into();
                crate::agent_runs::note_plan_ready(&r.final_answer);
            }
        }
    }
    if let Some(rec) = recording {
        let id = rec.run_id().to_string();
        match &mut out {
            Ok(r) => {
                r.run_id = Some(id);
                // The planner asked the person something and the run waits for the answer: the log says what (for the UI token's reader only).
                if let (Some(question), "waiting_user") = (r.question.as_deref(), r.stopped_reason.as_str()) {
                    crate::agent_runs::note_question(question, &r.choices);
                }
                rec.finish(&r.stopped_reason, r.tool_rounds, &r.final_answer, &r.steps);
                // What the run came to is joined into the record of its coding session (only a coding run that has one).
                session_run::finish(goal, r.run_id.as_deref().unwrap_or(""), r);
                crate::hooks::stop(r.run_id.as_deref().unwrap_or(""), &r.stopped_reason);
                // A run that went well can leave a draft of a skill for the person to read, when they switched that on (`AQUA_SKILL_PROPOSALS=on`).
                crate::skills_inbox::propose_after_run_in_background(r.run_id.as_deref().unwrap_or(""));
            }
            Err(e) => {
                session_run::detach();
                rec.finish(&format!("error: {e:#}"), 0, "", &[]);
                // A run that ends in an error ends too: the `Stop` hooks are told (card OA11-01).
                crate::hooks::stop(&id, &format!("error: {e:#}"));
            }
        }
    }
    out
}

/// Compatibility wrapper — prefer `run_mcp_agent_shared`.
pub fn run_mcp_agent(
    llm: &LlamaRouterEngine,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
) -> Result<McpAgentResult> {
    run_mcp_agent_shared(llm, browser, windows, goal, max_steps, None)
}

pub fn run_browser_agent(
    llm: &LlamaRouterEngine,
    browser: BrowserHandle,
    goal: &str,
    max_steps: u32,
) -> Result<McpAgentResult> {
    run_mcp_agent_shared(llm, Some(browser), None, goal, max_steps, None)
}
