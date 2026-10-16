//! What a run keeps on the thread that runs it: the hook that hears its steps, the context and the run it goes on from, its checklist and mode, the coding record and the checks the loop ran, the cancel flags, the caller, the session of the project and the clock a helper was given. All of it is **one** [`RunContext`] held by **one** `thread_local!` (`RUN`) in this file, and every function that sets or reads a part of it is here too (card OA3-05, move only; card OA13-03, task AQ-13: the seventeen thread-local variables (sixteen outside tests) that were spread over `state.rs`, `child.rs` and `session_run.rs` became the fields of one struct, with the same accessors and the same values at the same times).
//!
//! The loop is synchronous, so a thread-local carries the context of a run to the guard without widening every function signature. What it costs is that the context is lost at a thread hop; the
//! context is therefore one value that can be named, listed and set aside whole (`set_aside` / `put_back`, which a helper run uses), and a part of it is never reached except through a function of this file. Each part
//! has a cell of its own inside the struct, as it had a `thread_local!` of its own: a function that holds one part open while it calls out cannot be stopped by another part being open.

use super::*;
use std::cell::{Cell, RefCell};
use std::time::Duration;

/// The name a run is recorded with for the caller that started it.
pub fn windows_caller_name(caller: WindowsCaller) -> &'static str {
    match caller {
        WindowsCaller::Voice => "voice",
        WindowsCaller::Api => "api",
        WindowsCaller::Agent => "agent",
    }
}

/// The caller a recorded name stands for, for a run that goes on from that one: it has the powers the first had, no more. A name nobody knows is the voice, which
/// can only look.
pub fn windows_caller_named(name: &str) -> WindowsCaller {
    match name {
        "agent" => WindowsCaller::Agent,
        "api" => WindowsCaller::Api,
        _ => WindowsCaller::Voice,
    }
}

pub(super) type StepHook = Arc<dyn Fn(&AgentStep) + Send + Sync>;

/// What a run that goes on after a question starts from: the run it continues, the steps that run had taken (the calls and what they
/// returned, as the record kept them), and how many questions have been asked in this task so far.
#[derive(Debug, Clone, Default)]
pub struct Resume {
    pub parent_run_id: String,
    pub history: Vec<(String, String)>,
    pub questions_asked: u32,
    /// Calls (by `aqua_tool_host::short_digest`) that were running when the parent stopped and may or may not have happened: a call that changes
    /// something and repeats one of them asks every time. Empty for a run that goes on after a question.
    pub unknown_digests: Vec<String>,
    /// The checklist the run before it had kept (`todo.write`): `(text, status)`. The run that goes on starts with it (card OA10-04, AG-15).
    pub checklist: Vec<(String, String)>,
    /// The lines the person had added to the run before it: the goal of the run that goes on is the goal as they made it (card OA10-04, AG-15).
    pub steered: Vec<String>,
    /// A plan the person approved (`POST /api/v1/runs/{id}/plan/approve`): the run that goes on works to it and has it recited (card OA10-04, AG-06). Up to five steps.
    pub plan: Vec<String>,
}

/// The most questions the planner may ask in one task. A third would be a person doing the work of a program that cannot decide.
pub const MAX_QUESTIONS: u32 = 2;

/// What a run is for: to do the task (`Build`), or only to read and write the plan for it (`Plan`, card OA10-04, AG-06: it ends `plan_ready` and the person approves the plan before a
/// build run starts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    #[default]
    Build,
    Plan,
}

