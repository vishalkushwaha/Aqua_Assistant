//! One run of the tool agent for a turn, the question it may leave and the run that goes on from the answer, and what is said when it gave no usable answer (move only: this was part of `pipeline.rs`).

use super::*;

/// One run of the tool agent for `prompt`, and what is said back. `said` is what the person actually said (the earlier turns are chosen from
/// it). `resume` is set when the run goes on after a question: the earlier run's steps, and what was asked and answered.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_agent_turn(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    prompt: &str,
    said: &str,
    surface_line: Option<&str>,
    laya: Option<&Arc<LayaGate>>,
    resume: Option<(crate::modules::browser_agent::Resume, String)>,
    caller: WindowsCaller,
) -> Result<String> {
    let run = |goal: &str, rounds: u32| run_mcp_agent_shared(llm, browser.clone(), windows.clone(), goal, rounds, surface_line);
    run_agent_turn_with(prompt, said, laya, resume, caller, &run)
}

/// What runs the agent for a turn: the goal and the number of rounds it may take. The model is what a test cannot have, so a test gives its own.
pub(crate) type RunAgent<'a> = &'a dyn Fn(&str, u32) -> Result<crate::modules::browser_agent::McpAgentResult>;

/// [`run_agent_turn`] with what runs the agent given: everything that decides how the run is made (what it is told of the turns before, the caller and so the rounds, what
/// it carries over from a run it goes on from, what is said back, the question it leaves) is this function.
pub(super) fn run_agent_turn_with(prompt: &str, said: &str, laya: Option<&Arc<LayaGate>>, resume: Option<(crate::modules::browser_agent::Resume, String)>, caller: WindowsCaller, run: RunAgent<'_>) -> Result<String> {
    let (resume, earlier) = match resume {
        Some((r, block)) => (Some(r), Some(block)),
        None => (None, crate::conversation::context_for_agent(said)),
    };
    let asked_before = resume.as_ref().map_or(0, |r| r.questions_asked);
    // The orb's own turns are the voice (six rounds, 180 s, nothing that changes anything). A run that goes on from another one has the powers that one had.
    let rounds = if caller == WindowsCaller::Voice { 6 } else { crate::modules::browser_agent::AGENT_MAX_ROUNDS };
    let outcome = crate::modules::browser_agent::with_agent_context(earlier, || {
        crate::modules::browser_agent::with_agent_resume(resume, || with_windows_caller(caller, || crate::turn_timing::stage("agent", || run(prompt, rounds))))
    });
    match outcome {
        Ok(result) => {
            info!(
                "MCP agent finished reason={} tools={} answer_chars={}",
                result.stopped_reason,
                result.tool_rounds,
                result.final_answer.len()
            );
            // The planner asked the person something: the next thing they say is the answer (unless it is a stop or a new request).
            if let (Some(question), "waiting_user") = (result.question.as_ref(), result.stopped_reason.as_str()) {
                crate::conversation::set_pending(crate::conversation::Pending {
                    // A run that could not be recorded has no record id; what it held is looked up by the id it had in this process.
                    run_id: result.run_id.clone().or_else(crate::agent_runs::scope::run_that_just_ended).unwrap_or_default(),
                    question: question.clone(),
                    choices: result.choices.clone(),
                    goal: prompt.to_string(),
                    asked: asked_before + 1,
                    caller: crate::modules::browser_agent::windows_caller_name(caller),
                    expires: std::time::Instant::now() + crate::conversation::IDLE,
                });
                return Ok(spoken_question(question, &result.choices));
            }
            // Reinforce mcp_agent only when tools actually ran (confirmed computer-use).
            if let Some(g) = laya {
                if result.tool_rounds > 0 {
                    g.feedback_route(prompt, GateRoute::McpAgent);
                }
            }
            let answer = strip_reasoning_tags(&result.final_answer);
            if is_usable_answer(&answer) {
                if result.tool_rounds > 0 {
                    let target = crate::modules::browser_agent::plain_open_app_request(prompt).unwrap_or_else(|| "that".into());
                    return Ok(crate::follow_up::after_step(&answer, &target, crate::follow_up::kind_of(prompt)));
                }
                return Ok(answer);
            }
            // The agent stopped without an answer. Falling back to chat would let the model make one up (and
            // claim a task was done), so say what happened instead.
            warn!("MCP agent gave no usable answer (reason={}, tools={})", result.stopped_reason, result.tool_rounds);
            Ok(agent_gave_up(result.tool_rounds))
        }
        Err(e) => {
            warn!("MCP agent failed: {e:#}");
            Ok(agent_gave_up(0))
        }
    }
}

