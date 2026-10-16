//! Recipes version 2, the replay (card R2-13, task T2-13.2). Off unless `AQUA_RECIPES_V2=on`.
//!
//! A recipe of version 2 (see `memory/agent_learning.rs`) has placeholders, postconditions and a flag on each step that changes something. This replays it for a goal that gives
//! its placeholders their values, and does what version 1's replay does not:
//!
//! * **Writes replay, through the same door as every other call.** Each step goes to [`call_named_tool`], the one function all the agent's calls pass through: a tool of the host
//!   is decided, asked, audited and masked by the tool host, a click or a typed key by the browser's and the desktop's own guards. A replay is not a way round a question, and it is
//!   not the host's `Replay` caller either (which refuses every change): it is the agent, doing what the planner would have done.
//! * **A recipe that was learned from text from outside asks.** Its run had read a page, a window or a file, and the steps may have been steered by it: before its first change
//!   this run is marked as having read text from outside too, so the host asks, every time, whatever was said yes to earlier.
//! * **A spoken request changes nothing.** A step that changes something is not replayed for the voice.
//! * **A refusal ends the run, a mismatch does not.** "You declined", "not allowed" and "not by voice" stop the run as they do in the planner's loop. A step that fails, a
//!   postcondition that is false (the file is not there, the result does not hold the text) or a step with nothing to fill (typed text is never kept) hands the run to the planner
//!   from that step, with what the replay did so far in its history; what the planner then does is learned again as the recipe.
//!
//! The steps are given to `emit` as the planner's loop gives its own (a `tool_start` before the call, then `tool` or `tool_error`), so the run record, the screen and the next
//! recipe see them the same way.

use super::*;
use crate::memory::agent_learning::{check_postcondition, fill_arguments, fill_text, LearnedRecipe};

/// The most steps a recipe may have to be replayed.
const MAX_REPLAY_STEPS: usize = 60;

/// What the replay came to.
pub(super) enum Replay {
    /// No recipe applies, or its first step cannot be replayed: the run goes on as if there were none.
    Nothing,
    /// The run is over. The steps have been given to `emit`.
    Done { final_answer: String, tool_rounds: u32, stopped_reason: String },
    /// The recipe went part of the way and stopped, or ran every step and left the answer to the planner. The planner goes on with `history` as what has been done; `calls` steps
    /// were taken.
    Handover { history: Vec<(String, String)>, calls: u32 },
}

/// What a replay needs of the run it is in.
pub(super) struct Replayer<'a> {
    pub learning: &'a AgentLearning,
    pub ctx: &'a crate::memory::AgentLearningContext,
    pub tools: &'a [McpTool],
    pub browser: &'a Option<BrowserHandle>,
    pub windows: &'a Option<Arc<Mutex<StdioMcpClient>>>,
    pub goal: &'a str,
}

/// The note the planner is given in its history when a recipe stops: what ran and why it stopped.
fn replay_note(ran: u32, at: usize, tool: &str, why: &str) -> (String, String) {
    let ran = match ran {
        0 => "no steps".to_string(),
        1 => "1 step".to_string(),
        n => format!("{n} steps"),
    };
    (
        "system".to_string(),
        format!("A saved recipe for this task ran {ran} and stopped at step {} ({tool}): {why}. Go on from there toward the goal; do not repeat the steps that already ran.", at + 1),
    )
}

/// `calls` steps were taken (numbered 1 to `calls`); `ran` of them went as the recipe expects, and it stopped at step `at` (counted from 0).
fn replay_handover(mut history: Vec<(String, String)>, calls: u32, ran: u32, at: usize, tool: &str, why: &str) -> Replay {
    history.push(replay_note(ran, at, tool, why));
    Replay::Handover { history, calls }
}

/// A refusal that ends the run in the planner's loop ends it here too: the person said no, a rule forbids it, or the voice asked for a change.
fn replay_refusal(obs: &str, caller: WindowsCaller) -> Option<(&'static str, String)> {
    if obs.contains("[CONFIRMATION_DENIED]") {
        return Some(("user_declined", format!("I stopped without doing it because you declined: {}.", declined_action(obs))));
    }
    if obs.starts_with("ERROR [POLICY_TOOL_BLOCKED]") && caller == WindowsCaller::Voice {
        return Some(("voice_cannot_act", VOICE_CANNOT_ACT.to_string()));
    }
    if obs.starts_with("ERROR [PERMISSION_DENIED]") {
        return Some(("policy_denied", format!("I stopped because that action is not allowed: {}", truncate(obs, 300))));
    }
    None
}