/// Everything the loop keeps for the run on this thread (card OA13-03, task AQ-13). One per thread, reached only through the functions of this file. A new thread starts with the values of
/// [`RunContext::new`]: no hook, no context, no checklist, the mode `Build`, no turn to cancel, and the caller `Agent` (as the first `thread_local!` of the caller was; the voice and the API
/// say so with [`with_windows_caller`]).
pub(super) struct RunContext {
    /// The hook told of every step of the runs on this thread (see [`with_step_hook`]).
    step_hook: RefCell<Option<StepHook>>,
    /// The earlier turns of the conversation, as data, given to the runs on this thread after their rules (see [`with_agent_context`]).
    agent_context: RefCell<Option<String>>,
    /// The run this one goes on from (see [`with_agent_resume`]).
    resume: RefCell<Option<Resume>>,
    /// The checklist `todo.write` keeps: `(text, status)`.
    checklist: RefCell<Vec<(String, String)>>,
    /// The planner round the run is in, and how many `agent.explore` searches it has used.
    planner_round: Cell<u32>,
    explore_used: Cell<u32>,
    /// What the run is for (see [`with_run_mode`]).
    mode: Cell<RunMode>,
    /// The goal prompts are built for; the coding record is of it.
    context_goal: RefCell<String>,
    /// The coding record of the run on this thread as of the start of the round, with the goal it is of. The history the prompt is made of has no arguments, and the record needs the calls to know
    /// which of them were checks, so it is made once a round from the calls of the run and shown from here for every prompt that is built in the round (the prompt is made more than once while
    /// it is fitted to the window).
    coding_record: RefCell<Option<(String, crate::modules::coding_driver::CodingRecord)>>,
    /// The checks the loop ran by itself after the edit of this round (arguments, what the tool answered): the loop records each as a step of the run (see `take_automatic_checks`).
    automatic_checks: RefCell<Vec<(Value, String)>>,
    /// The share of a wait before asking the model again that a test on this thread really waits, in percent (0 for none).
    #[cfg(test)]
    backoff_percent: Cell<u64>,
    /// Set by the request that owns this thread's turn; it trips when that request is dropped (the caller hung up or the turn timed out), so the agent loop stops instead of carrying on unattended.
    turn_cancel: RefCell<Option<Arc<std::sync::atomic::AtomicBool>>>,
    /// Who started the run on this thread (see [`with_windows_caller`]).
    caller: Cell<WindowsCaller>,
    /// Has this run already searched with `local_find`? Then what it found is for the user to choose from, in their next request, so `local_open` is refused until then.
    searched: Cell<bool>,
    /// The run that has made a call to the browser tools (the loop of a run is one thread).
    browser_used_by: RefCell<String>,
    /// The session of the project the run is a run of (`session_run`).
    session: RefCell<Option<super::session_run::Attached>>,
    /// What is left of the clock of the run that asked, while a helper runs (the helper may take less, never more).
    clock: Cell<Option<Duration>>,
}

impl RunContext {
    const fn new() -> Self {
        RunContext {
            step_hook: RefCell::new(None),
            agent_context: RefCell::new(None),
            resume: RefCell::new(None),
            checklist: RefCell::new(Vec::new()),
            planner_round: Cell::new(0),
            explore_used: Cell::new(0),
            mode: Cell::new(RunMode::Build),
            context_goal: RefCell::new(String::new()),
            coding_record: RefCell::new(None),
            automatic_checks: RefCell::new(Vec::new()),
            #[cfg(test)]
            backoff_percent: Cell::new(100),
            turn_cancel: RefCell::new(None),
            caller: Cell::new(WindowsCaller::Agent),
            searched: Cell::new(false),
            browser_used_by: RefCell::new(String::new()),
            session: RefCell::new(None),
            clock: Cell::new(None),
        }
    }
}

thread_local! {
    static RUN: RunContext = const { RunContext::new() };
}