/// The question as it is said aloud: with its choices, when it offered some that the question does not already name.
pub(super) fn spoken_question(question: &str, choices: &[String]) -> String {
    let lower = question.to_lowercase();
    if choices.is_empty() || choices.iter().all(|c| lower.contains(&c.to_lowercase())) {
        return question.to_string();
    }
    format!("{} {}?", question.trim_end_matches(['?', '.', ' ']).to_string() + ":", choices.join(", or "))
}

/// The next thing said while a question is waiting. A stop ends it; something that is clearly a new request leaves it behind and is
/// handled as one; anything else is the answer, and the run that asked goes on from where it was. `Ok(None)`: not an answer.
pub(super) fn answer_pending_question(
    llm: &LlamaRouterEngine,
    browser: &Option<Arc<BrowserBackend>>,
    windows: &Option<Arc<Mutex<StdioMcpClient>>>,
    said: &str,
    heard: Heard<'_>,
    laya: Option<&Arc<LayaGate>>,
) -> Result<Option<String>> {
    let Some(pending) = crate::conversation::pending() else { return Ok(None) };
    if crate::voice_commands::is_stop_command(said, heard.wake_phrase()) {
        crate::conversation::take_pending();
        note_route("none");
        return Ok(Some("Okay, I won't go on with that.".to_string()));
    }
    // What is said while a question waits still has to pass the rules that refuse known attack wording, and a command of its own is a new
    // request: both leave the question behind.
    let attack = crate::laya_validator::attack_phrase(said).is_some() || crate::laya_validator::harmful_intent(said).is_some();
    if attack || !crate::conversation::is_answer_to(&pending, said) || (browser.is_none() && windows.is_none()) {
        crate::conversation::take_pending();
        return Ok(None);
    }
    crate::conversation::take_pending();
    note_route("agent");
    go_on_after_answer(&pending, said, laya, &|goal, rounds| run_mcp_agent_shared(llm, browser.clone(), windows.clone(), goal, rounds, None)).map(Some)
}

/// The run that asked a question goes on with the person's answer, as a run of the caller that asked: a resumed run that was an agent's or the API's does not lose its powers because
/// the answer came by voice (it ran as the voice before, which fails closed but could not finish the task).
pub(super) fn go_on_after_answer(pending: &crate::conversation::Pending, said: &str, laya: Option<&Arc<LayaGate>>, run: RunAgent<'_>) -> Result<String> {
    // The answer is data for the planner (one line, secrets masked): what was asked, what was said, and that the task goes on.
    let block = crate::agent_runs::answer::context_block(&pending.question, said);
    let resume = crate::modules::browser_agent::Resume {
        parent_run_id: pending.run_id.clone(),
        history: crate::agent_runs::resume_history(&pending.run_id, 6),
        questions_asked: pending.asked,
        unknown_digests: Vec::new(),
        ..Default::default()
    };
    run_agent_turn_with(&pending.goal, said, laya, Some((resume, block)), crate::modules::browser_agent::windows_caller_named(pending.caller), run)
}

