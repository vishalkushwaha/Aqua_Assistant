//! The loop itself: `run_mcp_agent_inner`, one function that asks the planner for a step, calls the tool, reads what came back and decides whether the run may end, with the limits on cut-off replies it uses. The function is moved whole (card OA3-05 cut what was around it, and left the function for a later step).

use super::*;

/// What a coding run is told when the model's reply was cut off at the token limit (a reply is at most 640 tokens, 1,200 after one more try): the way to a long file.
const REPLY_TOO_LONG: &str = "ERROR [REPLY_TOO_LONG] your last reply was cut off at the token limit. Write at most 40 lines per call: fs.write mode=create for the first part, then mode=append for the rest.";

/// What the answer of a coding run says when the model's reply was cut off twice in a row.
const CUT_OFF_ENDED_THE_RUN: &str = "The model's reply was cut off at the token limit twice in a row, so the run stopped. A long file is written in parts of about 40 lines (fs.write for the first, then mode=append for each part).";

/// How many cut-off replies of the planner a coding run is told about (the same words are not given twice in a row).
const MAX_CUT_OFFS_PER_RUN: u32 = 2;

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_mcp_agent_inner(
    planner: &dyn crate::modules::planner::Planner,
    learning: &AgentLearning,
    browser: Option<BrowserHandle>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    goal: &str,
    max_steps: u32,
    surface_hint: Option<&str>,
    progress: Option<std::sync::mpsc::Sender<AgentStep>>,
    exp: &mut super::super::agent_experience::ExperienceUse,
) -> Result<McpAgentResult> {
    let goal = goal.trim();

    // A closed progress channel means the client (SSE stream) went away: nobody is waiting for
    // this run any more, so it must stop instead of driving the desktop unattended.
    let listener_gone = std::cell::Cell::new(false);
    set_searched_this_run(false);
    reset_checklist();
    // A run that goes on from another has the checklist that one kept (card OA10-04, AG-15): the model keeps it current, as it would its own.
    if let Some(items) = with_resume(|r| r.map(|r| r.checklist.clone())).filter(|items| !items.is_empty()) {
        restore_checklist(&items);
        crate::agent_runs::note_checklist(&items);
    }
    let cancel_generation = CANCEL_GENERATION.load(std::sync::atomic::Ordering::SeqCst);
    let emit = |steps: &mut Vec<AgentStep>, step: AgentStep| {
        // The run record hears the step first (and a streamed voice turn shows it as it happens), so that the copy sent to the agent stream can say where in the run's log it is: a client
        // that is cut off takes the log up again after it (card OA10-01). The step the loop keeps is not marked.
        report_step(&step);
        if let Some(ref tx) = progress {
            let mut sent = step.clone();
            crate::agent_runs::tag_cursor(&mut sent);
            if tx.send(sent).is_err() {
                listener_gone.set(true);
            }
        }
        steps.push(step);
    };

    let mut steps: Vec<AgentStep> = Vec::new();
    let budget = max_steps.clamp(1, max_rounds_for_goal(goal));
    // The agent the run is (card OA11-04) may take fewer rounds than the task would have, never more.
    let budget = child::rounds_cap().map_or(budget, |cap| budget.min(cap).max(1));
    let progress_total = budget.saturating_add(3);

    emit(
        &mut steps,
        phase_event(
            0,
            "receive",
            "Receive goal",
            "completed",
            goal,
            Some("Prepare MCP tools"),
            Some(0),
            Some(progress_total),
        ),
    );
    emit(
        &mut steps,
        phase_event(
            0,
            "prepare",
            "Prepare MCP tools",
            "running",
            "Starting / listing Browser + Windows MCP tools…",
            Some("Plan with llama.cpp"),
            Some(0),
            Some(progress_total),
        ),
    );

    // A plain "open Notepad" is done by the program, as it is for a spoken or typed request: a few milliseconds to a few seconds,
    // instead of starting the browser and desktop tool servers, listing their tools and spending a planning round of the 8B model
    // (about a minute on a CPU) on a one-word decision. The same guards apply: no shells, no administration tools.
    // (A helper agent, card OA11-04, is given a question to work out with its tools, and "where is ..." is a question about code to it, not a search of the computer: it takes neither shortcut.)
    if let Some(reply) = (!crate::agents::in_helper()).then(|| open_app_directly(goal, &windows, windows_caller_now())).flatten() {
        emit(
            &mut steps,
            phase_event(1, "act", "Open the app", "completed", reply.clone(), Some("Done"), Some(progress_total), Some(progress_total)),
        );
        return Ok(McpAgentResult { final_answer: reply, steps, tool_rounds: 0, stopped_reason: "scripted_open_app".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }

    // "where is the Need for Speed folder" is a search of this computer, not a browser task.
    if let Some(reply) = (!crate::agents::in_helper()).then(|| aqua_mcp_adapters::local_files::handle_request(goal)).flatten() {
        emit(
            &mut steps,
            phase_event(1, "act", "Search this computer", "completed", reply.clone(), Some("Done"), Some(progress_total), Some(progress_total)),
        );
        return Ok(McpAgentResult { final_answer: reply, steps, tool_rounds: 0, stopped_reason: "local_search".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
    }

    let learn_ctx = learning.prepare(goal);

    // Browser readiness: managed browser starts its server; the extension backend runs the
    // full Browser MCP contract (detect → setup → start → connect → verify).
    let mut browser_control_ready = false;
    let mut browser_user_action: Option<String> = None;
    if let Some(ref b) = browser {
        let backend_label = match b.kind() {
            BackendKind::Managed => "Managed browser",
            BackendKind::Extension => "Browser MCP (your Chrome)",
        };
        emit(
            &mut steps,
            phase_event(
                0,
                "browser_mcp",
                backend_label,
                "running",
                "Checking the browser…",
                Some("Prepare MCP tools"),
                Some(0),
                Some(progress_total),
            ),
        );
        let readiness = b.ensure_ready();
        browser_control_ready = readiness.ready;
        browser_user_action = readiness.user_action_required.clone();
        let server_up = readiness.ready
            || readiness.extension_health.as_ref().is_some_and(|h| h.server_running);
        emit(
            &mut steps,
            phase_event(
                0,
                "browser_mcp",
                backend_label,
                if server_up { "completed" } else { "failed" },
                readiness.detail.clone(),
                Some(if readiness.ready {
                    "Execute with browser_* tools"
                } else {
                    "Continue with available tools"
                }),
                Some(0),
                Some(progress_total),
            ),
        );
        if let Some(health) = readiness.extension_health.clone() {
            emit(
                &mut steps,
                AgentStep {
                    step: 0,
                    kind: "browser_mcp_health".into(),
                    detail: json!({
                        "id": "browser-mcp-health",
                        "phase": "browser_mcp",
                        "title": "Browser MCP contract",
                        "status": if health.ready { "completed" } else { "failed" },
                        "health": health,
                        "message": health.detail,
                        "next": health.user_action_required.clone().unwrap_or_else(|| "Plan next action".into()),
                    }),
                },
            );
        }
        if !readiness.ready {
            warn!("{backend_label} not ready: {}", readiness.detail);
            // Hard-fail only when no browser server is up AND Windows is also unavailable.
            if !server_up && windows.is_none() {
                let hint = readiness.user_action_required.clone().unwrap_or_default();
                emit(
                    &mut steps,
                    phase_event(
                        0,
                        "prepare",
                        "Prepare MCP tools",
                        "failed",
                        format!("{backend_label} unavailable: {}. {hint}", readiness.detail),
                        None,
                        None,
                        None,
                    ),
                );
                bail!("{backend_label} not ready: {}. {hint}", readiness.detail);
            }
        }
    }
    if let Some(ref w) = windows {
        ensure_started(w, "Windows-MCP")?;
    }

    let mut tools = collect_tools_shared(browser.as_ref(), windows.as_ref(), goal)?;
    if tools.is_empty() {
        emit(
            &mut steps,
            phase_event(
                0,
                "prepare",
                "Prepare MCP tools",
                "failed",
                "No MCP tools available — start Browser MCP and/or Windows-MCP first.",
                None,
                None,
                None,
            ),
        );
        bail!("No MCP tools available — start Browser MCP and/or Windows-MCP first.");
    }

    // If extension is offline, strip browser_* so the planner uses Windows tools until Connect.
    if !browser_control_ready {
        let before = tools.len();
        tools.retain(|t| {
            let src = t.source.as_deref().unwrap_or("");
            src != "browser" && !t.name.to_ascii_lowercase().starts_with("browser_")
        });
        if tools.len() < before {
            info!(
                "Browser MCP not connected — hiding {} browser_* tools (Windows fallback active)",
                before - tools.len()
            );
        }
        if tools.is_empty() {
            let hint = browser_user_action.unwrap_or_else(|| {
                "Connect the Browser MCP Chrome extension, then retry.".into()
            });
            emit(
                &mut steps,
                phase_event(
                    0,
                    "prepare",
                    "Prepare MCP tools",
                    "failed",
                    format!("Browser control unavailable and no Windows tools left. {hint}"),
                    None,
                    None,
                    None,
                ),
            );
            bail!("Browser control unavailable: {hint}");
        }
    }

    emit(
        &mut steps,
        phase_event(
            0,
            "prepare",
            "Prepare MCP tools",
            "completed",
            format!(
                "{} tools ready (browser_control={})",
                tools.len(),
                browser_control_ready
            ),
            Some("Plan with llama.cpp"),
            Some(1),
            Some(progress_total),
        ),
    );

    // Recipes version 2 (AQUA_RECIPES_V2=on): a recipe with placeholders and postconditions is replayed through the same door as every call (a write asks, is audited and is
    // masked as it always is), and when a step does not go as it did the planner takes over from that step with what the replay did so far.
    let mut replayed: Vec<(String, String)> = Vec::new();
    let mut replayed_calls = 0u32;
    if crate::memory::agent_learning::recipes_v2_enabled() && !with_resume(|r| r.is_some()) {
        let stop = || -> Option<&'static str> {
            if listener_gone.get() || turn_cancelled() {
                Some("client_disconnected")
            } else if !crate::queue::in_background() && CANCEL_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != cancel_generation {
                Some("cancelled")
            } else {
                None
            }
        };
        let replayer = recipe_replay::Replayer { learning, ctx: &learn_ctx, tools: &tools, browser: &browser, windows: &windows, goal };
        let outcome = recipe_replay::try_recipe_v2(&replayer, &mut |s| emit(&mut steps, s), &stop);
        match outcome {
            recipe_replay::Replay::Done { final_answer, tool_rounds, stopped_reason } => {
                emit(
                    &mut steps,
                    phase_event(0, "plan", "Learned fast path", "completed", "Replaying a prior successful recipe", Some("Done"), Some(progress_total), Some(progress_total)),
                );
                return Ok(McpAgentResult { final_answer, steps, tool_rounds, stopped_reason, gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
            }
            recipe_replay::Replay::Handover { history, calls } => {
                emit(
                    &mut steps,
                    phase_event(0, "plan", "Learned recipe", "completed", format!("A saved recipe ran {calls} step(s) and stopped; the planner goes on from there"), Some("Plan with llama.cpp"), Some(1), Some(progress_total)),
                );
                replayed = history;
                replayed_calls = calls;
            }
            recipe_replay::Replay::Nothing => {}
        }
    }
    // Replay a prior successful recipe when similarity is high enough (general memory, not task scripts).
    if let Some(fast) = try_learned_fast_path(&learning, &learn_ctx, &tools, &browser, &windows, goal) {
        emit(
            &mut steps,
            phase_event(
                0,
                "plan",
                "Learned fast path",
                "completed",
                "Replaying a prior successful recipe",
                Some("Done"),
                Some(progress_total),
                Some(progress_total),
            ),
        );
        for s in &fast.steps {
            if let Some(ref tx) = progress {
                let _ = tx.send(s.clone());
            }
        }
        let mut merged = steps;
        merged.extend(fast.steps);
        return Ok(McpAgentResult {
            final_answer: fast.final_answer,
            steps: merged,
            tool_rounds: fast.tool_rounds,
            stopped_reason: fast.stopped_reason,
            gate: fast.gate, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
        });
    }

    let surface_block = surface_hint
        .map(|h| format!("\nSURFACE HINT: {h}\n"))
        .unwrap_or_default();

    // Verified experience from earlier tasks (compact, marked as untrusted notes, never orders) and
    // what earlier attempts of this same task taught. Empty when memory is off or has nothing.
    let experience_block = exp.prepare_block(goal, &tools);
    if !experience_block.is_empty() {
        emit(
            &mut steps,
            phase_event(
                0,
                "memory",
                "Relevant experience",
                "completed",
                format!("{} earlier experience(s) may apply; checking them against this task", exp.used.len()),
                Some("Plan with llama.cpp"),
                Some(1),
                Some(progress_total),
            ),
        );
    }

    // The prompt starts with the rules and the list of tools, which are the same from one task to the next, so that the model
    // keeps them in its cache. The order of the list must not follow what was learned (that changes after almost every run).
    let shown_tools = in_prompt_order(tools_shown_to_planner(goal, &tools));
    // Beyond one call or an answer, the model is told about the forms it may use (chaining steps, asking) only when they are switched on.
    // (With `AQUA_PREFIX_STABLE=on` a coding goal's own file, folder and check come last, after those forms, so that they are all that differs from one coding goal to the next.)
    let planner_block = format!(
        "{}{}{}",
        planner_prompt_block_with(goal, &shown_tools, coding_prompt_reachable()),
        crate::modules::planner::batch_hint_for(goal),
        planner_task_line(goal, &shown_tools, coding_prompt_reachable())
    );
    // The reply can name only a tool that was shown, and give only its arguments. Built once: the shown set does not change in a run.
    let mut constraint = crate::modules::planner::constraint_for_goal(&shown_tools, goal);
    // Tools the agent finds by search (`tools.search`, when it is on) join what it may call from the next round; the start of the prompt does not change.
    let mut found_tools: Vec<McpTool> = Vec::new();
    // Tools that what the person added while the run went on calls for ("also read b.txt" to a task that had no file tool): listed at the end of the prompt too.
    let mut steer_tools: Vec<McpTool> = Vec::new();
    // The plan the model wrote for a task of several steps (`AQUA_PLAN=on`), recited at the end of every prompt after it. Not authority: it only
    // reminds the model what the whole task was.
    // A plan the person approved (card OA10-04, AG-06) is the plan of this run from the start: it is recited at the end of every prompt, and the run's record has it.
    let mut plan: Vec<String> = with_resume(|r| r.map(|r| crate::modules::planner::plan::clean(&r.plan))).unwrap_or_default();
    let wants_plan = crate::modules::coding_session::wants_plan(goal) || !plan.is_empty();
    if !plan.is_empty() {
        crate::agent_runs::note_plan(&plan);
    }
    let mut recalls = 0usize;
    // What was said just before (when this request sounds like it continues it): data after the rules, so the cached start is untouched.
    let conversation_block = agent_context().map(|c| format!("\n{c}")).unwrap_or_default();
    // What the project says about itself, when the person has trusted its folder (and that one line when it has not).
    let project_block = project_notes_for(goal, &shown_tools);
    let brief = if crate::modules::run_profile::coding(goal) {
        // The brief ends with the repository the project folder holds, when the git tools can name one (card OA7-02, TM-05: `Git: proj on main, 3 changed files`).
        // The folder the goal is about (card OA4-05, CU-04); the first root when it names none.
        goal_project_dir(goal)
            .or_else(|| project_roots().first().cloned())
            .map(|r| {
                let r = &r;
                let text = crate::modules::project_brief::without_secrets(&crate::modules::project_brief::brief(r));
                match crate::modules::project_brief::git_line_via_host(r) {
                    Some(git) if !text.is_empty() => format!("{text}\n{git}"),
                    _ => text,
                }
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!("\n{s}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    // What the person asked Aqua to remember, when it fits this task (data about them, never instructions).
    let remembered_block = crate::memory::user_facts::context_block(goal).map(|b| format!("\n{b}")).unwrap_or_default();
    // The user's own skills that fit this goal (a name and one line each; the text comes only through `skill.load`).
    let skills_block = crate::skills::block_for_goal(goal).map(|b| format!("\n{b}")).unwrap_or_default();
    // The blocks that follow the rules and the list of tools, in the order they have always had, each with how soon it goes when the whole is over its room (priority: the higher the
    // number, the sooner; `context::blocks::assemble`, card OA4-02 task CX-07). They are put together in each round, because the room depends on the reply the round may need; the
    // same blocks and room give the same bytes. Every one is data with its own framing, so it is kept whole or left out whole.
    use crate::modules::context::blocks::Block;
    let system_blocks = vec![
        Block::new("surface", surface_block, 1, true),
        // The words of the agent this run was asked to be (card OA11-04): notes after the rules, kept before the other blocks go. Empty for a run that is no agent.
        Block::new("agent", child::primary_block(), 1, true),
        Block::new("conversation", conversation_block, 3, true),
        Block::new("remembered", remembered_block, 4, true),
        Block::new("skills", skills_block, 4, true),
        Block::new("project", project_block, 2, true),
        Block::new("brief", brief, 3, true),
        Block::new("checklist", checklist_block(), 2, true),
        // What the earlier runs of this coding session came to (card OA10-04, `AQUA_CODING_SESSIONS`): at most 600 characters of data, empty unless the switch is on and the run has a session.
        Block::new("session", session_run::block(), 2, true),
        Block::new("separator", "\n", 0, true),
        Block::new("lessons", if learn_ctx.lessons_prompt.is_empty() { String::new() } else { format!("\n{}", learn_ctx.lessons_prompt) }, 5, true),
        Block::new("experience", if experience_block.is_empty() { String::new() } else { format!("\n{experience_block}") }, 5, true),
    ];
    // What was left out of the system part, as it was last told in the log.
    let mut left_out_told: Vec<&'static str> = Vec::new();
    // A run that goes on after a question starts from what the run before it had done, so it does not read the page or the file again.
    let resumed = resumed_from().unwrap_or_default();
    let questions_asked = resumed.questions_asked;
    // How many times a "no" that came with words has let this run go on (card OA10-03, AG-11): at most twice.
    let mut denials_gone_on_from = 0u32;
    let mut history: Vec<(String, String)> = resumed.history;
    // What a saved recipe (version 2) did before it stopped is what has been done: the planner goes on from there.
    history.extend(replayed);
    // The secret of this run's fence markers: the same all through the run, so the prompt only grows at its end.
    set_goal_for_context(goal);
    set_coding_record(None);
    crate::modules::coding_session::reset_reads();
    let fence_key = new_fence_key();
    let mut tool_rounds = replayed_calls;
    let mut failed_tools: Vec<String> = Vec::new();
    let mut browser_recovery_attempted = false;
    let mut max_steps = budget;
    // Whether the run was given its eight more rounds (once, `AQUA_CONTINUE`).
    let mut extended = false;
    let agent_model = planner.model_name();
    info!("MCP agent planner model: {agent_model}");

    let run_started = std::time::Instant::now();
    // A run in the background has no person waiting for it: its clock is longer, and the time it waits for the foreground is not counted.
    let mut run_limit = if crate::queue::in_background() { crate::queue::BACKGROUND_RUN_LIMIT } else { agent_run_limit(windows_caller_now()) };
    // The clock of the agent the run is (card OA11-04), and what is left of the clock of the run that asked for it when it is a helper: the shorter one.
    if let Some(cap) = child::clock_cap() {
        run_limit = run_limit.min(cap);
    }
    // `step` numbers everything the loop does (each planner reply and each call is one); `rounds` counts the planner replies, which are
    // what the budget limits: a call the planner chained into its last reply is not another round.
    let mut step = replayed_calls;
    let mut rounds = 0u32;
    // How many times the model was given up to a person in this run (a background run only, see `llama_server::should_yield`).
    let mut yields = 0u32;
    // Replies of the planner that were cut off at the token limit in a coding run, and whether the last one was: the first is told to the model, a second right after it ends the run (ED-14).
    let mut cut_offs = 0u32;
    let mut cut_off_last = false;
    let mut queued: std::collections::VecDeque<(String, Value)> = std::collections::VecDeque::new();
    let mut last_call: Option<LastCall> = None;
    // What the person added while the run went on (see `steer`): the goal the planner is shown, once something was said. A run that goes on from another has what was added to that one
    // (card OA10-04, AG-15), and keeps it with its own record as more is said.
    let mut steered_lines: Vec<String> = resumed.steered.clone();
    let mut steered_goal: Option<String> = (!steered_lines.is_empty()).then(|| crate::steer::amended_goal(goal, &steered_lines));
    // What the run has been told about going round in circles (once each, see `stuck`).
    let mut stuck_told: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Whether a coding run was told that the same call three times in a row is a loop (card OA4-03, WF-08): the first time it is told, the second time the run ends (or asks the person).
    let mut doom_told = false;
    // What the server did to a prompt that did not fit (card OA4-02, task CX-03): the results are shown in less room from then on, for the rest of the run, so that the older
    // results do not change from one round to the next; and the round in which a refusal was already recovered from (a second one in that round ends the run).
    let mut extra_shrinks = 0u32;
    let mut overflow_recovered_in: Option<u32> = None;
    // Requests for the next step in a row that failed because the model was busy or could not be used (task AG-10); an answer clears it.
    let mut transient_failures = 0u32;
    // The model's answer to the last round of the budget, and the sources it cites, when `AQUA_LAST_STEP_FINAL` held that round to an answer (card OA5-04, TL-10).
    let mut forced_final = None;
    loop {
        step += 1;
        // A call chained after the last one runs without asking the planner again, if the last one left things as they were.
        let batched = match (queued.is_empty(), last_call.take()) {
            (true, _) => None,
            (false, Some(last)) if batch_may_continue(&last) => queued.pop_front(),
            (false, _) => {
                for (tool, _) in queued.drain(..) {
                    history.push((tool, BATCH_NOT_RUN.to_string()));
                }
                None
            }
        };
        if batched.is_none() {
            rounds += 1;
            set_planner_round(rounds);
            if rounds > max_steps {
                // A coding run whose newest check names fewer failures than the one before it is getting somewhere: it is given more rounds, once, and says so (card OA10-04, WF-12, `AQUA_CONTINUE`).
                if !extended && extension_allowed(goal, &steps) {
                    extended = true;
                    max_steps += crate::agent_runs::continuation::EXTENSION_ROUNDS;
                    emit(&mut steps, phase_event(step, "extend", "More rounds", "running", format!("The failing tests are fewer than before: {} more rounds were added to the {} of this run.", crate::agent_runs::continuation::EXTENSION_ROUNDS, max_steps - crate::agent_runs::continuation::EXTENSION_ROUNDS), None, Some(progress_total), Some(progress_total)));
                } else {
                    break;
                }
            }
            // The step boundary before a planner call: a run in the background waits here while the person is being served.
            run_limit += crate::queue::at_step_boundary();
            // What the person said while the run went on is added to the goal, as their own words, for this and every later call.
            if !crate::queue::in_background() {
                let added = crate::steer::take_for(&crate::agent_runs::scope::current_run());
                if !added.is_empty() {
                    let amended = crate::steer::amended_goal(steered_goal.as_deref().unwrap_or(goal), &added);
                    // "Also read b.txt" to a task that had no file tools: the tools of what was added are shown from here on, as they would have been at the start (at the
                    // end of the prompt, so that its start, which the model keeps in its cache, does not change).
                    if !crate::modules::agent_experience::is_practice() {
                        if let Some(host) = crate::agent_tools::host() {
                            let mut changed = false;
                            for t in crate::agent_tools::tools_for_goal(host, &amended) {
                                if !tools.iter().any(|x| x.name == t.name) {
                                    tools.push(t.clone());
                                }
                                let shown = shown_tools.iter().chain(&found_tools).chain(&steer_tools).any(|x| x.name == t.name);
                                if !shown {
                                    steer_tools.push(t);
                                    changed = true;
                                }
                            }
                            if changed {
                                constraint = crate::modules::planner::constraint_for_goal(&[shown_tools.clone(), found_tools.clone(), steer_tools.clone()].concat(), goal);
                            }
                        }
                    }
                    steered_goal = Some(amended);
                    steered_lines.extend(added.iter().cloned());
                    crate::agent_runs::note_steered(&steered_lines);
                    emit(&mut steps, phase_event(step, "steer", "The person added to the task", "completed", added.join(" / "), None, Some(progress_total), Some(progress_total)));
                }
            }
        }
        // From here to the end of the round the task is the one the person has now: the checks of what an answer needs (a look-up, a file, evidence) read what was added too.
        let goal = steered_goal.as_deref().unwrap_or(goal);
        // The mode latch (card OA4-01, task WF-03): a run that began as something else and has now changed a source file or run a check is a coding run from here on: the record, the
        // stuck policy and the guards that come after an edit apply to it. What was decided at the start (the prompt, the tools, the rounds) stays.
        if crate::modules::run_profile::latch_pending() && crate::modules::run_profile::latch_if_code_run(&crate::modules::agent_experience::tool_calls_of(&steps)) {
            info!("the run changed code or ran a check: it is a coding run from step {step}");
            crate::agent_runs::note_profile(crate::modules::run_profile::RunProfile::Coding.as_str());
        }
        // The calls of the run so far, for the coding record of this round's prompt (card OA4-04, WF-06): the record says what the checks said and not what the first error was.
        remember_calls_for_record(goal, &steps);
        // A run has a clock. Each round can take a minute or more of model time, and the single model is shared with
        // every other request, so a run that has used its time says so instead of holding everything up.
        if run_started.elapsed() >= run_limit {
            info!("MCP agent stopped before step {step}: out of time ({}s)", run_limit.as_secs());
            emit(
                &mut steps,
                phase_event(step, "finalize", "Stopped: out of time", "failed", format!("The run used its {} seconds", run_limit.as_secs()), None, Some(progress_total), Some(progress_total)),
            );
            let note = coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Time);
            let where_it_stopped = continuation_note(goal, &steps);
            return Ok(McpAgentResult {
                final_answer: with_stop_note(with_stop_note(ran_out_of_time(tool_rounds, run_limit, windows_caller_now()), note), where_it_stopped),
                steps,
                tool_rounds,
                stopped_reason: "deadline".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
        }
        let hung_up = listener_gone.get() || turn_cancelled();
        // Stop ends the runs the person is waiting for; a run in the background is ended from the queue (its own flag is `hung_up` above).
        let stopped = !crate::queue::in_background() && CANCEL_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != cancel_generation;
        if hung_up || stopped {
            let reason = if hung_up { "client_disconnected" } else { "cancelled" };
            info!("MCP agent stopped before step {step}: {reason}");
            let note = coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Stopped);
            return Ok(McpAgentResult {
                final_answer: with_stop_note("Stopped.".into(), note),
                steps,
                tool_rounds,
                stopped_reason: reason.into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
        }
        // A run that goes round in circles is told once and, when it goes on, stopped: each round is a minute of the person's time and of the one model.
        if batched.is_none() {
            use super::super::stuck::{calls_of, judge, trailing_monologue, Verdict};
            match judge(&calls_of(&steps), trailing_monologue(&history)) {
                Verdict::Stop(s) => {
                    info!("MCP agent stopped before step {step}: going round in circles ({:?}, {} times)", s.pattern, s.times);
                    emit(&mut steps, phase_event(step, "finalize", "Stopped: going round in circles", "failed", s.answer(), None, Some(progress_total), Some(progress_total)));
                    learning.learn_failure(goal, &failed_tools);
                    let note = coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Circles);
                    return Ok(McpAgentResult { final_answer: with_stop_note(s.answer(), note), steps, tool_rounds, stopped_reason: "stuck".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
                }
                Verdict::Nudge(s) => {
                    // The older notes about a repeated call or failure may have said it already.
                    let said_already = history.last().is_some_and(|(t, o)| t == "system" && o.starts_with("NOTE:"));
                    if stuck_told.insert(s.signature()) && !said_already {
                        warn!("{}", s.nudge());
                        history.push(("system".into(), s.nudge()));
                    }
                }
                Verdict::Fine => {}
            }
        }
        let newly_found = crate::agent_runs::scope::take_found(&crate::agent_runs::scope::current_run());
        if !newly_found.is_empty() {
            if let Some(h) = crate::agent_tools::host() {
                let coding_run = crate::agent_tools::is_coding_goal(goal);
                for t in crate::agent_tools::tools_named(h, &newly_found) {
                    // A coding run does not get back, by searching for it, a tool that `coding_driver::DENY` kept out of its list (card OA5-01, TM-08).
                    if coding_run && crate::modules::coding_driver::denied(&t.name) {
                        continue;
                    }
                    if !tools.iter().any(|x| x.name == t.name) {
                        tools.push(t.clone());
                    }
                    if !found_tools.iter().any(|x| x.name == t.name) {
                        found_tools.push(t);
                    }
                }
                constraint = crate::modules::planner::constraint_for_goal(&[shown_tools.clone(), found_tools.clone(), steer_tools.clone()].concat(), goal);
            }
        }
        // The lessons are already in the system prompt (fenced); repeating them here would send them twice per step.
        let round_note = (rounds, max_steps, run_limit.saturating_sub(run_started.elapsed()).as_secs());
        // What is added at the end of the prompt (the tools found by search, the tools for what the person added, the plan) is worked out first, so that the fit leaves room for it.
        let plan_tail = if !wants_plan {
            String::new()
        } else if plan.is_empty() && tool_rounds == 0 {
            crate::modules::planner::plan::ASK.to_string()
        } else if !plan.is_empty() {
            crate::modules::planner::plan::recitation(&plan, rounds, max_steps)
        } else {
            String::new()
        };
        let window = planner.max_context_tokens();
        // The rules and the list of tools, then the blocks, within the room of the window less the reply: the blocks that do not fit go whole, the later and less needed first.
        let assembled = system_assembled(&planner_block, &system_blocks, system_room_chars(window, reply_reserve(goal)));
        if assembled.dropped != left_out_told {
            if !assembled.dropped.is_empty() {
                warn!("the system part of the prompt was over its room in the window of {window} tokens: left out whole, the sooner first: {}", assembled.dropped.join(", "));
            }
            left_out_told = assembled.dropped.clone();
        }
        let system = assembled.text;
        // The checklist `todo.write` keeps goes before the plan, at the end like the rest (empty unless the run wrote one).
        let plan_tail = format!("{}{}{plan_tail}", checklist_tail(), session_run::tail());
        // With `AQUA_PREFIX_STABLE=on` the coding record is the last thing before the closing switch, so that its changes do not make the model read the results again (card OA4-02,
        // task CX-05). It is worked out with the rest of the tail, so that the fit leaves room for it.
        // The last round the budget allows (card OA5-04, TL-10; `AQUA_LAST_STEP_FINAL=on`): the reply can only be an answer and the prompt says so at its end, so that a run that used all its
        // rounds ends with what the model has to say about it and not with a template. Not for a budget of one round (the one round is the work), and not for a step the planner already wrote.
        let last_step = batched.is_none() && max_steps >= 2 && rounds == max_steps && crate::modules::planner::last_step_final_enabled();
        let last_step_line = if last_step { format!("\n{}", crate::modules::planner::LAST_STEP_LINE) } else { String::new() };
        let tail = format!("{}{}{last_step_line}", prompt_tail(&found_tools, &steer_tools, &plan_tail, window), coding_record_tail(goal, &history));
        let (prompt, planner_tokens) = prompt_fitted_to_context_reserving(&system, steered_goal.as_deref().unwrap_or(goal), &history, &fence_key, window, Some(round_note), tail.chars().count(), reply_reserve(goal), extra_shrinks);
        let prompt = with_tail(prompt, &tail);
        if batched.is_none() {
            info!("MCP agent step {step}: prompting LLM ({} chars)", prompt.len());
            emit(
                &mut steps,
                phase_event(
                    step,
                    "plan",
                    format!("Plan with llama.cpp ({agent_model})"),
                    "running",
                    format!("Round {rounds}/{max_steps}: deciding next action for \"{goal}\" using {agent_model}"),
                    Some("Act with a tool or finalize"),
                    Some(step.saturating_add(1)),
                    Some(progress_total),
                ),
            );
        }

        // Clear "open youtube / open gmail" goals: skip llama (was freezing UI on Round 1/10).
        // After compose+send, stop — do not burn 40s/round on chatty "Hello!" LLM replies.
        // If a refocus retry is pending, fall through to scripted_mail_send_action instead.
        // What the model server measured for this round's reply (none when the step was scripted or the server measures nothing).
        let mut plan_timings: Option<crate::modules::llm_router::llama_server::Timings> = None;
        let llm_out = if let Some((tool, arguments)) = batched.as_ref() {
            // The planner already wrote this step in its last reply.
            info!("MCP agent step {step}: chained step → {}", call_shape(tool, arguments));
            emit(
                &mut steps,
                phase_event(step, "plan", "Plan (chained step)", "completed", format!("Next step the planner chained: {tool}"), Some("Act with a tool or finalize"), Some(step.saturating_add(1)), Some(progress_total)),
            );
            json!({ "tool": tool, "arguments": arguments }).to_string()
        } else if is_email_send_goal(goal)
            && email_compose_opened(&history)
            && email_send_focus_fail_count(&history) >= 2
            && !email_send_attempted(&history)
        {
            let email = extract_email_addr(goal).unwrap_or_else(|| "recipient".into());
            let final_answer = format!(
                "Opened Gmail compose to {email}, but could not focus the Gmail window to press Send \
(Ctrl+Enter). Bring the Gmail compose tab to the front (sign in if needed), then ask me to send again. \
Browser MCP extension Connect would also enable reliable browser control."
            );
            emit(
                &mut steps,
                phase_event(
                    step,
                    "finalize",
                    "Finalize answer",
                    "completed",
                    "Compose opened but Gmail window focus/send failed",
                    None,
                    Some(progress_total),
                    Some(progress_total),
                ),
            );
            emit(
                &mut steps,
                AgentStep {
                    step,
                    kind: "final".into(),
                    detail: json!({
                        "id": format!("final-{step}"),
                        "phase": "finalize",
                        "title": "Task incomplete",
                        "status": "completed",
                        "answer": final_answer,
                        "message": "Honest failure: compose opened, send keystroke not delivered to Gmail.",
                        "progress": { "current": progress_total, "total": progress_total },
                    }),
                },
            );
            let result = McpAgentResult {
                final_answer,
                steps,
                tool_rounds,
                stopped_reason: "mail_send_focus_failed".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            };
            learning.learn_failure(goal, &failed_tools);
            return Ok(result);
        } else if is_email_send_goal(goal)
            && email_send_attempted(&history)
            && !email_send_needs_refocus_retry(&history)
        {
            let final_answer = email_send_final_answer(goal, email_send_confirmed(&history));
            info!("MCP agent step {step}: scripted email finalize after send");
            emit(
                &mut steps,
                phase_event(
                    step,
                    "finalize",
                    "Finalize answer",
                    "completed",
                    "Compose opened and Ctrl+Enter/Send attempted — finishing without more llama rounds",
                    None,
                    Some(progress_total),
                    Some(progress_total),
                ),
            );
            emit(
                &mut steps,
                AgentStep {
                    step,
                    kind: "final".into(),
                    detail: json!({
                        "id": format!("final-{step}"),
                        "phase": "finalize",
                        "title": "Task complete",
                        "status": "completed",
                        "answer": final_answer,
                        "message": "Email send path completed (compose + send attempt).",
                        "progress": { "current": progress_total, "total": progress_total },
                    }),
                },
            );
            let result = McpAgentResult {
                final_answer,
                steps,
                tool_rounds,
                stopped_reason: "scripted_mail_send".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            };
            learning.learn_success(
                goal,
                &recipe_from_steps(&result.steps, &result.final_answer),
            );
            return Ok(result);
        } else if let Some((tool, arguments)) =
            scripted_email_compose_action(goal, step, &history, &tools)
        {
            let scripted = json!({ "tool": tool, "arguments": arguments }).to_string();
            info!("MCP agent step {step}: scripted email compose → {}", call_shape(&tool, &arguments));
            emit(
                &mut steps,
                phase_event(
                    step,
                    "plan",
                    "Plan (email compose bootstrap)",
                    "completed",
                    format!("Opened prefilled compose via {tool}; still need Send confirmation"),
                    Some("Observe UI then click Send"),
                    Some(step.saturating_add(1)),
                    Some(progress_total),
                ),
            );
            scripted
        } else if let Some((tool, arguments)) =
            scripted_mail_send_action(goal, step, &history, &tools)
        {
            let scripted = json!({ "tool": tool, "arguments": arguments }).to_string();
            info!("MCP agent step {step}: scripted mail send → {}", call_shape(&tool, &arguments));
            emit(
                &mut steps,
                phase_event(
                    step,
                    "plan",
                    "Plan (send email)",
                    "completed",
                    format!("Compose is open — sending via {tool}"),
                    Some("Observe send confirmation"),
                    Some(step.saturating_add(1)),
                    Some(progress_total),
                ),
            );
            scripted
        } else if let Some((tool, arguments)) =
            scripted_live_search_action(goal, step, &history, &tools)
        {
            let scripted = json!({ "tool": tool, "arguments": arguments }).to_string();
            info!("MCP agent step {step}: scripted live-data search (skip llama) → {}", call_shape(&tool, &arguments));
            emit(
                &mut steps,
                phase_event(
                    step,
                    "plan",
                    "Plan (search the web for current data)",
                    "completed",
                    "Live data: searching Google for today's values instead of answering from memory",
                    Some("Read the results"),
                    Some(step.saturating_add(1)),
                    Some(progress_total),
                ),
            );
            scripted
        } else if let Some((tool, arguments)) =
            scripted_open_action(goal, step, &history, &tools)
        {
            let scripted = json!({ "tool": tool, "arguments": arguments }).to_string();
            let compound = goal_has_follow_on_work(goal);
            info!(
                "MCP agent step {step}: scripted open (skip llama) compound={compound} → {scripted}"
            );
            emit(
                &mut steps,
                phase_event(
                    step,
                    "plan",
                    if compound {
                        "Plan (open then continue)"
                    } else {
                        "Plan (scripted open)"
                    },
                    "completed",
                    if compound {
                        format!(
                            "Bootstrapped navigation; remaining goal steps still require tools ({tool})"
                        )
                    } else {
                        format!("Fast path: open site without llama.cpp ({tool})")
                    },
                    Some(if compound {
                        "Act, then continue full goal"
                    } else {
                        "Act with a tool or finalize"
                    }),
                    Some(step.saturating_add(1)),
                    Some(progress_total),
                ),
            );
            scripted
        } else {
            // The request is planner_block (the rules and the list of tools), then the surface, memory and user turn. Warm the bytes the request really starts
            // with: the whole block, or, when a small window made the list of tools shorter (see `tools_fitted`), the part of it that is still there. Warming only
            // makes the first read faster, so a request that shares too little with the block is not warmed, and a warm-up that fails does not stop the run: the
            // request that follows reads the prompt itself and has its own way of failing (which keeps the steps made so far).
            let warm_prefix = if prompt.starts_with(&planner_block) { planner_block.as_str() } else { common_start(&prompt, &planner_block) };
            let warmed = warm_prefix.chars().count() >= MIN_WARM_CHARS;
            if warmed {
                if let Err(e) = planner.warm_prefix(warm_prefix) {
                    warn!("agent warm-up skipped: {e:#}");
                }
            }
            info!(
                "agent request: prompt_chars={} prefix_matches_warmup={warmed}",
                prompt.chars().count()
            );
            let last_step_constraint = if last_step { crate::modules::planner::constraint_for_last_step(goal) } else { None };
            let request = crate::modules::planner::PlanRequest { prompt: &prompt, max_tokens: planner_tokens, constraint: if last_step { last_step_constraint.as_ref() } else { constraint.as_ref() } };
            // A background run that gave the model up to a person (`AQUA_BACKGROUND_YIELD=on`) asks again for the same round; after `MAX_YIELDS_PER_RUN` times it is not asked to give it up again,
            // so a person who keeps using the model cannot hold a background run back for good.
            // With the model layer's router on (`AQUA_MODEL_ROUTER`) the router asks again by itself for a prompt the server refuses as too long, and this gives it the shorter prompt: the one
            // the recovery below would ask for, one step down in how much of the results is shown, once in a round (card OA9-02, task PR-03). The loop is left in the state that recovery leaves
            // it in (the level, the round it happened in), so a refusal of the shorter prompt is the second one of the round and ends the run as it always did. With the router off the planner
            // does not call this and the recovery below is the only one.
            let shrunk_to: std::cell::Cell<Option<u32>> = std::cell::Cell::new(None);
            let shorter_read: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
            let mut shorter_prompt = |refusal: &crate::model::ProviderError| -> Option<String> {
                let first_of_the_round = overflow_recovered_in != Some(rounds) && shrunk_to.get().is_none();
                if !first_of_the_round || recovery_for(crate::modules::planner::PlanErrorKind::ContextOverflow, false, 0, extra_shrinks) != Recovery::Shrink {
                    return None;
                }
                // What the refusal says the prompt cost is learned first, as the recovery below does, so that the shorter prompt is fitted by the count the server gave.
                learn_from_refusal(&prompt, &refusal.to_string());
                let level = (extra_shrinks + 2).min(MAX_EXTRA_SHRINKS);
                let (fitted, _) = prompt_fitted_to_context_reserving(&system, steered_goal.as_deref().unwrap_or(goal), &history, &fence_key, window, Some(round_note), tail.chars().count(), reply_reserve(goal), level);
                let shorter = with_tail(fitted, &tail);
                if shorter == prompt {
                    // Already as short as the floors allow: nothing more to take out, and the refusal goes back as it came.
                    return None;
                }
                warn!("agent step {step}: the prompt did not fit the window ({refusal}); the model layer asks again with less of the results (level {level})");
                shrunk_to.set(Some(level));
                *shorter_read.borrow_mut() = Some(shorter.clone());
                Some(shorter)
            };
            // Native tool calls (card OA9-06; off unless AQUA_PLANNER_TOOL_MODE or a model's profile says native): the tools this round shows are carried to the planner, which decides whether the model is
            // spoken to that way. With no native mode possible the list is empty and nothing changes. The last round names no tool, so it carries none.
            let native_candidates: Vec<crate::model::native_tools::ToolSpec> = if !last_step && crate::model::native_tools::possible() {
                [shown_tools.clone(), found_tools.clone(), steer_tools.clone()].concat().iter().map(|t| crate::model::native_tools::ToolSpec { name: t.name.clone(), description: t.description.clone(), schema: t.input_schema.clone() }).collect()
            } else {
                Vec::new()
            };
            let mut ask_planner = || {
                crate::model::native_tools::with_candidates(native_candidates.clone(), || planner.plan_shrinking(&request, &mut |secs| {
                    emit(
                        &mut steps,
                        phase_event(
                            step,
                            "plan",
                            format!("Plan with llama.cpp ({agent_model})"),
                            "running",
                            format!(
                                "Still planning… {secs}s on {agent_model}"
                            ),
                            Some("Act with a tool or finalize"),
                            Some(step.saturating_add(1)),
                            Some(progress_total),
                        ),
                    );
                }, &mut shorter_prompt))
            };
            let planned = if yields >= MAX_YIELDS_PER_RUN { crate::modules::llm_router::llama_server::without_yielding(&mut ask_planner) } else { ask_planner() };
            // If the model layer asked again with a shorter prompt, the loop is where its own recovery would have left it: one level down for the rest of the run, and this round marked as recovered
            // (the answer counts as such below, and a refusal of the shorter prompt, which is the second of the round, ends the run).
            if let Some(level) = shrunk_to.get() {
                extra_shrinks = level;
                overflow_recovered_in = Some(rounds);
            }
            // What the server read is the prompt it was last given.
            let shorter = shorter_read.take();
            let prompt_read: &str = shorter.as_deref().unwrap_or(&prompt);
            match planned {
                Ok(reply) => {
                    cut_off_last = false;
                    plan_timings = reply.timings;
                    transient_failures = 0;
                    // The server counted the prompt (what it read and what it took from its cache): the meter learns the cost of a character from it, and the next prompt is fitted by that.
                    if let Some(t) = reply.timings {
                        crate::modules::context::meter::meter().observe(&prompt_read, t.prompt_n + t.cache_n);
                    }
                    // The prompt the server refused as too long, asked again shorter in this round, was read.
                    if overflow_recovered_in.take().is_some_and(|r| r == rounds) {
                        count_context_overflow(true);
                    }
                    reply.text
                }
                Err(e) if crate::modules::llm_router::llama_server::is_yield(&e) => {
                    // Not the person's Stop and not a failure of the model: the call stopped so that a person could have it. Wait until that person has been served (at most two
                    // minutes), then ask for this round again. The prompt the model had read is still in its cache.
                    yields += 1;
                    info!("agent step {step}: the model was given up to a person who was waiting (time {yields} of at most {MAX_YIELDS_PER_RUN} for this run)");
                    crate::modules::llm_router::llama_server::wait_until_no_person_waits(std::time::Duration::from_secs(120), turn_cancelled);
                    run_limit += crate::queue::at_step_boundary();
                    rounds -= 1;
                    continue;
                }
                // A coding reply that was cut off at the token limit is something the model can do better, not the end of the run: it is told to write less in one call (once, and not twice in a row).
                Err(e) if !cut_off_last && cut_offs < MAX_CUT_OFFS_PER_RUN && crate::modules::llm_router::llama_engine::is_cut_off(&e) && crate::modules::run_profile::coding_latched(goal) => {
                    cut_offs += 1;
                    cut_off_last = true;
                    warn!("agent step {step}: the model's reply was cut off at the token limit; it is told to write less per call ({cut_offs} of at most {MAX_CUT_OFFS_PER_RUN} in this run)");
                    history.push(("system".into(), REPLY_TOO_LONG.to_string()));
                    continue;
                }
                Err(e) => {
                    // A prompt the server refused as too long is asked again shorter, once in a round; a model that was busy or could not be used is asked again after a wait, three
                    // times (card OA4-02, tasks CX-03 and AG-10). Neither costs a round, and the person's Stop is never asked again.
                    let kind = crate::modules::planner::PlanErrorKind::of(&e);
                    let recovered_here = overflow_recovered_in == Some(rounds);
                    match recovery_for(kind, recovered_here, transient_failures, extra_shrinks) {
                        Recovery::Shrink => {
                            learn_from_refusal(&prompt, &format!("{e:#}"));
                            extra_shrinks = (extra_shrinks + 2).min(MAX_EXTRA_SHRINKS);
                            overflow_recovered_in = Some(rounds);
                            warn!("agent step {step}: the prompt did not fit the window ({e:#}); asking again with less of the results (level {extra_shrinks})");
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "plan",
                                    format!("Plan with llama.cpp ({agent_model})"),
                                    "running",
                                    "The prompt did not fit the model's window; asking again with less of the earlier results",
                                    Some("Act with a tool or finalize"),
                                    Some(step.saturating_add(1)),
                                    Some(progress_total),
                                ),
                            );
                            rounds -= 1;
                            continue;
                        }
                        Recovery::Wait(ms) => {
                            transient_failures += 1;
                            warn!("agent step {step}: the model could not be asked ({} - {e:#}); asking again in {ms} ms (time {transient_failures} of {})", kind.name(), crate::modules::coding_driver::PLANNER_RETRIES);
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "plan",
                                    format!("Model unavailable, trying again ({transient_failures} of {})", crate::modules::coding_driver::PLANNER_RETRIES),
                                    "running",
                                    format!("{e}"),
                                    Some("Act with a tool or finalize"),
                                    Some(step.saturating_add(1)),
                                    Some(progress_total),
                                ),
                            );
                            // A Stop pressed during the wait ends the wait; the top of the loop then ends the run as a stopped one.
                            let stop_requested = || listener_gone.get() || turn_cancelled() || (!crate::queue::in_background() && CANCEL_GENERATION.load(std::sync::atomic::Ordering::SeqCst) != cancel_generation);
                            wait_unless_stopped(ms, &stop_requested);
                            rounds -= 1;
                            continue;
                        }
                        Recovery::GiveUp => {
                            if kind == crate::modules::planner::PlanErrorKind::ContextOverflow {
                                count_context_overflow(false);
                            }
                        }
                    }
                    warn!("MCP agent LLM step {step} failed: {e:#}");
                    emit(
                        &mut steps,
                        phase_event(
                            step,
                            "plan",
                            format!("Plan with llama.cpp ({agent_model})"),
                            "failed",
                            format!("LLM error ({agent_model}): {e}"),
                            None,
                            Some(step.saturating_add(1)),
                            Some(progress_total),
                        ),
                    );
                    if tool_rounds > 0 || cut_offs > 0 {
                        // A coding run that changed files says which, and what its last check said (card OA4-04, AG-09); the sentence of the loop stands when there is nothing of the sort to say.
                        let why = if is_stop_error(&e) { crate::modules::coding_driver::StopWhy::Stopped } else { crate::modules::coding_driver::StopWhy::Model };
                        let note = coding_stop_note(goal, &steps, why);
                        let unfinished = note.unwrap_or_else(|| synthesize_unfinished(goal, &history));
                        let result = McpAgentResult {
                            final_answer: if crate::modules::llm_router::llama_engine::is_cut_off(&e) { format!("{unfinished}\n{CUT_OFF_ENDED_THE_RUN}") } else { unfinished },
                            steps,
                            tool_rounds,
                            // A Stop pressed while the model was planning ends that call with an error; it is the person's Stop, and is
                            // recorded as one: the experience that was offered neither gains nor loses by a run nobody let finish.
                            stopped_reason: if is_stop_error(&e) { "cancelled".to_string() } else { format!("llm_error: {e}") },
                            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                        };
                        // The model failed mid-run: nothing was verified, so nothing is learned as a success.
                        learning.learn_failure(goal, &failed_tools);
                        return Ok(result);
                    }
                    return Err(e).context("LLM generate for MCP agent");
                }
            }
        };

        emit(
            &mut steps,
            AgentStep {
                step,
                kind: "llm".into(),
                detail: json!({
                    "raw": truncate(&llm_out, 2000),
                    "id": format!("llm-{step}"),
                    "phase": "plan",
                    "title": format!("llama.cpp ({agent_model})"),
                    "status": "completed",
                    "model": agent_model,
                    "timings": plan_timings.map(|t| json!({
                        "prompt_tokens_read": t.prompt_n,
                        "prompt_tokens_cached": t.cache_n,
                        "tokens_written": t.predicted_n,
                        "prompt_ms": t.prompt_ms,
                        "predicted_ms": t.predicted_ms,
                    })),
                    "message": format!("{agent_model} returned {} chars", llm_out.len()),
                    "next": "Parse action (tool or final)",
                    "progress": { "current": step.saturating_add(1), "total": progress_total },
                }),
            },
        );
        // Close the "Still planning…" row: it shares the id "plan", so without this it keeps spinning.
        emit(
            &mut steps,
            phase_event(
                step,
                "plan",
                format!("Plan with llama.cpp ({agent_model})"),
                "completed",
                format!("Planned by {agent_model}"),
                Some("Parse action (tool or final)"),
                Some(step.saturating_add(1)),
                Some(progress_total),
            ),
        );

        if wants_plan {
            // Not once the run has read text from outside: see `plan::taken_from`.
            if let Some(p) = crate::modules::planner::plan::taken_from(&llm_out, crate::agent_runs::scope::run_is_tainted()) {
                if p != plan {
                    plan = p;
                    crate::agent_runs::note_plan(&plan);
                    emit(&mut steps, AgentStep { step, kind: "plan_set".into(), detail: json!({ "id": format!("plan-set-{step}"), "phase": "plan", "title": "Plan", "status": "completed", "plan": plan.clone() }) });
                }
            }
        }

        let action = match parse_action(&llm_out) {
            Ok(a) => a,
            Err(e) => {
                // After we already composed+sent, don't burn remaining steps on "Hello!" parse fails.
                if is_email_send_goal(goal) && email_send_attempted(&history) {
                    let final_answer = email_send_final_answer(goal, email_send_confirmed(&history));
                    emit(
                        &mut steps,
                        phase_event(
                            step,
                            "finalize",
                            "Finalize answer",
                            "completed",
                            format!("LLM parse failed after send ({e}); finishing email path"),
                            None,
                            Some(progress_total),
                            Some(progress_total),
                        ),
                    );
                    emit(
                        &mut steps,
                        AgentStep {
                            step,
                            kind: "final".into(),
                            detail: json!({
                                "id": format!("final-{step}"),
                                "phase": "finalize",
                                "title": "Task complete",
                                "status": "completed",
                                "answer": final_answer,
                                "progress": { "current": progress_total, "total": progress_total },
                            }),
                        },
                    );
                    let result = McpAgentResult {
                        final_answer,
                        steps,
                        tool_rounds,
                        stopped_reason: "scripted_mail_send".into(),
                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                    };
                    learning.learn_success(
                        goal,
                        &recipe_from_steps(&result.steps, &result.final_answer),
                    );
                    return Ok(result);
                }
                if rounds == max_steps {
                    // The model's own words, which may cite what the run read: checked like any other final (card R6-18).
                    let (final_answer, cited_sources) = finalize_cited_answer(&crate::agent_runs::scope::current_run(), &words_of_unreadable_reply(&llm_out));
                    let result = McpAgentResult {
                        final_answer,
                        steps,
                        tool_rounds,
                        stopped_reason: format!("parse_error_on_last_step: {e}"),
                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: cited_sources,
                    };
                    learning.learn_failure(goal, &failed_tools);
                    return Ok(result);
                }
                history.push((
                    "parse_error".into(),
                    format!(
                        "{e}. Respond with ONLY one JSON object now, e.g. \
{{\"tool\":\"Snapshot\",\"arguments\":{{\"use_vision\":false}}}} or \
{{\"tool\":\"Click\",\"arguments\":{{\"name\":\"Send\"}}}}. \
No greetings. No markdown. /no_think"
                    ),
                ));
                continue;
            }
        };

        // Calls chained into one reply: the first runs now, the others wait for the steps that follow (and are dropped if the first one fails,
        // is asked about, or changes the page).
        let action = match action {
            LlmAction::Batch(mut calls) => {
                let (tool, arguments) = calls.remove(0);
                queued.extend(calls.into_iter().take(crate::modules::planner::schema::max_batch().saturating_sub(1)));
                LlmAction::Tool { tool, arguments }
            }
            other => other,
        };

        match action {
            LlmAction::Batch(_) => unreachable!("a batch was turned into its first call above"),
            LlmAction::Recall { handle } => {
                // An earlier result asked for again: the stored original goes at the end of the history, fenced like any result. It costs a
                // round, and there is a limit, so a model that keeps asking does not spend the run on it.
                recalls += 1;
                let (label, text) = if recalls > crate::modules::context::MAX_RECALLS {
                    ("recall".to_string(), format!("ERROR [INVALID_ARGUMENTS] recall: you have asked for earlier results {} times already; use what you have.", crate::modules::context::MAX_RECALLS))
                } else {
                    match crate::modules::context::recall(&history, &handle) {
                        Ok((tool, original)) => (format!("recall {handle} ({tool})"), original),
                        Err(why) => ("recall".to_string(), format!("ERROR [INVALID_ARGUMENTS] recall: {why}")),
                    }
                };
                emit(
                    &mut steps,
                    AgentStep {
                        step,
                        kind: "recall".into(),
                        detail: json!({ "id": format!("recall-{step}"), "phase": "plan", "title": format!("Recall {handle}"), "status": "completed", "handle": handle, "message": format!("Looked at an earlier result again ({handle})") }),
                    },
                );
                history.push((label, record_observation(&text)));
                continue;
            }
            LlmAction::Ask { question, choices } => {
                // A task has two questions at most. After that the planner is told to decide, and the round is spent.
                if questions_asked >= MAX_QUESTIONS {
                    history.push(("system".into(), format!("You have already asked {MAX_QUESTIONS} questions in this task and may not ask another. Decide with what you know and carry on, or give the final answer with what you can say.")));
                    continue;
                }
                // The planner has a question only the person can answer: the run ends with it (`waiting_user`) and the person's next
                // sentence is the answer, which the run goes on from (see `Resume`). An answer is text, never an approval.
                emit(
                    &mut steps,
                    phase_event(step, "finalize", "Waiting for you", "completed", question.clone(), None, Some(progress_total), Some(progress_total)),
                );
                emit(
                    &mut steps,
                    AgentStep {
                        step,
                        kind: "final".into(),
                        detail: json!({
                            "id": format!("final-{step}"),
                            "phase": "finalize",
                            "title": "Question for you",
                            "status": "completed",
                            "answer": question,
                            "choices": choices,
                            "message": "The planner asked a question instead of guessing.",
                            "progress": { "current": progress_total, "total": progress_total },
                        }),
                    },
                );
                return Ok(McpAgentResult { final_answer: question.clone(), steps, tool_rounds, stopped_reason: "waiting_user".into(), gate: None, run_id: None, question: Some(question), choices, sources: Vec::new() });
            }
            LlmAction::Final { final_answer } | LlmAction::FinalAlt { final_text: final_answer } => {
                // The answer of a plan run is its plan: it claims nothing is done, so the checks of what was done (a passing test, evidence for every figure) do not apply to it, and a plan run
                // that was sent back to "change the code and run the tests" could never end (card OA10-04, AG-06). Only an answer that is no plan at all is sent back.
                let rejected = if plan_run() {
                    is_placeholder_final(&final_answer).then(|| "that final is a placeholder, not a plan: write the plan, at most five numbered steps".to_string())
                } else {
                    reject_premature_final(goal, &final_answer, &history).or_else(|| reject_unchecked_coding_final(goal, &final_answer, &steps, &history))
                };
                if let Some(reason) = rejected {
                    warn!("Rejecting premature final: {reason}");
                    emit(
                        &mut steps,
                        phase_event(
                            step,
                            "verify",
                            "Verify goal evidence",
                            "failed",
                            format!("Premature final rejected: {reason}"),
                            Some("Continue acting / observing"),
                            Some(step.saturating_add(2)),
                            Some(progress_total),
                        ),
                    );
                    emit(
                        &mut steps,
                        AgentStep {
                            step,
                            kind: "final_rejected".into(),
                            detail: json!({
                                "id": format!("final-rejected-{step}"),
                                "phase": "verify",
                                "title": "Final answer rejected",
                                "status": "failed",
                                "answer": truncate(&final_answer, 500),
                                "reason": reason,
                                "message": format!("Rejected premature success: {reason}"),
                                "next": "Act or observe again",
                            }),
                        },
                    );
                    if crate::modules::run_profile::coding_latched(goal) && !goal_asks_for_information(goal) {
                        history.push(("system".into(), format!("FINAL REJECTED: {reason}. Reply with the next tool call as JSON.")));
                        continue;
                    }
                    history.push((
                        "system".into(),
                        format!(
                            "FINAL REJECTED: {reason}. \
Call Snapshot/Screenshot/browser_snapshot (or continue acting), then final only when the FULL goal is evidenced. \
If App launch failed for a website, use browser_navigate."
                        ),
                    ));
                    continue;
                }
                if last_step {
                    // The answer of the last round the budget allows (card OA5-04, TL-10) is what the run says it came to; the run is still one that used all its rounds, so it ends where
                    // it always did, below, with the model's words in the place of the template (checked against what the run read, as any other final is).
                    let (answer, cited) = finalize_cited_answer(&crate::agent_runs::scope::current_run(), &final_answer);
                    forced_final = Some((crate::modules::llm_router::strip_reasoning_tags(&answer), cited));
                    break;
                }
                emit(
                    &mut steps,
                    phase_event(
                        step,
                        "finalize",
                        "Finalize answer",
                        "completed",
                        "Goal verified — returning final answer",
                        None,
                        Some(progress_total),
                        Some(progress_total),
                    ),
                );
                emit(
                    &mut steps,
                    AgentStep {
                        step,
                        kind: "final".into(),
                        detail: json!({
                            "id": format!("final-{step}"),
                            "phase": "finalize",
                            "title": "Task complete",
                            "status": "completed",
                            "answer": final_answer,
                            "message": "Task verified complete.",
                            "progress": { "current": progress_total, "total": progress_total },
                        }),
                    },
                );
                // What the run read on the web checks the answer: a cited claim must be in the source it cites, a claim with no citation is cited from
                // the source that says it or marked, and the answer ends with the sources it used.
                let (final_answer, cited_sources) = finalize_cited_answer(&crate::agent_runs::scope::current_run(), &final_answer);
                // The answer of a coding run ends with what the program saw of its checks (`AQUA_ANSWER_EVIDENCE`, card OA4-04): the model's claim and the evidence side by side.
                let final_answer = if crate::flags::is_on(&crate::flags::ANSWER_EVIDENCE) && crate::modules::run_profile::coding_latched(goal) && !goal_asks_for_information(goal) {
                    format!("{}\n\n{}", final_answer.trim_end(), crate::modules::agent_experience::code_evidence_for_goal(&steps, goal).answer_note())
                } else {
                    final_answer
                };
                remember_calls_for_record(goal, &steps);
                let result = McpAgentResult {
                    final_answer: crate::modules::llm_router::strip_reasoning_tags(&final_answer),
                    steps,
                    tool_rounds,
                    stopped_reason: "final".into(),
                    gate: None, run_id: None, question: None, choices: Vec::new(), sources: cited_sources,
                };
                if tool_rounds > 0 {
                    learning.learn_success(
                        goal,
                        &recipe_from_steps(&result.steps, &result.final_answer),
                    );
                }
                return Ok(result);
            }
            LlmAction::Tool { tool, arguments } => {
                tool_rounds += 1;
                info!("MCP agent calling tool {tool}");
                emit(
                    &mut steps,
                    phase_event(
                        step,
                        "act",
                        format!("Act: {tool}"),
                        "running",
                        format!("Calling {tool}…"),
                        Some("Observe result"),
                        Some(step.saturating_add(2)),
                        Some(progress_total),
                    ),
                );
                emit(
                    &mut steps,
                    AgentStep {
                        step,
                        kind: "tool_start".into(),
                        detail: json!({
                            // Same id as success/error so the UI timeline upserts out of "running".
                            "id": format!("tool-{step}-{tool}"),
                            "phase": "act",
                            "title": format!("Call {tool}"),
                            "status": "running",
                            "tool": tool,
                            "arguments": arguments,
                            "message": format!("Round {step}/{max_steps}: calling {tool}…"),
                            "next": "Wait for observation",
                            "progress": { "current": step.saturating_add(2), "total": progress_total },
                        }),
                    },
                );
                let arguments = if tool.eq_ignore_ascii_case("Snapshot") {
                    snapshot_arguments_for_goal(goal, arguments)
                } else {
                    navigation_for_goal(goal, &tool, arguments)
                };
                if (tool.eq_ignore_ascii_case("browser_navigate") || tool.eq_ignore_ascii_case("local_open_url"))
                    && crate::laya_validator::needs_web_lookup(goal)
                    && !crate::laya_validator::looks_live_info(goal)
                {
                    let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let search_again = url.contains("google.com/search")
                        && history.iter().any(|(_, o)| o.contains("google.com/search"));
                    let site_again = !url.is_empty()
                        && visited_sites(&history).iter().any(|seen| url_key(seen) == url_key(url));
                    if search_again || site_again {
                        let final_answer = research_page_answer(goal, &history).unwrap_or_else(|| {
                            format!("I already opened the page for \"{goal}\" and did not find a description I could quote.")
                        });
                        return Ok(McpAgentResult {
                            final_answer,
                            steps,
                            tool_rounds,
                            stopped_reason: "final".into(),
                            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                        });
                    }
                }
                // A helper (`agent.task`, card OA11-04) is started here, where the planner is: it is a run of its own that the model of this one is paused for.
                let called = if tool == child::TOOL {
                    child::dispatch(planner, learning, &arguments, &child::Budget { started: run_started, limit: run_limit })
                } else {
                    call_named_tool(&tool, arguments.clone(), &tools, &browser, &windows)
                };
                match called {
                    Ok((label, raw_obs)) => {
                        let mut obs = sanitize_tool_observation(&tool, &raw_obs);
                        // A tool of the host says whether it failed (an `ERROR [CODE]` line, or a command's exit code); only what a page or a
                        // window returned is judged by the words in it.
                        let failed = if label == "host" { crate::modules::code_verification::host_call_failed(&obs) } else { obs.starts_with("ERROR") || observation_indicates_failure(&obs) };
                        last_call = Some(LastCall { failed, host: label == "host", asked: crate::agent_tools::last_call_asked(), tool: tool.clone() });
                        // The user declined an approval: stop here. Letting the model continue
                        // makes it retry the same action (asking again) or look for a way around.
                        // The one exception (card OA10-03, AG-11, decision D-12): a coding run whose "no" came with words ("edit config.toml instead") is shown the words and goes on,
                        // at most twice, when the setting is on. A plain "no", a third one, and any run that is not a coding run stop as they always did.
                        if obs.contains("[CONFIRMATION_DENIED]") && deny_feedback_lets_the_run_go_on(&obs, goal, &mut denials_gone_on_from) {
                            emit(
                                &mut steps,
                                phase_event(step, "denied_with_words", "You said no, and what to do instead", "completed", declined_action(&obs), Some("Go on with what you said"), Some(step.saturating_add(2)), Some(progress_total)),
                            );
                        } else if obs.contains("[CONFIRMATION_DENIED]") {
                            let what = declined_action(&obs);
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "finalize",
                                    "Stopped: you declined",
                                    "completed",
                                    what.clone(),
                                    None,
                                    Some(progress_total),
                                    Some(progress_total),
                                ),
                            );
                            return Ok(McpAgentResult {
                                final_answer: format!("I stopped without doing it because you declined: {what}."),
                                steps,
                                tool_rounds,
                                stopped_reason: "user_declined".into(),
                                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                            });
                        }
                        // A page that asks for a sign-in (a password box and a log-in button) when the task needs the person's account: the browser Aqua drives is not signed in,
                        // and Aqua never types a password for the person. Going on is guessing (it typed a made-up address into the sign-in form, then tried to post into a box
                        // that was not there, then opened the page again and again), so say what is wrong and stop.
                        if (label == "browser" || tool.starts_with("browser.")) && !failed {
                            if let Some(site) = login_wall_host(&obs) {
                                if goal_needs_an_account(goal) {
                                    let which = browser.as_ref().map(|b| b.policy().browser.label()).unwrap_or("browser");
                                    let answer = format!("{site} is asking you to sign in, and the {which} window Aqua opened is not signed in to it, so I stopped instead of guessing. Sign in yourself in that window (I never type your password), then ask me again.");
                                    emit(
                                        &mut steps,
                                        phase_event(step, "finalize", "Stopped: a sign-in is needed", "completed", answer.clone(), None, Some(progress_total), Some(progress_total)),
                                    );
                                    return Ok(McpAgentResult {
                                        final_answer: answer,
                                        steps,
                                        tool_rounds,
                                        stopped_reason: "needs_login".into(),
                                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                    });
                                }
                            }
                        }
                        // A voice request may not click, type or submit. Trying other tools would only burn minutes of model
                        // time on things that will be refused too, so say so and stop.
                        if obs.starts_with("ERROR [POLICY_TOOL_BLOCKED]") && windows_caller_now() == WindowsCaller::Voice {
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "finalize",
                                    "Stopped: not by voice",
                                    "completed",
                                    truncate(&obs, 300),
                                    None,
                                    Some(progress_total),
                                    Some(progress_total),
                                ),
                            );
                            return Ok(McpAgentResult {
                                final_answer: VOICE_CANNOT_ACT.to_string(),
                                steps,
                                tool_rounds,
                                stopped_reason: "voice_cannot_act".into(),
                                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                            });
                        }
                        // Aqua's own policy refused this tool. There is no other route to try.
                        if obs.starts_with("ERROR [PERMISSION_DENIED]") {
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "finalize",
                                    "Stopped: not allowed",
                                    "completed",
                                    truncate(&obs, 300),
                                    None,
                                    Some(progress_total),
                                    Some(progress_total),
                                ),
                            );
                            return Ok(McpAgentResult {
                                final_answer: format!("I stopped because that action is not allowed: {}", truncate(&obs, 300)),
                                steps,
                                tool_rounds,
                                stopped_reason: "policy_denied".into(),
                                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                            });
                        }
                        // A cap of the autonomy setting (card R7-18) ends the run. The model would only be refused again, and the repair hint, the learning of a failed tool and the
                        // web fallback after a failed `App` call would spend the rounds that are left on it. The answer says which limit and what was done; the record names the cap.
                        if obs.starts_with(crate::autonomy::OBSERVATION_PREFIX) {
                            let done: Vec<String> = steps.iter().filter(|s| s.kind == "tool").filter_map(|s| s.detail["tool"].as_str().map(str::to_string)).collect();
                            emit(
                                &mut steps,
                                AgentStep {
                                    step,
                                    kind: "tool_error".into(),
                                    detail: json!({
                                        "id": format!("tool-{step}-{tool}"),
                                        "phase": "act",
                                        "title": format!("{tool} stopped by a limit"),
                                        "status": "failed",
                                        "tool": tool,
                                        "source": label,
                                        "arguments": arguments,
                                        "observation": record_observation(&obs),
                                        "task_completed": false,
                                        "message": format!("Stopped by a limit: {}", truncate(obs.lines().next().unwrap_or("limit"), 160)),
                                        "next": "Stopped",
                                    }),
                                },
                            );
                            emit(
                                &mut steps,
                                phase_event(step, "finalize", "Stopped: a limit was reached", "completed", truncate(&obs, 300), None, Some(progress_total), Some(progress_total)),
                            );
                            let note = coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Limit);
                            return Ok(McpAgentResult {
                                final_answer: with_stop_note(crate::autonomy::stop_answer(&obs, &done), note),
                                steps,
                                tool_rounds,
                                stopped_reason: crate::autonomy::STOPPED_REASON.into(),
                                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                            });
                        }
                        if failed {
                            failed_tools.push(tool.clone());
                            let repair = learning.on_tool_error(goal, &tool, &obs);
                            // "APP LAUNCH FAILED ... open the website directly" is for an app that would not start. It used to be added to every failure, so a browser
                            // call that did not find its element told the model to open the site again, which it did, four times.
                            let fallback = if tool.eq_ignore_ascii_case("App") { app_launch_fallback_hint(goal, &obs) } else { None };
                            if let Some(ref hint) = fallback {
                                obs = format!("{obs}\n{hint}");
                            }
                            obs = format!("{obs}\nREPAIR HINT: {repair}");
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "act",
                                    format!("Act: {tool}"),
                                    "failed",
                                    truncate(obs.lines().next().unwrap_or("error"), 160),
                                    Some("Try fallback / next plan"),
                                    Some(step.saturating_add(2)),
                                    Some(progress_total),
                                ),
                            );
                            emit(
                                &mut steps,
                                AgentStep {
                                    step,
                                    kind: "tool_error".into(),
                                    detail: json!({
                                        "id": format!("tool-{step}-{tool}"),
                                        "phase": "act",
                                        "title": format!("{tool} failed"),
                                        "status": "failed",
                                        "tool": tool,
                                        "source": label,
                                        "arguments": arguments,
                                        "observation": record_observation(&obs),
                                        "repair": repair,
                                        "fallback": fallback,
                                        "task_completed": false,
                                        "message": format!(
                                            "❌ {tool} failed: {}",
                                            truncate(
                                                obs.lines().next().unwrap_or("error"),
                                                120
                                            )
                                        ),
                                        "next": fallback.as_deref().unwrap_or("Replan"),
                                    }),
                                },
                            );
                            history.push((tool.clone(), crate::modules::coding_session::annotate(&tool, &obs, arguments.get("path").and_then(|v| v.as_str()).unwrap_or(""))));

                            // Instagram/YouTube/etc.: don't wait another 30–90s llama plan — open the site now.
                            if let Some(recovered) = try_web_fallback_after_app_fail(
                                goal,
                                &tool,
                                &mut steps,
                                step,
                                max_steps,
                                progress_total,
                                &mut tool_rounds,
                                &mut history,
                                &learning,
                                &progress,
                                &browser,
                            ) {
                                return Ok(recovered);
                            }

                            // browser_* failed due to extension offline — one auto-recovery pass.
                            if !browser_recovery_attempted
                                && browser.is_some()
                                && !is_policy_refusal(&obs)
                                && (observation_indicates_browser_outage(&obs)
                                    || observation_indicates_extension_offline(&obs))
                            {
                                browser_recovery_attempted = true;
                                emit(
                                    &mut steps,
                                    phase_event(
                                        step,
                                        "browser_mcp",
                                        "Browser MCP recovery",
                                        "running",
                                        "Browser tool failed — re-running setup/start/connect verify…",
                                        Some("Retry browser tools if ready"),
                                        Some(step.saturating_add(1)),
                                        Some(progress_total),
                                    ),
                                );
                                if let Some(ref b) = browser {
                                    match Ok::<_, anyhow::Error>(b.ensure_ready()) {
                                        Ok(health) => {
                                            browser_control_ready = health.ready;
                                            info!(
                                                "Browser recovery: ready={} backend={:?}",
                                                browser_control_ready, health.backend
                                            );
                                            emit(
                                                &mut steps,
                                                phase_event(
                                                    step,
                                                    "browser_mcp",
                                                    "Browser MCP recovery",
                                                    if health.ready { "completed" } else { "failed" },
                                                    format!(
                                                        "ready={} — {}",
                                                        health.ready, health.detail
                                                    ),
                                                    Some("Continue task"),
                                                    Some(step.saturating_add(1)),
                                                    Some(progress_total),
                                                ),
                                            );
                                            if health.ready {
                                                // Re-expose browser tools and nudge retry of the original action.
                                                if let Ok(fresh) =
                                                    collect_tools_shared(browser.as_ref(), windows.as_ref(), goal)
                                                {
                                                    tools = fresh;
                                                }
                                                history.push((
                                                    "browser_mcp_recovery".into(),
                                                    format!(
                                                        "Browser MCP is now ready ({} tools). \
Retry the previous browser tool for the FULL goal. /no_think",
                                                        tools.len()
                                                    ),
                                                ));
                                                continue;
                                            }
                                            if let Some(ref ua) = health.user_action_required {
                                                history.push((
                                                    "browser_mcp_recovery".into(),
                                                    format!(
                                                        "Browser MCP still not connected: {ua}. \
Use Windows Snapshot/Click/Type/Shortcut (or local_open_url) — do NOT claim browser control success."
                                                    ),
                                                ));
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Browser MCP recovery failed: {e:#}");
                                            emit(
                                                &mut steps,
                                                phase_event(
                                                    step,
                                                    "browser_mcp",
                                                    "Browser MCP recovery",
                                                    "failed",
                                                    format!("{e}"),
                                                    Some("Fall back to Windows/local tools"),
                                                    Some(step.saturating_add(1)),
                                                    Some(progress_total),
                                                ),
                                            );
                                        }
                                    }
                                }
                            }

                            // browser_navigate failed because the browser is unavailable (not because the
                            // policy or the user refused) — still open the URL locally and continue.
                            if tool.eq_ignore_ascii_case("browser_navigate") && !is_policy_refusal(&obs) {
                                if let Some(url) = web_fallback_url(goal).filter(|u| authorize_local_open(&browser, u).is_ok()) {
                                    tool_rounds += 1;
                                    emit(
                                        &mut steps,
                                        phase_event(
                                            step,
                                            "act",
                                            "Act: open website (fallback)",
                                            "running",
                                            format!("Browser MCP unavailable — opening {url} locally…"),
                                            Some("Continue remaining goal steps"),
                                            Some(step.saturating_add(2)),
                                            Some(progress_total),
                                        ),
                                    );
                                    match open_url_locally(url, None) {
                                        Ok(local_obs) => {
                                            emit(
                                                &mut steps,
                                                phase_event(
                                                    step,
                                                    "act",
                                                    "Act: open website (fallback)",
                                                    "completed",
                                                    format!("Opened {url} locally; continue full goal"),
                                                    Some("Plan next compose/send/click steps"),
                                                    Some(step.saturating_add(2)),
                                                    Some(progress_total),
                                                ),
                                            );
                                            emit(
                                                &mut steps,
                                                AgentStep {
                                                    step,
                                                    kind: "tool".into(),
                                                    detail: json!({
                                                        "id": format!("tool-{step}-local_open_url"),
                                                        "phase": "act",
                                                        "title": "local_open_url ok",
                                                        "status": "completed",
                                                        "tool": "local_open_url",
                                                        "source": "local",
                                                        "arguments": { "url": url },
                                                        "observation": record_observation(&local_obs),
                                                        "message": format!("✅ Opened {url}; remaining goal steps still pending."),
                                                        "next": "Continue full goal with Windows/Browser tools",
                                                    }),
                                                },
                                            );
                                            history.push(("local_open_url".into(), local_obs));
                                            if goal_is_navigation_only(goal) {
                                                let final_answer = format!(
                                                    "Opened {url} in your default browser."
                                                );
                                                emit(
                                                    &mut steps,
                                                    phase_event(
                                                        step,
                                                        "finalize",
                                                        "Finalize answer",
                                                        "completed",
                                                        format!("Opened {url} (navigation-only)"),
                                                        None,
                                                        Some(progress_total),
                                                        Some(progress_total),
                                                    ),
                                                );
                                                emit(
                                                    &mut steps,
                                                    AgentStep {
                                                        step,
                                                        kind: "final".into(),
                                                        detail: json!({
                                                            "id": format!("final-{step}"),
                                                            "phase": "finalize",
                                                            "title": "Task complete",
                                                            "status": "completed",
                                                            "answer": final_answer,
                                                            "progress": { "current": progress_total, "total": progress_total },
                                                        }),
                                                    },
                                                );
                                                let result = McpAgentResult {
                                                    final_answer,
                                                    steps,
                                                    tool_rounds,
                                                    stopped_reason: "scripted_open".into(),
                                                    gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                                };
                                                learning.learn_success(
                                                    goal,
                                                    &recipe_from_steps(
                                                        &result.steps,
                                                        &result.final_answer,
                                                    ),
                                                );
                                                return Ok(result);
                                            }
                                        }
                                        Err(e) => {
                                            warn!("local open after browser_navigate fail: {e:#}");
                                        }
                                    }
                                }
                            }
                        } else {
                            emit(
                                &mut steps,
                                phase_event(
                                    step,
                                    "act",
                                    format!("Act: {tool}"),
                                    "completed",
                                    format!("{tool} returned via {label}"),
                                    Some("Observe / verify"),
                                    Some(step.saturating_add(2)),
                                    Some(progress_total),
                                ),
                            );
                            emit(
                                &mut steps,
                                AgentStep {
                                    step,
                                    kind: "tool".into(),
                                    detail: json!({
                                        "id": format!("tool-{step}-{tool}"),
                                        "phase": "act",
                                        "title": format!("{tool} ok"),
                                        "status": "completed",
                                        "tool": tool,
                                        "source": label,
                                        "arguments": arguments,
                                        "observation": record_observation(&obs),
                                        "action_ack_only": is_action_ack_only(&obs),
                                        "task_completed": false,
                                        "message": format!("✅ {tool} returned."),
                                        "next": "Observe UI if needed",
                                    }),
                                },
                            );
                            // The check the loop ran after an edit, as a call of the run (card OA4-04, ED-15).
                            for (check_args, check_obs) in take_automatic_checks() {
                                emit(&mut steps, automatic_check_step(step, check_args, &check_obs));
                            }
                            // Keep URL in history so compose detection works when MCP omits it.
                            if let Some(url) = arguments.get("url").and_then(|v| v.as_str()) {
                                if !obs.lines().any(|l| l.trim().strip_prefix("url=").is_some_and(|u| u.eq_ignore_ascii_case(url))) {
                                    obs = format!("{obs}\nurl={url}");
                                }
                            }
                            history.push((tool.clone(), crate::modules::coding_session::annotate(&tool, &obs, arguments.get("path").and_then(|v| v.as_str()).unwrap_or(""))));
                            if let Some(final_answer) = research_page_answer(goal, &history) {
                                return Ok(McpAgentResult {
                                    final_answer,
                                    steps,
                                    tool_rounds,
                                    stopped_reason: "final".into(),
                                    gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                });
                            }
                            if goal_is_news(goal) {
                                let read = history.iter().map(|(_, o)| o.as_str()).collect::<Vec<_>>().join("\n");
                                let lines = headline_lines(&read);
                                // A search page's first handful of titles is not a finished list: related questions pad it, and the story lives on a news site.
                                let enough = if observation_is_search_page(&read) { 8 } else { 5 };
                                if lines.len() >= enough {
                                    return Ok(McpAgentResult {
                                        final_answer: format!("Regarding \"{goal}\":\n{}", lines.join("\n")),
                                        steps,
                                        tool_rounds,
                                        stopped_reason: "final".into(),
                                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                    });
                                }
                            }
                            if crate::modules::run_profile::coding_latched(goal) {
                                let calls: Vec<(String, serde_json::Value)> = steps.iter().filter(|s| s.kind == "tool").filter_map(|s| {
                                    Some((s.detail.get("tool")?.as_str()?.to_string(), s.detail.get("arguments")?.clone()))
                                }).collect();
                                if let Some(repeated) = crate::modules::coding_driver::doom_loop(&calls).map(str::to_string) {
                                    if !doom_told {
                                        // The first time: said once, and the run goes on. A call that waits or polls a job never gets here (see `coding_driver::is_polling`).
                                        doom_told = true;
                                        let note = crate::modules::coding_driver::doom_nudge(&repeated);
                                        warn!("{note}");
                                        history.push(("system".into(), note));
                                    } else if crate::modules::coding_session::allow_ask(goal) && questions_asked < MAX_QUESTIONS && !crate::queue::in_background() {
                                        // The second time, a run that may ask asks: the person can say what to change, and the answer goes on from here (see `Resume`). A run nobody is watching cannot ask.
                                        let question = crate::modules::coding_driver::doom_question(&repeated);
                                        let choices = vec!["Try another way".to_string(), "Stop here".to_string()];
                                        emit(&mut steps, phase_event(step, "finalize", "Waiting for you", "completed", question.clone(), None, Some(progress_total), Some(progress_total)));
                                        return Ok(McpAgentResult { final_answer: question.clone(), steps, tool_rounds, stopped_reason: "waiting_user".into(), gate: None, run_id: None, question: Some(question), choices, sources: Vec::new() });
                                    } else if let Some(msg) = crate::modules::coding_driver::doom_message(&calls) {
                                        let note = coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Circles);
                                        return Ok(McpAgentResult {
                                            final_answer: with_stop_note(msg, note),
                                            steps,
                                            tool_rounds,
                                            stopped_reason: "doom_loop".into(),
                                            gate: None,
                                            run_id: None,
                                            question: None,
                                            choices: Vec::new(),
                                            sources: Vec::new(),
                                        });
                                    }
                                }
                            }
                            if let Some(note) = repeated_failure_note(&steps, &tool, &arguments).or_else(|| repeated_call_note(&steps, &tool, &arguments)) {
                                warn!("{note}");
                                history.push(("system".into(), note));
                            }

                            // Navigation-only goals may finish after opening the URL.
                            // Compound goals (send/compose/search/…) must keep acting.
                            if goal_is_navigation_only(goal) {
                                if let Some(url) = web_fallback_url(goal) {
                                    let cmd = arguments
                                        .get("command")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let local_url = arguments
                                        .get("url")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let opened = (tool.eq_ignore_ascii_case("local_open_url")
                                        && local_url.contains(url))
                                        || (tool.eq_ignore_ascii_case("browser_navigate")
                                            && local_url.contains(url))
                                        || (tool.eq_ignore_ascii_case("PowerShell")
                                            && cmd.contains("Start-Process")
                                            && cmd.contains(url));
                                    if opened {
                                        let final_answer =
                                            format!("Opened {url} in your default browser.");
                                        emit(
                                            &mut steps,
                                            phase_event(
                                                step,
                                                "finalize",
                                                "Finalize answer",
                                                "completed",
                                                format!("Opened {url} (navigation-only)"),
                                                None,
                                                Some(progress_total),
                                                Some(progress_total),
                                            ),
                                        );
                                        emit(
                                            &mut steps,
                                            AgentStep {
                                                step,
                                                kind: "final".into(),
                                                detail: json!({
                                                    "id": format!("final-{step}"),
                                                    "phase": "finalize",
                                                    "title": "Task complete",
                                                    "status": "completed",
                                                    "answer": final_answer,
                                                    "message": "Opened site via local/browser start.",
                                                    "progress": { "current": progress_total, "total": progress_total },
                                                }),
                                            },
                                        );
                                        let result = McpAgentResult {
                                            final_answer,
                                            steps,
                                            tool_rounds,
                                            stopped_reason: "scripted_open".into(),
                                            gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                        };
                                        learning.learn_success(
                                            goal,
                                            &recipe_from_steps(&result.steps, &result.final_answer),
                                        );
                                        return Ok(result);
                                    }
                                }
                            }

                            // A file search or the opening of a search result already has its answer. Say it, instead of
                            // spending model rounds (about a minute each on a CPU) to word it again: that also led the model
                            // to repeat the same search three times.
                            if (tool.eq_ignore_ascii_case("local_find") || tool.eq_ignore_ascii_case("local_open")) && !obs.starts_with("ERROR") {
                                let final_answer = if tool.eq_ignore_ascii_case("local_find") {
                                    aqua_mcp_adapters::local_files::last_search_reply().unwrap_or_else(|| obs.clone())
                                } else {
                                    obs.clone()
                                };
                                emit(
                                    &mut steps,
                                    AgentStep {
                                        step,
                                        kind: "final".into(),
                                        detail: json!({
                                            "id": format!("final-{step}"),
                                            "phase": "finalize",
                                            "title": "Task complete",
                                            "status": "completed",
                                            "answer": final_answer,
                                            "message": "Answered from the file search.",
                                            "progress": { "current": progress_total, "total": progress_total },
                                        }),
                                    },
                                );
                                return Ok(McpAgentResult { final_answer, steps, tool_rounds, stopped_reason: "local_answer".into(), gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new() });
                            }

                            // After UI-changing acks, auto-observe so the model sees real state.
                            let finds_or_opens_locally = tool.eq_ignore_ascii_case("local_find") || tool.eq_ignore_ascii_case("local_open");
                            if !finds_or_opens_locally
                                && (is_action_ack_only(&obs)
                                || tool.eq_ignore_ascii_case("App")
                                || tool.eq_ignore_ascii_case("browser_navigate")
                                || tool.eq_ignore_ascii_case("local_open_url")
                                || tool.eq_ignore_ascii_case("Shortcut")
                                || tool.eq_ignore_ascii_case("Click")
                                || tool.eq_ignore_ascii_case("browser_press_key")
                                || tool.eq_ignore_ascii_case("browser_click")
                                || tool.eq_ignore_ascii_case("browser_type")
                                || (tool.eq_ignore_ascii_case("PowerShell")
                                    && (obs.to_ascii_lowercase().contains("ctrl+enter")
                                        || obs.to_ascii_lowercase().contains("sendkeys"))))
                                && !is_observe_tool(&tool)
                            {
                                if let Some(auto_obs) = maybe_auto_observe(
                                    &tools, &browser, &windows, &tool,
                                ) {
                                    info!("Auto-observe after {}: {}", tool, auto_obs.0);
                                    emit(
                                        &mut steps,
                                        phase_event(
                                            step,
                                            "observe",
                                            "Observe UI state",
                                            "completed",
                                            format!("Auto-observe via {}", auto_obs.0),
                                            Some("Plan next action or finalize"),
                                            Some(step.saturating_add(2)),
                                            Some(progress_total),
                                        ),
                                    );
                                    emit(
                                        &mut steps,
                                        AgentStep {
                                            step,
                                            kind: "auto_observe".into(),
                                            detail: json!({
                                                "id": format!("observe-{step}"),
                                                "phase": "observe",
                                                "title": "Observe UI",
                                                "status": "completed",
                                                "tool": auto_obs.0,
                                                "source": auto_obs.1,
                                                "observation": record_observation(&auto_obs.2),
                                                "observation_required": true,
                                                "task_completed": false,
                                                "message": format!(
                                                    "Observing UI via {}…",
                                                    auto_obs.0
                                                ),
                                                "next": "Plan next action",
                                            }),
                                        },
                                    );
                                    history.push((auto_obs.0, auto_obs.2));
                                }
                            }

                            // Email: after compose + focused send attempt, finish (avoid Hello! loops).
                            // If Snapshot shows a non-Gmail Chrome tab focused, retry send once.
                            if is_email_send_goal(goal) && email_send_attempted(&history) {
                                if email_send_focus_looks_wrong(&history)
                                    && !history.iter().any(|(t, _)| {
                                        t.eq_ignore_ascii_case("mail_send_refocus")
                                    })
                                {
                                    warn!("Gmail send focus looked wrong — scheduling one refocus retry");
                                    history.push((
                                        "mail_send_refocus".into(),
                                        "Last SendKeys may have hit a non-Gmail Chrome window. \
Will re-focus a Gmail/Compose window and press Ctrl+Enter again.".into(),
                                    ));
                                    continue;
                                }
                                let final_answer = email_send_final_answer(goal, email_send_confirmed(&history));
                                emit(
                                    &mut steps,
                                    phase_event(
                                        step,
                                        "finalize",
                                        "Finalize answer",
                                        "completed",
                                        "Email compose + send attempt done",
                                        None,
                                        Some(progress_total),
                                        Some(progress_total),
                                    ),
                                );
                                emit(
                                    &mut steps,
                                    AgentStep {
                                        step,
                                        kind: "final".into(),
                                        detail: json!({
                                            "id": format!("final-{step}"),
                                            "phase": "finalize",
                                            "title": "Task complete",
                                            "status": "completed",
                                            "answer": final_answer,
                                            "message": "Compose opened and send keystroke/click issued.",
                                            "progress": { "current": progress_total, "total": progress_total },
                                        }),
                                    },
                                );
                                let result = McpAgentResult {
                                    final_answer,
                                    steps,
                                    tool_rounds,
                                    stopped_reason: "scripted_mail_send".into(),
                                    gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                                };
                                learning.learn_success(
                                    goal,
                                    &recipe_from_steps(&result.steps, &result.final_answer),
                                );
                                return Ok(result);
                            }
                        }
                    }
                    Err(e) => {
                        failed_tools.push(tool.clone());
                        let obs = format!("ERROR: {e}");
                        let repair = learning.on_tool_error(goal, &tool, &obs);
                        emit(
                            &mut steps,
                            phase_event(
                                step,
                                "act",
                                format!("Act: {tool}"),
                                "failed",
                                format!("{e}"),
                                Some("Replan"),
                                Some(step.saturating_add(2)),
                                Some(progress_total),
                            ),
                        );
                        emit(
                            &mut steps,
                            AgentStep {
                                step,
                                kind: "tool_error".into(),
                                detail: json!({
                                    "id": format!("tool-err-{step}-{tool}"),
                                    "phase": "act",
                                    "title": format!("{tool} error"),
                                    "status": "failed",
                                    "tool": tool,
                                    "arguments": arguments,
                                    "observation": obs,
                                    "repair": repair,
                                    "task_completed": false,
                                    "message": format!("❌ {tool} error: {e}"),
                                    "next": "Replan",
                                }),
                            },
                        );
                        history.push((tool, format!("{obs}\nREPAIR HINT: {repair}")));
                    }
                }
            }
        }
    }

    let (forced_text, forced_sources) = match forced_final {
        Some((text, sources)) => (Some(text), sources),
        None => (None, Vec::new()),
    };
    // A coding run that used its rounds and changed files says which, what its last check said and that it stopped (card OA4-04, AG-09): the sentence of the loop is for runs with nothing of the sort.
    let stop_note = if forced_text.is_none() { coding_stop_note(goal, &steps, crate::modules::coding_driver::StopWhy::Rounds) } else { None };
    // A coding run that used its rounds says where it stopped, and keeps it so that the person can ask for it to be gone on from (card OA10-04, WF-12, `AQUA_CONTINUE`).
    let where_it_stopped = continuation_note(goal, &steps);
    let result = McpAgentResult {
        final_answer: with_stop_note(forced_text.unwrap_or_else(|| stop_note.unwrap_or_else(|| synthesize_unfinished(goal, &history))), where_it_stopped),
        steps: {
            let mut s = steps;
            // Mark end-of-budget for the monitor.
            let end = phase_event(
                max_steps,
                "finalize",
                "Finalize answer",
                if observations_cover_goal(goal, &history) {
                    "completed"
                } else {
                    "failed"
                },
                "Reached max planning rounds",
                None,
                Some(progress_total),
                Some(progress_total),
            );
            if let Some(ref tx) = progress {
                let _ = tx.send(end.clone());
            }
            s.push(end);
            s
        },
        tool_rounds,
        stopped_reason: "max_steps".into(),
        gate: None, run_id: None, question: None, choices: Vec::new(), sources: forced_sources,
    };
    // Do not learn "success" when the last state still fails goal evidence.
    let evidenced = observations_cover_goal(goal, &history)
        && last_result_is_verified_observation(&history)
        && !history
            .last()
            .map(|(_, o)| observation_indicates_failure(o))
            .unwrap_or(true);
    if tool_rounds > 0 && evidenced {
        learning.learn_success(
            goal,
            &recipe_from_steps(&result.steps, &result.final_answer),
        );
    } else {
        learning.learn_failure(goal, &failed_tools);
    }
    Ok(result)
}