/// The context of the run on this thread.
fn run<R>(f: impl FnOnce(&RunContext) -> R) -> R {
    RUN.with(f)
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The step hook
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Runs `f` with `hook` told of every step of the agent runs that `f` makes on this thread, as they happen. A streamed voice turn
/// uses it to show what the agent is doing. The previous hook (if any) comes back when `f` ends, however it ends.
pub fn with_step_hook<R>(hook: StepHook, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<StepHook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            run(|r| *r.step_hook.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(run(|r| r.step_hook.borrow_mut().replace(hook)));
    f()
}

/// Tell the hook of this thread, if there is one, about a step of an agent run.
pub(crate) fn report_step(step: &AgentStep) {
    // The run record hears every step too (it writes a tool call before the tool runs: the loop announces it first).
    crate::agent_runs::observe_step(step);
    let hook = run(|r| r.step_hook.borrow().clone());
    if let Some(hook) = hook {
        hook(step);
    }
}

/// For tests outside the crate: say a step to everyone who is listening, as the loop does.
#[doc(hidden)]
pub fn report_step_for_test(step: &AgentStep) {
    report_step(step);
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The context of the conversation and the run this one goes on from
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Runs `f`, and any agent run it makes on this thread is given `context` (the earlier turns of the conversation, as data) after its rules.
/// The previous value comes back when `f` ends, however it ends.
pub fn with_agent_context<R>(context: Option<String>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_agent_context(self.0.take());
        }
    }
    let _restore = Restore(run(|r| r.agent_context.replace(context)));
    f()
}

/// The context of the conversation the runs on this thread are given (see [`with_agent_context`]).
pub(super) fn agent_context() -> Option<String> {
    run(|r| r.agent_context.borrow().clone())
}

/// Puts `context` in place of the one of the conversation on this thread.
pub(super) fn set_agent_context(context: Option<String>) {
    run(|r| *r.agent_context.borrow_mut() = context);
}

/// Runs `f` with the run this thread goes on from, when it goes on from one (see [`with_agent_resume`]). `f` must not call back into a function that changes it.
pub(super) fn with_resume<R>(f: impl FnOnce(Option<&Resume>) -> R) -> R {
    run(|r| f(r.resume.borrow().as_ref()))
}

/// A copy of the run this thread goes on from, if it goes on from one.
pub(super) fn resumed_from() -> Option<Resume> {
    with_resume(|r| r.cloned())
}

/// A run that goes on from another one (after a question, or after a stop) is a new run: it starts as tainted and as private as the one before it
/// ended, and a call that changes something and repeats one that was running when that one stopped asks every time. A run that goes on from nothing
/// takes nothing.
pub(crate) fn take_from_parent_run_in(run_id: &str, store: Option<&crate::agent_runs::AgentRunStore>) {
    let Some((previous, had_history, unknown)) = with_resume(|r| r.map(|r| (r.parent_run_id.clone(), !r.history.is_empty(), r.unknown_digests.clone()))) else { return };
    crate::agent_runs::scope::inherit_run_marks_in(&previous, had_history, store);
    if let Some(host) = crate::agent_tools::host() {
        host.note_unknown_calls(run_id, unknown);
    }
}

/// What kind of task the run this thread goes on from was (`coding`, `files`, `web` or `desktop`, as its record has it), when it goes on from one: the run that goes on is the same kind,
/// whatever the words of its goal say (card OA4-01).
pub(super) fn inherited_profile_in(store: Option<&crate::agent_runs::AgentRunStore>) -> Option<super::super::run_profile::RunProfile> {
    let parent = with_resume(|r| r.map(|r| r.parent_run_id.clone())).filter(|p| !p.is_empty())?;
    super::super::run_profile::RunProfile::parse(&store?.profile_of(&parent)?)
}

/// Runs `f`, and the agent run it makes on this thread continues `resume` (the run that ended with a question). The previous value comes
/// back when `f` ends, however it ends.
pub fn with_agent_resume<R>(resume: Option<Resume>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Resume>);
    impl Drop for Restore {
        fn drop(&mut self) {
            run(|r| *r.resume.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(run(|r| r.resume.replace(resume)));
    f()
}

/// How many questions the task the run on this thread belongs to had asked before this run (0 for a run that goes on from nothing): a run that ends with a question is the
/// `questions_asked_before() + 1`st of its task (card OA10-03: the record of the question keeps it, so that an answer given by the route goes on with the right count).
pub fn questions_asked_before() -> u32 {
    with_resume(|r| r.map_or(0, |r| r.questions_asked))
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The round, the checklist and what the loop keeps between rounds
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

pub(crate) fn set_planner_round(round: u32) {
    run(|r| {
        r.planner_round.set(round);
        if round <= 1 {
            r.explore_used.set(0);
        }
    });
}

/// The planner round the run on this thread is in.
pub(super) fn planner_round() -> u32 {
    run(|r| r.planner_round.get())
}

/// How many `agent.explore` searches the run on this thread has used.
pub(super) fn explore_used() -> u32 {
    run(|r| r.explore_used.get())
}

/// Counts the `agent.explore` searches the run has used as `used`.
pub(super) fn set_explore_used(used: u32) {
    run(|r| r.explore_used.set(used));
}

pub(crate) fn reset_checklist() {
    run(|r| r.checklist.borrow_mut().clear());
}

/// Replaces the run checklist. At most 20 items. An empty list clears it.
fn checklist_item(v: &serde_json::Value) -> Option<(String, String)> {
    if let Some(s) = v.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| (s.to_string(), "pending".into()));
    }
    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return None;
    }
    let status = v.get("status").and_then(|t| t.as_str()).unwrap_or("pending");
    let status = match status {
        "doing" | "done" => status,
        _ => "pending",
    };
    Some((text, status.to_string()))
}

pub(crate) fn replace_checklist(arguments: &serde_json::Value) -> String {
    let mut items: Vec<(String, String)> = arguments.get("items").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(checklist_item).take(20).collect()).unwrap_or_default();
    let mut seen_doing = false;
    for (_, status) in items.iter_mut() {
        if status == "doing" {
            if seen_doing {
                *status = "pending".into();
            }
            seen_doing = true;
        }
    }
    let n = items.len();
    // The record of the run keeps it too, so that a run that is gone on from has it (card OA10-04, AG-15).
    crate::agent_runs::note_checklist(&items);
    run(|r| *r.checklist.borrow_mut() = items);
    if n == 0 { "Checklist cleared.".into() } else { format!("Checklist now has {n} item(s).") }
}