/// The run that asked a question goes on with the answer the person gave to `POST /api/v1/runs/{id}/answer` (card OA10-03, AG-03): the same task, started again as a new run
/// (`parent_run_id` is the one that asked) as the caller that asked, with the calls that came back and the question and the answer as data. It is not [`run_agent_turn_with`]: that one
/// leaves a question this run asks next waiting in the person's conversation, where the next thing they say to the orb would be taken as its answer; the client of the route is
/// told the question in the answer instead, and answers it at the route again.
pub fn run_answered_agent(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    plan: &crate::agent_runs::answer::Plan,
    store: &crate::agent_runs::AgentRunStore,
) -> Result<crate::modules::browser_agent::McpAgentResult> {
    run_answered_agent_with(plan, store, &|goal, rounds| run_mcp_agent_shared(llm, browser.clone(), windows.clone(), goal, rounds, None))
}

/// [`run_answered_agent`] with what runs the agent given (see [`RunAgent`]).
pub(crate) fn run_answered_agent_with(plan: &crate::agent_runs::answer::Plan, store: &crate::agent_runs::AgentRunStore, run: RunAgent<'_>) -> Result<crate::modules::browser_agent::McpAgentResult> {
    note_route("agent");
    let caller = crate::modules::browser_agent::windows_caller_named(&plan.caller);
    let (resume, block) = plan.resume_in(store);
    // The orb's own turns are the voice (six rounds); every other caller asks for as many as a run may have, and the loop gives the run its own cap (12, or 24 for a coding run).
    let rounds = if caller == WindowsCaller::Voice { 6 } else { crate::modules::browser_agent::CODING_MAX_ROUNDS };
    crate::modules::browser_agent::with_agent_context(Some(block), || {
        crate::modules::browser_agent::with_agent_resume(Some(resume), || with_windows_caller(caller, || crate::turn_timing::stage("agent", || run(&plan.goal, rounds))))
    })
}

/// A run that stopped in the middle goes on from its record (`POST /api/v1/runs/{id}/resume`): the same task, started again as a new run that has
/// the calls that came back, what the stopped run held, and what is not known about the calls that were running when it stopped (see
/// `agent_runs::resume`). What is said back is the answer, as for any run.
pub fn run_resumed_agent(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    plan: &crate::agent_runs::resume::Plan,
    laya: Option<&Arc<LayaGate>>,
) -> Result<String> {
    run_resumed_agent_with(plan, laya, &|goal, rounds| run_mcp_agent_shared(llm, browser.clone(), windows.clone(), goal, rounds, None))
}

/// [`run_resumed_agent`] with what runs the agent given (see [`RunAgent`]).
pub(crate) fn run_resumed_agent_with(plan: &crate::agent_runs::resume::Plan, laya: Option<&Arc<LayaGate>>, run: RunAgent<'_>) -> Result<String> {
    note_route("agent");
    let resume = crate::modules::browser_agent::Resume {
        parent_run_id: plan.parent_run_id.clone(),
        history: plan.history.clone(),
        questions_asked: 0,
        unknown_digests: crate::agent_runs::resume::unknown_digests(plan),
        // What the stopped run kept of its checklist and of what the person added to it goes on with the run (card OA10-04, AG-15).
        checklist: plan.checklist.clone(),
        steered: plan.steered.clone(),
        ..Default::default()
    };
    run_agent_turn_with(&plan.goal, &plan.goal, laya, Some((resume, crate::agent_runs::resume::context_block(plan))), crate::modules::browser_agent::windows_caller_named(&plan.caller), run)
}

/// An agent answer that is really something else: empty, or the planner's own half-written JSON.
pub(super) fn is_usable_answer(answer: &str) -> bool {
    let a = answer.trim();
    if a.is_empty() {
        return false;
    }
    let looks_like_plan = a.starts_with('{') && (a.contains("\"final\"") || a.contains("\"tool\"") || a.contains("\"action\""));
    !looks_like_plan
}

pub(super) fn agent_gave_up(tool_rounds: u32) -> String {
    if tool_rounds > 0 {
        "I started on that but could not finish it, so I can't tell you it was done. Please check, or ask me again.".to_string()
    } else {
        "I could not do that. Nothing was changed.".to_string()
    }
}