/// The answer of a recipe that changed something: the answer it was learned with, with this goal's values in it, or a plain sentence of what was done. It is not weighed by the
/// checks that the planner's answer is (they ask for evidence in what a page showed); what makes the replay trustworthy is that every step went through the door and the
/// postconditions held.
fn replay_write_answer(recipe: &LearnedRecipe, bindings: &[(String, String)]) -> String {
    let saved = fill_text(&recipe.final_answer, bindings);
    let saved = saved.trim();
    if !saved.is_empty() && !saved.contains("{{") && !crate::memory::solutions_db::is_placeholder_answer(saved) && !crate::memory::solutions_db::looks_like_raw_dump(saved) {
        return saved.to_string();
    }
    let n = recipe.tools.len();
    let checked = if recipe.postconditions.is_empty() { "" } else { " and checked the result" };
    format!("I ran the {n} {} saved for this task{checked}.", if n == 1 { "step" } else { "steps" })
}

/// The answer of a recipe that only reads, by the rules version 1's replay has: live data comes from what was just read, a saved answer is used only when nothing fresher
/// is, and the answer must pass the checks the planner's answer passes. `None` when no honest answer can be made from what was read.
fn replay_read_answer(goal: &str, saved: &str, last_obs: &str, history: &[(String, String)]) -> Option<String> {
    if crate::laya_validator::looks_live_info(goal) && format_live_observation(last_obs).is_none() {
        return None;
    }
    let answer = if saved.split_whitespace().count() >= 10 {
        let live = format_live_observation(last_obs).unwrap_or_default();
        if !live.is_empty() && (crate::laya_validator::looks_live_info(goal) || live.contains('\n')) {
            live
        } else if live.is_empty() {
            saved.to_string()
        } else {
            format!("{} (latest reading: {})", saved.trim(), live)
        }
    } else if !last_obs.is_empty() {
        format_live_observation(last_obs).unwrap_or_else(|| if !saved.trim().is_empty() { saved.to_string() } else { last_obs.to_string() })
    } else if !saved.trim().is_empty() {
        saved.to_string()
    } else {
        return None;
    };
    if answer.trim().is_empty() || crate::memory::solutions_db::looks_like_raw_dump(&answer) {
        return None;
    }
    reject_premature_final(goal, &answer, history).is_none().then_some(answer)
}