/// Puts back the checklist the run before this one had kept (card OA10-04, AG-15): at most twenty items, one doing, the statuses as `todo.write` knows them. Text from a record is the model's
/// own earlier words and is shown as the checklist always is, as data the model keeps current; it widens no permission.
pub(crate) fn restore_checklist(items: &[(String, String)]) {
    let mut seen_doing = false;
    let items: Vec<(String, String)> = items
        .iter()
        .take(20)
        .map(|(text, status)| {
            let status = match status.as_str() {
                "doing" if !seen_doing => {
                    seen_doing = true;
                    "doing"
                }
                "done" => "done",
                _ => "pending",
            };
            (text.clone(), status.to_string())
        })
        .collect();
    run(|r| *r.checklist.borrow_mut() = items);
}

pub(crate) fn checklist_block() -> String {
    run(|r| {
        let items = r.checklist.borrow();
        if items.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = items.iter().enumerate().map(|(i, (s, status))| format!("{}. [{status}] {s}", i + 1)).collect();
            format!("\nChecklist (keep this current with todo.write; only one item is doing):\n{}", lines.join("\n"))
        }
    })
}

/// The most characters of the checklist that go at the end of a prompt (about 200 tokens).
pub(super) const CHECKLIST_TAIL_CHARS: usize = 700;