/// Replays the recipe the run's goal matched, if it is a version 2 recipe and every condition for replaying it holds. `emit` takes each step as it happens; `stop` says whether the
/// person pressed Stop or went away (and why).
pub(super) fn try_recipe_v2(r: &Replayer, emit: &mut dyn FnMut(AgentStep), stop: &dyn Fn() -> Option<&'static str>) -> Replay {
    let Some(matched) = r.ctx.matched.as_ref() else { return Replay::Nothing };
    if !matched.is_fast_path_eligible {
        return Replay::Nothing;
    }
    // A recipe that has been failing lately is not trusted again (the same rule as version 1's).
    if matched.solution.success_rate < 0.5 && matched.solution.execution_count > 2 {
        return Replay::Nothing;
    }
    let Some(recipe) = r.ctx.recipe.as_ref() else { return Replay::Nothing };
    // A recipe from a later version is shown to the planner as a hint, not replayed by code that does not know what its fields mean. A run takes at most a dozen
    // rounds, so a recipe of many dozens of steps is not one Aqua made.
    if recipe.version != 2 || recipe.tools.is_empty() || recipe.tools.len() > MAX_REPLAY_STEPS {
        return Replay::Nothing;
    }
    // A practice run is unattended and changes nothing.
    if crate::modules::agent_experience::is_practice() {
        return Replay::Nothing;
    }
    let id = matched.solution.id;
    let caller = windows_caller_now();
    let bindings = &r.ctx.bindings;
    info!("MCP agent recipe v2 (solution_id={id}, {} steps)", recipe.tools.len());

    let mut history: Vec<(String, String)> = Vec::new();
    let mut last_obs = String::new();
    let mut calls = 0u32;
    let mut write_ran = false;
    for (i, call) in recipe.tools.iter().enumerate() {
        if let Some(reason) = stop() {
            return Replay::Done { final_answer: "Stopped.".into(), tool_rounds: calls, stopped_reason: reason.into() };
        }
        let step = (i + 1) as u32;
        // A step with nothing to fill (typed text was never kept, or the goal gave no value) cannot be replayed: the planner does it from here.
        let args = match fill_arguments(&call.arguments, &recipe.params, bindings) {
            Ok(a) => a,
            Err(why) => {
                info!("Recipe v2 stops before step {step} ({}): {why}", call.tool);
                return if calls == 0 { Replay::Nothing } else { replay_handover(history, calls, calls, i, &call.tool, &why) };
            }
        };
        // The step's own flag says it changes something, and so does what it is now: a flag in a file is not the only thing that decides.
        let writes = call.write || recipe_step_writes(pick_source(&call.tool, r.tools), &call.tool, &args);
        if writes && caller == WindowsCaller::Voice {
            return if calls == 0 { Replay::Nothing } else { replay_handover(history, calls, calls, i, &call.tool, "a spoken request cannot change anything") };
        }
        if writes && recipe.tainted {
            // The run it was learned from had read text from outside: this one is held to the same rule, so the host asks before it changes anything.
            crate::agent_runs::scope::taint_current_run();
        }
        emit(AgentStep {
            step,
            kind: "tool_start".into(),
            detail: json!({
                "id": format!("tool-{step}-{}", call.tool),
                "phase": "act",
                "title": format!("Call {}", call.tool),
                "status": "running",
                "tool": call.tool,
                "arguments": args,
                "message": format!("Saved recipe, step {step}/{}: {}", recipe.tools.len(), call.tool),
                "next": "Wait for observation",
            }),
        });
        let outcome = call_named_tool(&call.tool, args.clone(), r.tools, r.browser, r.windows);
        calls += 1;
        let (label, raw) = match outcome {
            Ok(done) => done,
            Err(e) => {
                let obs = format!("ERROR {}: {e:#}", call.tool);
                emit(replay_tool_error(step, &call.tool, "guard", &args, &obs));
                history.push((call.tool.clone(), obs));
                r.learning.mark_recipe_reused(id, false);
                return replay_handover(history, calls, calls - 1, i, &call.tool, "the step could not be run");
            }
        };
        let obs = sanitize_tool_observation(&call.tool, &raw);
        let failed = if label == "host" { crate::modules::code_verification::host_call_failed(&obs) } else { obs.starts_with("ERROR") };
        if let Some((reason, answer)) = replay_refusal(&obs, caller) {
            emit(replay_tool_error(step, &call.tool, &label, &args, &obs));
            emit(phase_event(step, "finalize", "Stopped", "completed", truncate(&obs, 300), None, None, None));
            return Replay::Done { final_answer: answer, tool_rounds: calls, stopped_reason: reason.into() };
        }
        if failed {
            warn!("Recipe v2 step {step} ({}) failed: {}", call.tool, truncate(&obs, 200));
            emit(replay_tool_error(step, &call.tool, &label, &args, &obs));
            history.push((call.tool.clone(), obs.clone()));
            r.learning.mark_recipe_reused(id, false);
            let _ = r.learning.on_tool_error(&matched.solution.intent_pattern, &call.tool, &obs);
            return replay_handover(history, calls, calls - 1, i, &call.tool, "the step returned an error");
        }
        emit(AgentStep {
            step,
            kind: "tool".into(),
            detail: json!({
                "id": format!("tool-{step}-{}", call.tool),
                "phase": "act",
                "title": format!("{} ok", call.tool),
                "status": "completed",
                "tool": call.tool,
                "source": label,
                "arguments": args,
                "observation": record_observation(&obs),
                "action_ack_only": is_action_ack_only(&obs),
                "task_completed": false,
                "message": format!("{} returned (saved recipe).", call.tool),
                "next": "Observe UI if needed",
            }),
        });
        history.push((call.tool.clone(), obs.clone()));
        last_obs = obs.clone();
        write_ran |= writes;
        if (is_action_ack_only(&obs) || call.tool.eq_ignore_ascii_case("App")) && !is_observe_tool(&call.tool) {
            if let Some((tool, source, seen)) = maybe_auto_observe(r.tools, r.browser, r.windows, &call.tool) {
                emit(AgentStep { step, kind: "auto_observe".into(), detail: json!({ "tool": tool, "source": source, "observation": record_observation(&seen) }) });
                history.push((tool, seen));
            }
        }
        // What must be true after this step, checked now: when it is not, the planner takes over from here, knowing what the step did.
        for p in recipe.postconditions.iter().filter(|p| p.after() == i) {
            if let Err(why) = check_postcondition(p, &obs, bindings, &|path: &str| std::path::Path::new(path).exists()) {
                warn!("Recipe v2 step {step} ({}): {why}", call.tool);
                r.learning.mark_recipe_reused(id, false);
                return replay_handover(history, calls, calls, i, &call.tool, &format!("what the recipe expects after it is not so: {why}"));
            }
        }
    }

    // Every step ran and every postcondition held.
    let n = recipe.tools.len();
    let changed = recipe.holds_writes() || write_ran;
    if !changed && last_obs.contains("<untrusted:") {
        // What the last step returned is a file's or a command's text that the host fenced: it is not turned into an answer by code (it would put the text itself in the
        // answer, markers and all). The planner reads it and says what it says; every step is already done, so that is one round and not all of them.
        r.learning.mark_recipe_reused(id, true);
        let plural = if n == 1 { "step" } else { "steps" };
        history.push(("system".to_string(), format!("A saved recipe for this task ran all {n} {plural}. Give the answer from what they returned; do not run them again.")));
        return Replay::Handover { history, calls };
    }
    let answer = if changed {
        Some(replay_write_answer(recipe, bindings))
    } else {
        replay_read_answer(r.goal, &fill_text(&recipe.final_answer, bindings), &last_obs, &history)
    };
    let Some(answer) = answer else {
        r.learning.mark_recipe_reused(id, false);
        return replay_handover(history, calls, calls, n - 1, &recipe.tools[n - 1].tool, "no answer could be made from what the steps returned");
    };
    emit(AgentStep { step: calls + 1, kind: "final".into(), detail: json!({ "answer": answer }) });
    r.learning.mark_recipe_reused(id, true);
    Replay::Done { final_answer: answer, tool_rounds: calls, stopped_reason: "learned_fast_path".into() }
}

fn replay_tool_error(step: u32, tool: &str, source: &str, args: &Value, obs: &str) -> AgentStep {
    AgentStep {
        step,
        kind: "tool_error".into(),
        detail: json!({
            "id": format!("tool-{step}-{tool}"),
            "phase": "act",
            "title": format!("{tool} failed"),
            "status": "failed",
            "tool": tool,
            "source": source,
            "arguments": args,
            "observation": record_observation(obs),
            "task_completed": false,
            "message": format!("{tool} failed: {}", truncate(obs.lines().next().unwrap_or("error"), 120)),
            "next": "Replan",
        }),
    }
}

/// Card R2-13, validation: the replay through the real loop with the shared test host (`fs.read_file`, `fs.write_file`, a scripted approver) and a planner that says what it is
/// told. Each of these takes the environment lock (the switch is read from the environment) and the loop lock (the host and what it records are shared).
#[cfg(test)]
mod replay_tests {
    use super::super::tests::{shared_host, ANSWERS, ASKED, LOOP_LOCK, WRITES};
    use super::super::{recipe_from_steps, run_mcp_agent_inner, try_learned_fast_path, with_windows_caller, AgentStep, McpAgentResult, WindowsCaller};
    use super::*;
    use crate::memory::{intent_embedding, AgentLearning};
    use serde_json::json;
    use std::collections::VecDeque;