/// The run's checklist as words for the end of every prompt after it was written (card OA5-01: `checklist_block` goes into the start of the prompt, which is worked out once, so a list written by
/// `todo.write` in round three never reached round four). Empty while the run has no checklist. Items are cut to 60 characters, and what does not fit is counted.
pub(crate) fn checklist_tail() -> String {
    run(|r| {
        let items = r.checklist.borrow();
        if items.is_empty() {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        let mut used = 0usize;
        for (i, (text, status)) in items.iter().enumerate() {
            let part = format!("{}. [{status}] {}", i + 1, text.chars().take(60).collect::<String>());
            if used + part.chars().count() > CHECKLIST_TAIL_CHARS {
                parts.push(format!("(and {} more)", items.len() - i));
                break;
            }
            used += part.chars().count() + 2;
            parts.push(part);
        }
        format!(" Your checklist (keep it current with todo.write; one item is doing): {}.", parts.join("; "))
    })
}

/// Runs `f`, and the agent run it makes on this thread is of this mode. The previous mode comes back when `f` ends, however it ends.
pub fn with_run_mode<R>(mode: RunMode, f: impl FnOnce() -> R) -> R {
    struct Restore(RunMode);
    impl Drop for Restore {
        fn drop(&mut self) {
            run(|r| r.mode.set(self.0));
        }
    }
    let _restore = Restore(run(|r| r.mode.replace(mode)));
    f()
}

/// Is the run on this thread a plan run?
pub(crate) fn plan_run() -> bool {
    run(|r| r.mode.get()) == RunMode::Plan
}

/// The goal the prompts of the run on this thread are built for.
pub(super) fn goal_for_context() -> String {
    run(|r| r.context_goal.borrow().clone())
}

/// Sets the goal the prompts of the run on this thread are built for.
pub(super) fn set_goal_for_context(goal: &str) {
    run(|r| *r.context_goal.borrow_mut() = goal.to_string());
}

/// Keeps the coding record of the round, with the goal it is of (`None` forgets it).
pub(super) fn set_coding_record(record: Option<(String, crate::modules::coding_driver::CodingRecord)>) {
    run(|r| *r.coding_record.borrow_mut() = record);
}

/// The coding record of the round when it is of `goal`.
pub(super) fn coding_record_for(goal: &str) -> Option<crate::modules::coding_driver::CodingRecord> {
    run(|r| r.coding_record.borrow().as_ref().filter(|(g, _)| g == goal).map(|(_, record)| record.clone()))
}

/// Notes a check the loop ran by itself after an edit (arguments, what the tool answered).
pub(super) fn push_automatic_check(args: Value, observation: String) {
    run(|r| r.automatic_checks.borrow_mut().push((args, observation)));
}

/// The checks the loop ran after the edit of this round, once: they are taken, so that the next call starts with none.
pub(super) fn take_automatic_checks() -> Vec<(Value, String)> {
    run(|r| std::mem::take(&mut *r.automatic_checks.borrow_mut()))
}

/// How many percent of a wait before asking the model again this test thread really waits (100 unless the test says less).
#[cfg(test)]
pub(super) fn backoff_percent() -> u64 {
    run(|r| r.backoff_percent.get())
}

/// A test sets the share of the waits it really waits for.
#[cfg(test)]
pub(super) fn set_backoff_percent(percent: u64) {
    run(|r| r.backoff_percent.set(percent));
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

pub(super) static CANCEL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ask every agent run that is in progress to stop before its next step. A tool call that is
/// already executing finishes (it is bounded by its own timeout); the loop then ends.
pub fn cancel_running_agents() {
    CANCEL_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Host calls in flight (a build, a download, an ssh command) are stopped too.
    crate::agent_runs::scope::cancel_active();
}

/// Ties the current thread's agent runs to `flag` until the returned guard is dropped.
pub fn bind_turn_cancel(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> TurnCancelBinding {
    run(|r| *r.turn_cancel.borrow_mut() = Some(flag));
    TurnCancelBinding
}

pub struct TurnCancelBinding;
impl Drop for TurnCancelBinding {
    fn drop(&mut self) {
        run(|r| *r.turn_cancel.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn turn_cancelled_for_test() -> bool {
    turn_cancelled()
}

/// Has the turn this thread works for been told to stop (Stop, the end of a detached run, or its caller hanging up)? For the integration tests of `core/tests/`, whose scripted runs
/// stand in for the loop and have to see a stop the way the loop does (card OA10-03).
#[doc(hidden)]
pub fn turn_was_told_to_stop() -> bool {
    turn_cancelled()
}

/// How many times every running agent has been told to stop (what `cancel_running_agents` counts; the loop ends when it changes). For the same tests: a scripted run that sees it change
/// stops as the loop does.
#[doc(hidden)]
pub fn stop_count() -> u64 {
    CANCEL_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Has the turn running on this thread been told to stop? (the caller hung up, or Stop was pressed)
pub(crate) fn turn_cancelled() -> bool {
    run(|r| r.turn_cancel.borrow().as_ref().is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst)))
}

/// The flag for this thread's turn, so another thread (speech synthesis) can see the same cancel.
pub(crate) fn turn_cancel_flag() -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    run(|r| r.turn_cancel.borrow().clone())
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Who asked, and what the run has done with the browser and the local files
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Run `f` with desktop tool calls attributed to `caller` (voice can only look, never act). The previous caller comes back when `f` ends, however it ends: a panic inside
/// `f` must not leave this thread acting as the caller it was given (the voice fails closed, the agent and the API do not).
pub fn with_windows_caller<R>(caller: WindowsCaller, f: impl FnOnce() -> R) -> R {
    struct Restore(WindowsCaller);
    impl Drop for Restore {
        fn drop(&mut self) {
            run(|r| r.caller.set(self.0));
        }
    }
    let _restore = Restore(run(|r| r.caller.replace(caller)));
    f()
}

/// The caller desktop tool calls on this thread are attributed to now (see [`with_windows_caller`]). The record of a question keeps it (card OA10-03): the answer goes on as that caller.
pub(crate) fn windows_caller_now() -> WindowsCaller {
    run(|r| r.caller.get())
}

/// Has the run on this thread searched with `local_find`?
pub(super) fn searched_this_run() -> bool {
    run(|r| r.searched.get())
}

/// Says whether the run on this thread has searched with `local_find`.
pub(super) fn set_searched_this_run(searched: bool) {
    run(|r| r.searched.set(searched));
}

pub(super) fn note_browser_used() {
    let id = crate::agent_runs::scope::current_run();
    run(|r| *r.browser_used_by.borrow_mut() = id);
}

pub(super) fn browser_used_in_this_run() -> bool {
    let id = crate::agent_runs::scope::current_run();
    !id.is_empty() && run(|r| *r.browser_used_by.borrow() == id)
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The session of the project and the clock a helper was given
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Runs `f` with the cell that holds the session of the project the run on this thread is a run of (`session_run` is the only caller).
pub(super) fn with_session<R>(f: impl FnOnce(&RefCell<Option<super::session_run::Attached>>) -> R) -> R {
    run(|r| f(&r.session))
}

/// What is left of the clock of the run that asked, while a helper runs.
pub(super) fn clock() -> Option<Duration> {
    run(|r| r.clock.get())
}

/// Sets the clock a helper was given and returns the one it replaces.
pub(super) fn replace_clock(clock: Option<Duration>) -> Option<Duration> {
    run(|r| r.clock.replace(clock))
}

// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// Set aside while a helper runs
// ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What the run on this thread keeps of its own, taken out when a helper begins (card OA11-04): the helper's loop starts from nothing and leaves nothing behind. The hook that hears the
/// steps goes too (the helper's steps are not the run's), and so does the session (the helper is no run of the session). The context of the conversation, the run it goes on from, the
/// mode, the caller, the turn to cancel and the clock stay: the helper is of the same person, the same turn and the same time.
pub(super) struct SetAside {
    checklist: Vec<(String, String)>,
    planner_round: u32,
    explore_used: u32,
    context_goal: String,
    coding_record: Option<(String, crate::modules::coding_driver::CodingRecord)>,
    automatic_checks: Vec<(Value, String)>,
    searched: bool,
    browser_used_by: String,
    step_hook: Option<StepHook>,
    session: Option<super::session_run::Attached>,
}

/// Takes what [`SetAside`] holds out of the context of this thread, leaving it as a thread that has not run anything.
pub(super) fn set_aside() -> SetAside {
    run(|r| SetAside {
        checklist: std::mem::take(&mut *r.checklist.borrow_mut()),
        planner_round: r.planner_round.replace(0),
        explore_used: r.explore_used.replace(0),
        context_goal: std::mem::take(&mut *r.context_goal.borrow_mut()),
        coding_record: r.coding_record.borrow_mut().take(),
        automatic_checks: std::mem::take(&mut *r.automatic_checks.borrow_mut()),
        searched: r.searched.replace(false),
        browser_used_by: std::mem::take(&mut *r.browser_used_by.borrow_mut()),
        step_hook: r.step_hook.borrow_mut().take(),
        session: r.session.borrow_mut().take(),
    })
}

/// Gives back what [`set_aside`] took, in place of whatever the helper left.
pub(super) fn put_back(mut aside: SetAside) {
    run(|r| {
        *r.checklist.borrow_mut() = std::mem::take(&mut aside.checklist);
        r.planner_round.set(aside.planner_round);
        r.explore_used.set(aside.explore_used);
        *r.context_goal.borrow_mut() = std::mem::take(&mut aside.context_goal);
        *r.coding_record.borrow_mut() = aside.coding_record.take();
        *r.automatic_checks.borrow_mut() = std::mem::take(&mut aside.automatic_checks);
        r.searched.set(aside.searched);
        *r.browser_used_by.borrow_mut() = std::mem::take(&mut aside.browser_used_by);
        *r.step_hook.borrow_mut() = aside.step_hook.take();
        *r.session.borrow_mut() = aside.session.take();
    });
}

#[cfg(test)]
mod tests {
    //! The context of a run (card OA13-03, task AQ-13): what a thread starts with, what it does not share with another, what a helper sets aside and gets back whole, and what is put back
    //! when a run panics.

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn items(texts: &[&str]) -> Vec<(String, String)> {
        texts.iter().map(|t| (t.to_string(), "pending".to_string())).collect()
    }

    fn record() -> (String, crate::modules::coding_driver::CodingRecord) {
        ("fix a.rs".to_string(), crate::modules::coding_driver::CodingRecord::build("fix a.rs", &[], &[], &[]))
    }

    #[test]
    fn a_thread_that_nobody_set_up_starts_as_the_agent_with_nothing_else() {
        // The caller that nobody named is the agent (the `thread_local!` of the caller started at `Agent`; the voice and the API say so with `with_windows_caller`).
        let seen = std::thread::spawn(|| {
            (
                (windows_caller_now(), agent_context(), resumed_from().is_some(), checklist_block(), checklist_tail(), plan_run(), turn_cancelled(), turn_cancel_flag().is_some()),
                (goal_for_context(), searched_this_run(), explore_used(), planner_round(), clock(), with_session(|s| s.borrow().is_some()), coding_record_for("").is_none(), take_automatic_checks().is_empty()),
                questions_asked_before(),
            )
        })
        .join()
        .unwrap();
        assert_eq!(seen.0, (WindowsCaller::Agent, None, false, String::new(), String::new(), false, false, false));
        assert_eq!(seen.1, (String::new(), false, 0, 0, None, false, true, true));
        assert_eq!(seen.2, 0);
    }

    #[test]
    fn the_context_of_one_thread_is_not_seen_by_another_and_is_lost_at_a_thread_hop() {
        with_windows_caller(WindowsCaller::Voice, || {
            with_agent_context(Some("earlier turns".into()), || {
                restore_checklist(&items(&["read the file"]));
                set_goal_for_context("fix a.rs");
                assert_eq!(windows_caller_now(), WindowsCaller::Voice);
                assert_eq!(agent_context().as_deref(), Some("earlier turns"));
                // A thread the loop hops to has none of it: the voice is not carried over, which is what "the context is lost at a thread hop" says (and why whoever spawns a thread
                // for a run binds its caller again).
                let seen = std::thread::spawn(|| (windows_caller_now(), agent_context(), checklist_block(), goal_for_context())).join().unwrap();
                assert_eq!(seen, (WindowsCaller::Agent, None, String::new(), String::new()));
                assert!(checklist_block().contains("read the file"));
            });
            assert_eq!(agent_context(), None, "the context of the conversation is put back");
        });
        assert_eq!(windows_caller_now(), WindowsCaller::Agent);
        reset_checklist();
        set_goal_for_context("");
    }

    #[test]
    fn what_a_helper_sets_aside_comes_back_whole_and_the_helper_starts_from_nothing() {
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook: StepHook = {
            let heard = heard.clone();
            Arc::new(move |_| {
                heard.fetch_add(1, Ordering::SeqCst);
            })
        };
        let step = AgentStep { step: 1, kind: "tool".into(), detail: serde_json::json!({}) };
        with_step_hook(hook, || {
            with_agent_context(Some("the conversation".into()), || {
                with_run_mode(RunMode::Plan, || {
                    restore_checklist(&items(&["one", "two"]));
                    set_planner_round(3);
                    set_explore_used(2);
                    set_goal_for_context("fix a.rs");
                    set_coding_record(Some(record()));
                    push_automatic_check(serde_json::json!({"command": "cargo check"}), "ok".into());
                    set_searched_this_run(true);
                    let _ = replace_clock(Some(Duration::from_secs(40)));

                    let aside = set_aside();
                    // A helper's loop starts from nothing: no checklist, round 0, nothing searched, no record, no checks, no hook (its steps are not the run's) ...
                    assert_eq!((checklist_block(), checklist_tail(), planner_round(), explore_used(), goal_for_context(), searched_this_run()), (String::new(), String::new(), 0, 0, String::new(), false));
                    assert!(coding_record_for("fix a.rs").is_none() && take_automatic_checks().is_empty());
                    let before = heard.load(Ordering::SeqCst);
                    report_step(&step);
                    assert_eq!(heard.load(Ordering::SeqCst), before, "the helper's step went to the hook of the run");
                    // ... while what is the same person's and the same turn's stays: the conversation, the mode, the caller, the clock.
                    assert_eq!((agent_context().as_deref(), plan_run(), windows_caller_now(), clock()), (Some("the conversation"), true, WindowsCaller::Agent, Some(Duration::from_secs(40))));
                    // The helper does its own work and leaves it behind.
                    restore_checklist(&items(&["the helper's item"]));
                    set_planner_round(1);
                    set_searched_this_run(true);
                    put_back(aside);

                    assert!(checklist_block().contains("1. [pending] one") && checklist_block().contains("2. [pending] two") && !checklist_block().contains("helper"), "{}", checklist_block());
                    assert_eq!((planner_round(), explore_used(), goal_for_context(), searched_this_run()), (3, 2, "fix a.rs".to_string(), true));
                    assert!(coding_record_for("fix a.rs").is_some() && coding_record_for("another goal").is_none());
                    assert_eq!(take_automatic_checks(), vec![(serde_json::json!({"command": "cargo check"}), "ok".to_string())]);
                    assert!(take_automatic_checks().is_empty(), "the checks are taken once");
                    let before = heard.load(Ordering::SeqCst);
                    report_step(&step);
                    assert_eq!(heard.load(Ordering::SeqCst), before + 1, "the hook of the run hears its steps again");
                    // Round 1 is the start of a run: it resets the searches the run has used.
                    set_planner_round(1);
                    assert_eq!(explore_used(), 0);
                    let _ = replace_clock(None);
                    reset_checklist();
                    set_goal_for_context("");
                    set_coding_record(None);
                    set_searched_this_run(false);
                });
            });
        });
    }

    #[test]
    fn what_a_run_changed_on_its_thread_is_put_back_when_it_panics() {
        let flag = Arc::new(AtomicBool::new(false));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_windows_caller(WindowsCaller::Voice, || {
                with_run_mode(RunMode::Plan, || {
                    with_agent_context(Some("context".into()), || {
                        with_agent_resume(Some(Resume { parent_run_id: "r1".into(), questions_asked: 1, ..Default::default() }), || {
                            let _bound = bind_turn_cancel(flag.clone());
                            assert_eq!((questions_asked_before(), plan_run(), windows_caller_now()), (1, true, WindowsCaller::Voice));
                            flag.store(true, Ordering::SeqCst);
                            assert!(turn_cancelled());
                            panic!("the run failed");
                        })
                    })
                })
            })
        }));
        assert!(caught.is_err());
        // The thread does not go on acting as the voice, in plan mode, bound to a turn that is over (the voice fails closed, and nothing else may be left behind either).
        assert_eq!((windows_caller_now(), plan_run(), agent_context(), questions_asked_before(), turn_cancelled(), turn_cancel_flag().is_some()), (WindowsCaller::Agent, false, None, 0, false, false));
        assert!(!with_resume(|r| r.is_some()));
    }
}