    /// A planner that says what it is told to, one reply per question, and keeps every prompt it was shown.
    struct Planned {
        replies: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl Planned {
        fn new(replies: &[&str]) -> Self {
            Self { replies: Mutex::new(replies.iter().map(|r| r.to_string()).collect()), prompts: Mutex::new(Vec::new()) }
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    impl crate::modules::planner::Planner for Planned {
        fn model_name(&self) -> String {
            "scripted".into()
        }
        fn max_context_tokens(&self) -> u32 {
            4096
        }
        fn warm_prefix(&self, _prefix: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn plan(&self, req: &crate::modules::planner::PlanRequest<'_>, _on_tick: &mut dyn FnMut(u64)) -> anyhow::Result<crate::modules::planner::PlanReply> {
            self.prompts.lock().unwrap().push(req.prompt.to_string());
            let text = self.replies.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("the planner was asked more often than the test expected"))?;
            Ok(crate::modules::planner::PlanReply { text, ..Default::default() })
        }
    }

    const FINAL: &str = r#"{"final":"Saved the note in beta.txt."}"#;

    /// More replies than a run of six rounds can ask for.
    fn finals() -> Vec<&'static str> {
        vec![FINAL; 8]
    }

    const TEMPLATE: &str = "save the text \"{{text1}}\" in the file {{file1}}";
    const ALPHA: &str = "save the text \"alpha note\" in the file alpha.txt";
    const BETA: &str = "save the text \"beta note\" in the file beta.txt";
    const PARAMS: &str = r#""params":[{"name":"text1","kind":"text"},{"name":"file1","kind":"file"}]"#;

    /// A version 2 recipe as the learner stores it, for the template above: the steps given as JSON.
    fn recipe_json(steps: &str, extra: &str) -> String {
        format!(r#"{{"version":2,"tools":[{steps}],"final_answer":"Saved {{{{text1}}}} in {{{{file1}}}}.",{PARAMS}{extra}}}"#)
    }

    const WRITE_STEP: &str = r#"{"tool":"fs.write_file","arguments":{"content":"{{text1}}","path":"{{file1}}"},"source":"host","write":true}"#;
    const DELETE_STEP: &str = r#"{"tool":"fs.delete_file","arguments":{"path":"{{file1}}"},"source":"host","write":true}"#;

    fn fresh(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_recipe_replay_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn learning_with(name: &str, template: &str, json: &str) -> (AgentLearning, std::path::PathBuf) {
        let dir = fresh(name);
        let learning = AgentLearning::at(&dir);
        learning.solutions.save_or_reinforce(template, json, &intent_embedding(template)).unwrap();
        (learning, dir)
    }

    /// The environment lock, with the settings the loop reads put back to their defaults.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::modules::planner::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        for var in ["AQUA_PLANNER_BATCH", "AQUA_PLANNER_CONSTRAINT", "AQUA_RECIPES_V2"] {
            std::env::remove_var(var);
        }
        guard
    }

    /// `AQUA_RECIPES_V2=on` for one test; it is taken off again whatever happens.
    struct SwitchOn;
    impl SwitchOn {
        fn new() -> Self {
            std::env::set_var("AQUA_RECIPES_V2", "on");
            SwitchOn
        }
    }
    impl Drop for SwitchOn {
        fn drop(&mut self) {
            std::env::remove_var("AQUA_RECIPES_V2");
        }
    }

    /// One run of the loop, as `run_scripted` makes it: its own identity in the host, no browser, no desktop. The caller holds the loop lock.
    fn run(planner: &Planned, learning: &AgentLearning, goal: &str) -> McpAgentResult {
        let run = crate::agent_runs::scope::RunScope::begin(crate::agent_tools::host().cloned());
        let _ = &run;
        run_mcp_agent_inner(planner, learning, None, None, goal, 6, None, None, &mut crate::modules::agent_experience::ExperienceUse::default()).expect("the loop ends with a result")
    }

    fn writes_since(before: usize) -> Vec<String> {
        WRITES.lock().unwrap()[before..].to_vec()
    }

    #[test]
    fn a_recipe_learned_from_a_run_is_replayed_for_new_values_through_the_host_and_the_planner_is_not_asked() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        let dir = fresh("learned");
        let learning = AgentLearning::at(&dir);
        // The run that is learned from: one write of the text the goal quoted, to the file it named.
        let steps = vec![AgentStep { step: 1, kind: "tool".into(), detail: json!({ "tool": "fs.write_file", "source": "host", "arguments": {"path": "alpha.txt", "content": "alpha note"}, "observation": "Wrote alpha.txt" }) }];
        learning.learn_success(ALPHA, &recipe_from_steps(&steps, "Saved alpha note in alpha.txt."));
        let rows = learning.solutions.list_solutions().unwrap();
        assert_eq!((rows.len(), rows[0].intent_pattern.as_str()), (1, TEMPLATE));
        assert!(!rows[0].payload_recipe_json.contains("alpha"), "no value was stored: {}", rows[0].payload_recipe_json);

        // The same task with other values: the recipe is replayed, and the planner is never asked.
        let planner = Planned::new(&[]);
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, BETA);
        assert_eq!((r.stopped_reason.as_str(), r.final_answer.as_str(), r.tool_rounds), ("learned_fast_path", "Saved beta note in beta.txt.", 1), "{r:?}");
        assert!(planner.prompts().is_empty(), "no planning round was spent");
        assert_eq!(writes_since(writes), vec!["beta.txt".to_string()], "the host was asked to write this goal's file, once");
        assert!(ASKED.lock().unwrap().is_empty(), "a write the policy allows is not asked about");
        let tools: Vec<&AgentStep> = r.steps.iter().filter(|s| s.kind == "tool").collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].detail["arguments"], json!({"path": "beta.txt", "content": "beta note"}), "the call is made with the goal's values");
        let kinds: Vec<&str> = r.steps.iter().map(|s| s.kind.as_str()).collect();
        let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap_or(usize::MAX);
        assert!(at("tool_start") < at("tool") && at("tool") < at("final"), "announced before the call, as the loop announces its own: {kinds:?}");
        // Replaying is a use of the recipe, and the recipe is as it was.
        let rows = learning.solutions.list_solutions().unwrap();
        assert_eq!((rows.len(), rows[0].execution_count), (1, 2));
        assert!(rows[0].payload_recipe_json.contains("{{file1}}") && !rows[0].payload_recipe_json.contains("beta"), "{}", rows[0].payload_recipe_json);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recipe_learned_from_text_from_outside_asks_before_it_writes_and_a_no_ends_the_run() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        let (learning, dir) = learning_with("tainted", TEMPLATE, &recipe_json(WRITE_STEP, r#","tainted":true"#));

        // Nobody answers yes: the write is not done, and the run ends with the person's no, as a planner's write would.
        let planner = Planned::new(&[]);
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, BETA);
        assert_eq!(r.stopped_reason, "user_declined", "{r:?}");
        assert!(r.final_answer.starts_with("I stopped without doing it because you declined"), "{}", r.final_answer);
        assert_eq!(writes_since(writes), Vec::<String>::new(), "the write did not happen");
        {
            let asked = ASKED.lock().unwrap();
            assert_eq!(asked.len(), 1, "the person was asked");
            assert!(asked[0].always_ask, "every time, whatever was said yes to before: {:?}", asked[0]);
        }
        assert!(planner.prompts().is_empty() && r.steps.iter().all(|s| s.kind != "tool"), "and nothing else was tried");
        assert_eq!(learning.solutions.list_solutions().unwrap()[0].execution_count, 1, "a no is not a failure of the recipe");

        // Yes: the same recipe writes, and was asked about once more.
        ANSWERS.lock().unwrap().push_back(aqua_tool_host::ApprovalOutcome::Approved(aqua_tool_host::ApprovalScope::Once));
        let r = run(&planner, &learning, BETA);
        assert_eq!((r.stopped_reason.as_str(), r.final_answer.as_str()), ("learned_fast_path", "Saved beta note in beta.txt."), "{r:?}");
        assert_eq!(writes_since(writes), vec!["beta.txt".to_string()]);
        assert_eq!(ASKED.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_step_that_fails_hands_the_run_to_the_planner_from_that_step_and_the_corrected_run_is_learned() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        // The second step is a tool this host does not have: the changed world of the test.
        let (learning, dir) = learning_with("mismatch", TEMPLATE, &recipe_json(&format!("{WRITE_STEP},{DELETE_STEP}"), ""));

        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, BETA);
        // The first step ran, the second returned an error, and the planner went on from there without starting again.
        assert_eq!(writes_since(writes), vec!["beta.txt".to_string()], "the first step ran once");
        let errors: Vec<&AgentStep> = r.steps.iter().filter(|s| s.kind == "tool_error").collect();
        assert_eq!((errors.len(), errors[0].detail["tool"].as_str()), (1, Some("fs.delete_file")), "{:?}", r.steps.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>());
        let prompts = planner.prompts();
        assert!(!prompts.is_empty(), "the planner was asked");
        assert!(
            prompts[0].contains("A saved recipe for this task ran 1 step and stopped at step 2 (fs.delete_file): the step returned an error. Go on from there toward the goal; do not repeat the steps that already ran."),
            "{}",
            prompts[0]
        );
        assert!(prompts[0].contains("fs.write_file beta.txt"), "what the first step returned is in the history: {}", prompts[0]);
        assert_eq!(r.stopped_reason, "final", "{}", r.final_answer);
        // The run is learned again: the step that did not work is gone, the one that did is kept, with its placeholders.
        let rows = learning.solutions.list_solutions().unwrap();
        assert_eq!(rows.len(), 1);
        let recipe: LearnedRecipe = serde_json::from_str(&rows[0].payload_recipe_json).unwrap();
        assert_eq!((recipe.version, recipe.tools.len(), recipe.tools[0].tool.as_str()), (2, 1, "fs.write_file"), "{}", rows[0].payload_recipe_json);
        assert!(!rows[0].payload_recipe_json.contains("fs.delete_file") && !rows[0].payload_recipe_json.contains("beta"), "{}", rows[0].payload_recipe_json);
        assert!(rows[0].success_rate < 1.0, "the mismatch was counted against the recipe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_postcondition_that_is_not_true_after_a_step_hands_the_run_to_the_planner_from_that_step() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        // The host of the test writes nothing to the disk, so the file the recipe expects after its step is not there: the changed file of the test.
        let post = r#","postconditions":[{"kind":"file_exists","path":"{{file1}}","after":0}]"#;
        let (learning, dir) = learning_with("postcondition", TEMPLATE, &recipe_json(WRITE_STEP, post));
        let goal = "save the text \"gamma note\" in the file gamma_never_made.txt";

        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, goal);
        assert_eq!(writes_since(writes), vec!["gamma_never_made.txt".to_string()], "the step ran");
        let prompts = planner.prompts();
        assert!(!prompts.is_empty(), "the planner was asked");
        assert!(
            prompts[0].contains("stopped at step 1 (fs.write_file): what the recipe expects after it is not so: the file gamma_never_made.txt is not there."),
            "{}",
            prompts[0]
        );
        assert_ne!(r.stopped_reason, "learned_fast_path");
        assert!(learning.solutions.list_solutions().unwrap()[0].success_rate < 1.0, "counted against the recipe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recipe_that_reads_a_file_runs_its_steps_and_leaves_the_answer_to_the_planner_instead_of_putting_the_files_text_in_it() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        let template = "read the file {{file1}} and tell me what it says";
        let json = r#"{"version":2,"tools":[{"tool":"fs.read_file","arguments":{"path":"{{file1}}"},"source":"host"}],"final_answer":"","params":[{"name":"file1","kind":"file"}]}"#;
        let (learning, dir) = learning_with("read", template, json);

        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, "read the file delta.txt and tell me what it says");
        assert_ne!(r.stopped_reason, "learned_fast_path", "the text of a file is not an answer made by code");
        assert_eq!(writes_since(writes), Vec::<String>::new(), "a read changes nothing");
        let reads: Vec<&AgentStep> = r.steps.iter().filter(|s| s.kind == "tool").collect();
        assert_eq!((reads.len(), reads[0].detail["tool"].as_str(), reads[0].detail["arguments"].clone()), (1, Some("fs.read_file"), json!({"path": "delta.txt"})), "the step ran with the goal's file");
        let prompts = planner.prompts();
        assert!(!prompts.is_empty(), "the planner was asked once the steps were done");
        assert!(prompts[0].contains("A saved recipe for this task ran all 1 step. Give the answer from what they returned; do not run them again."), "{}", prompts[0]);
        assert!(prompts[0].contains("<untrusted:"), "what the file returned reaches the planner as text from outside, in its fence: {}", prompts[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_step_with_nothing_to_fill_stops_the_replay_before_it_and_a_first_step_like_that_replays_nothing() {
        let _env = lock_env();
        let _on = SwitchOn::new();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        // Text the planner wrote was hidden when the recipe was made: it cannot be typed again, so the step is the planner's.
        let typed = r#"{"tool":"fs.write_file","arguments":{"content":"<redacted 11 chars>","path":"{{file1}}"},"source":"host","write":true}"#;

        let (learning, dir) = learning_with("second_unfillable", TEMPLATE, &recipe_json(&format!("{WRITE_STEP},{typed}"), ""));
        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let _ = run(&planner, &learning, BETA);
        assert_eq!(writes_since(writes), vec!["beta.txt".to_string()], "only the first step ran");
        let prompts = planner.prompts();
        assert!(prompts[0].contains("ran 1 step and stopped at step 2 (fs.write_file): a value in this step was not kept when the recipe was made"), "{}", prompts[0]);
        let _ = std::fs::remove_dir_all(&dir);

        let (learning, dir) = learning_with("first_unfillable", TEMPLATE, &recipe_json(typed, ""));
        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, BETA);
        assert_eq!(writes_since(writes), Vec::<String>::new(), "nothing was replayed");
        assert!(!planner.prompts().is_empty() && planner.prompts().iter().all(|p| !p.contains("A saved recipe for this task ran")), "the run went on as if there were no recipe");
        assert_ne!(r.stopped_reason, "learned_fast_path");
        assert!(r.steps.iter().all(|s| s.kind != "tool" && s.kind != "tool_start"), "no step of the recipe was announced or run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_the_switch_off_a_version_2_recipe_is_not_replayed_and_a_spoken_request_replays_no_write() {
        let _env = lock_env();
        let _host = shared_host();
        let _one = LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ANSWERS.lock().unwrap().clear();
        ASKED.lock().unwrap().clear();
        let (learning, dir) = learning_with("off", TEMPLATE, &recipe_json(WRITE_STEP, ""));

        // Off: the planner does the task, and is not shown a recipe with placeholders in it.
        let planner = Planned::new(&finals());
        let writes = WRITES.lock().unwrap().len();
        let r = run(&planner, &learning, BETA);
        assert_ne!(r.stopped_reason, "learned_fast_path");
        assert_eq!(writes_since(writes), Vec::<String>::new());
        assert!(!planner.prompts().is_empty() && planner.prompts().iter().all(|p| !p.contains("{{file1}}")), "{:?}", planner.prompts());
        // The replay of version 1 does not take a recipe of version 2 either, whatever the switch says.
        let ctx = learning.prepare_with(BETA, true);
        assert!(ctx.recipe.as_ref().is_some_and(|r| r.is_v2()), "the recipe is found when the switch is given");
        assert!(try_learned_fast_path(&learning, &ctx, &[], &None, &None, BETA).is_none());
        assert_eq!(writes_since(writes), Vec::<String>::new());

        // On, for a spoken request: a recipe whose first step changes something replays nothing.
        let _on = SwitchOn::new();
        let planner = Planned::new(&finals());
        let r = with_windows_caller(WindowsCaller::Voice, || run(&planner, &learning, BETA));
        assert_ne!(r.stopped_reason, "learned_fast_path");
        assert_eq!(writes_since(writes), Vec::<String>::new(), "the voice changed nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_ends_the_run_the_way_it_does_in_the_planners_loop_and_other_errors_do_not() {
        let denied = "ERROR [CONFIRMATION_DENIED] fs.write_file: The user did not approve: Write beta.txt. Do not retry this action; tell the user.";
        let (reason, answer) = replay_refusal(denied, WindowsCaller::Agent).unwrap();
        assert_eq!((reason, answer.as_str()), ("user_declined", "I stopped without doing it because you declined: Write beta.txt."));
        let blocked = "ERROR [POLICY_TOOL_BLOCKED] browser_click: a voice request can read pages but not click, type or submit.";
        assert_eq!(replay_refusal(blocked, WindowsCaller::Voice).map(|r| r.0), Some("voice_cannot_act"));
        assert!(replay_refusal(blocked, WindowsCaller::Agent).is_none(), "for the agent a block is an error to go on from");
        let refused = "ERROR [PERMISSION_DENIED] fs.write_file: this tool is on the hard-deny list";
        assert_eq!(replay_refusal(refused, WindowsCaller::Agent).map(|r| r.0), Some("policy_denied"));
        for ordinary in ["ERROR [INVALID_ARGUMENTS] fs.delete_file: no such tool is available", "ERROR [NOT_FOUND] fs.read: no such file", "fs.write_file beta.txt"] {
            assert!(replay_refusal(ordinary, WindowsCaller::Agent).is_none(), "{ordinary}");
        }
    }

    #[test]
    fn a_step_counts_as_a_change_unless_it_is_known_to_only_look() {
        let none = json!({});
        for (source, tool, changes) in [
            ("browser", "browser_click", true),
            ("browser", "browser_type", true),
            ("browser", "browser_navigate", false),
            ("browser", "browser_snapshot", false),
            ("local", "local_find", false),
            ("local", "local_open", true),
            ("local", "local_open_url", true),
            ("guard", "anything", true),
            ("", "anything", true),
        ] {
            assert_eq!(recipe_step_writes(source, tool, &none), changes, "{source} {tool}");
        }
        assert!(recipe_step_writes("browser", "browser_tabs", &json!({"action": "close"})) && !recipe_step_writes("browser", "browser_tabs", &json!({"action": "list"})));
    }

    #[test]
    fn the_answer_of_a_recipe_that_changed_something_is_its_saved_answer_with_this_goals_values_or_a_plain_sentence() {
        let bindings = vec![("text1".to_string(), "beta note".to_string()), ("file1".to_string(), "beta.txt".to_string())];
        let recipe = |answer: &str, posts: usize| LearnedRecipe {
            version: 2,
            tools: vec![crate::memory::LearnedToolCall { tool: "fs.write".into(), write: true, ..Default::default() }],
            final_answer: answer.into(),
            postconditions: (0..posts).map(|_| crate::memory::agent_learning::Postcondition::ExitCode { value: 0, after: 0 }).collect(),
            ..Default::default()
        };
        assert_eq!(replay_write_answer(&recipe("Saved {{text1}} in {{file1}}.", 0), &bindings), "Saved beta note in beta.txt.");
        assert_eq!(replay_write_answer(&recipe("", 0), &bindings), "I ran the 1 step saved for this task.", "a recipe learned from outside text has no answer");
        assert_eq!(replay_write_answer(&recipe("", 1), &bindings), "I ran the 1 step saved for this task and checked the result.");
        assert_eq!(replay_write_answer(&recipe("Saved {{other9}}.", 0), &bindings), "I ran the 1 step saved for this task.", "an answer with a value nobody gave is not used");
        assert_eq!(replay_write_answer(&recipe("done answer", 0), &bindings), "I ran the 1 step saved for this task.", "text copied out of the instructions is not an answer");
    }

    #[test]
    fn a_recipe_that_only_reads_answers_by_the_rules_of_version_1() {
        // A weather question needs a fresh reading it can turn into an answer: with none, the planner reads it.
        assert_eq!(replay_read_answer("weather in Pune", "", "", &[]), None);
        // And an answer cannot be made from nothing at all.
        assert_eq!(replay_read_answer("show the notes", "", "", &[]), None);
    }
}
