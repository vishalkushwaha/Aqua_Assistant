//! Runs one workflow: its steps in order, each one's result available to the ones after it.
//!
//! The engine knows nothing about tools or models; it talks to an [`Executor`]. That keeps the rules
//! that matter (order, templating, retries, what stops a run, what is written to the log) testable
//! without a model or a browser.
//!
//! Rules:
//! - A step that fails is retried up to its `retries`, waiting `retry_delay * 2^n` (at most 5 minutes)
//!   between tries. Failures that cannot get better by trying again (a refused approval, a policy
//!   block, bad arguments) are not retried.
//! - After the last try, `on_failure` decides: `stop` ends the run and skips the rest; `continue`
//!   records the failure and moves on (the run then ends `partial`).
//! - What is written to the history is redacted (`sanitize::redact`) and capped. What is passed to the
//!   next step is the executor's own output, which the tool host has already masked.
//! - A failure to write the run's record is not ignored (card R6-14): a step that cannot be written down as started is not started, and a run whose record cannot be
//!   kept starts nothing more and ends failed.
//! - A tool call that failed with its answer lost (`maybe_done`: a timeout, a connection dropped after the request went out) may have been done already, so it is not
//!   tried again, unless the executor says the tool only reads.
//! - **What a run has read from outside is carried from step to step** (card OA8-04, SE-12; A6-17). Every step of one run is called with one run key, so what the tool host keeps by
//!   run (that the run has read text from outside, and so what it may change without asking) is not lost between the steps. An agent step is a run of its own: it is given the key
//!   too, starts as a run that has read outside text when the run has or when its goal is made from a step's output or a webhook's payload, and what it reads is carried back to the
//!   run. A step that is given what a webhook sent has the run count as having read outside text from then on. A later step that changes something therefore asks, and a run nobody
//!   watches is refused it with the usual reason.

use super::model::*;
use super::store::{now, WorkflowStore};
use super::template::{self, Target};
use crate::memory::experience::sanitize::redact;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_STORED_CHARS: usize = 20_000;
const MAX_BACKOFF_SECS: u64 = 300;

/// What a step produced.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub ok: bool,
    pub output: String,
    /// Structured result (a tool's `data`), reachable as `steps.<id>.data.<field>`.
    pub data: Value,
    pub error: String,
    /// Whether trying again could help.
    pub retryable: bool,
    /// The step failed, but what it asked for may already have happened (a request that timed out or dropped after it was sent).
    pub maybe_done: bool,
}

impl Outcome {
    pub fn success(output: impl Into<String>, data: Value) -> Self {
        Self { ok: true, output: output.into(), data, ..Default::default() }
    }
    pub fn failure(error: impl Into<String>, retryable: bool) -> Self {
        Self { ok: false, error: error.into(), retryable, ..Default::default() }
    }
}

pub trait Executor: Send + Sync {
    fn run_tool(&self, tool: &str, arguments: &Value, timeout: Duration, cancel: &AtomicBool) -> Outcome;
    fn run_agent(&self, goal: &str, max_steps: u32, timeout: Duration, cancel: &AtomicBool) -> Outcome;

    /// [`Executor::run_agent`] for a step of the run named `run_key` (card OA8-04, SE-12). `outside_text`: the goal is made from what an earlier step produced or a webhook sent. The
    /// agent is a run of its own, so an executor that keeps what a run has read (the tool host does) starts it as a run that has read outside text when the run named `run_key` has,
    /// or when `outside_text` is set, and counts what the agent read as read by the run: what it hands on is text from outside. An executor with nothing to keep says nothing.
    fn run_agent_in(&self, _run_key: &str, goal: &str, max_steps: u32, timeout: Duration, cancel: &AtomicBool, _outside_text: bool) -> Outcome {
        self.run_agent(goal, max_steps, timeout, cancel)
    }

    /// The run named `run_key` has been given text from outside that no tool of the host returned (what a webhook sent): from now on it counts as having read it.
    fn note_outside_text(&self, _run_key: &str) {}

    /// [`Executor::run_tool`] for a step of the run named `run_key`. Every tool step of one run is called with the same key, so what the tool host keeps by run
    /// (that the run has read text from outside, and so what it may change without asking) is kept across the steps and not lost between them (card R6-11).
    /// An executor with nothing to keep by run does not need to say anything.
    fn run_tool_in(&self, _run_key: &str, tool: &str, arguments: &Value, timeout: Duration, cancel: &AtomicBool) -> Outcome {
        self.run_tool(tool, arguments, timeout, cancel)
    }

    /// The run named `run_key` has ended: what was kept for it can go.
    fn run_ended(&self, _run_key: &str) {}

    /// Does this tool only read? A tool that is not known to (or an executor that cannot say) counts as one that writes: a write whose answer was lost is not sent again
    /// (card R6-14, T6-14.3).
    fn tool_is_read_only(&self, _tool: &str) -> bool {
        false
    }
}

/// Waits `secs` seconds; returns false if the run was cancelled meanwhile.
pub type Sleeper = dyn Fn(u64, &AtomicBool) -> bool + Send + Sync;

pub fn real_sleep(secs: u64, cancel: &AtomicBool) -> bool {
    for _ in 0..secs.saturating_mul(4) {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !cancel.load(Ordering::SeqCst)
}

pub struct Env<'a> {
    pub store: &'a WorkflowStore,
    pub exec: &'a dyn Executor,
    pub cancel: &'a AtomicBool,
    pub sleep: &'a (dyn Fn(u64, &AtomicBool) -> bool + Send + Sync + 'a),
}

/// What is written to the history: no secrets, and never more than a page.
pub fn stored_text(text: &str) -> String {
    let (clean, _) = redact(text);
    if clean.chars().count() > MAX_STORED_CHARS {
        format!("{}... (cut, {} characters in all)", clean.chars().take(MAX_STORED_CHARS).collect::<String>(), clean.chars().count())
    } else {
        clean
    }
}

fn base_context(w: &Workflow, trigger_kind: &str, payload: &Value) -> Value {
    let local = chrono::Local::now();
    json!({
        "workflow": { "id": w.id, "name": w.name },
        "trigger": { "kind": trigger_kind, "payload": payload },
        "now": { "date": local.format("%Y-%m-%d").to_string(), "time": local.format("%H:%M").to_string(), "iso": local.to_rfc3339() },
        "steps": {},
    })
}

fn set_step_result(ctx: &mut Value, id: &str, output: &str, data: &Value, ok: bool, attempts: u32, skipped: bool) {
    if let Some(steps) = ctx.get_mut("steps").and_then(|s| s.as_object_mut()) {
        steps.insert(id.to_string(), json!({ "output": output, "data": data, "ok": ok, "attempts": attempts, "skipped": skipped }));
    }
}

/// The name under which the steps of one workflow run are known to the tool host.
fn run_key(run_id: i64) -> String {
    format!("workflow_run_{run_id}")
}

fn run_once(env: &Env, step: &Step, ctx: &Value, run_key: &str) -> Outcome {
    let timeout = Duration::from_secs(if step.timeout_secs == 0 { DEFAULT_TIMEOUT_SECS } else { step.timeout_secs });
    match &step.kind {
        StepKind::Tool { tool, arguments } => {
            let args = if arguments.is_null() { json!({}) } else { arguments.clone() };
            match template::render_json(&args, ctx) {
                Ok(rendered) => env.exec.run_tool_in(run_key, tool, &rendered, timeout, env.cancel),
                Err(e) => Outcome::failure(format!("could not fill in the arguments: {e}"), false),
            }
        }
        StepKind::Agent { goal, max_steps } => match template::render(goal, ctx, Target::AgentGoal) {
            Ok(mut g) => {
                let outside = template::uses_outside_data(goal);
                if outside {
                    g.push_str(template::DATA_NOTE);
                }
                env.exec.run_agent_in(run_key, &g, *max_steps, timeout, env.cancel, outside)
            }
            Err(e) => Outcome::failure(format!("could not fill in the goal: {e}"), false),
        },
        StepKind::Set { value } => match template::render(value, ctx, Target::Plain) {
            Ok(text) => Outcome::success(text, Value::Null),
            Err(e) => Outcome::failure(format!("could not fill in the text: {e}"), false),
        },
    }
}

/// Run every step of `w` for the run `run_id` (already claimed in the store) and record the result.
pub fn execute(env: &Env, w: &Workflow, run_id: i64, trigger_kind: &str, payload: &Value) -> RunStatus {
    let status = execute_steps(env, w, run_id, trigger_kind, payload);
    env.exec.run_ended(&run_key(run_id));
    status
}

/// Keeps the first failure to write down what a run did (a full disk, a locked or damaged database). A run whose record cannot be kept cannot be trusted or looked at
/// afterwards, and a step done without a line to say so is how it would be done twice, so the run starts nothing more (card R6-14, T6-14.3).
fn kept(first: &mut Option<String>, run_id: i64, what: &str, result: Result<(), String>) {
    if let Err(e) = result {
        tracing::error!("workflow run {run_id}: {what} could not be saved: {e}");
        first.get_or_insert(e);
    }
}

fn execute_steps(env: &Env, w: &Workflow, run_id: i64, trigger_kind: &str, payload: &Value) -> RunStatus {
    let mut ctx = base_context(w, trigger_kind, payload);
    let mut had_error = false;
    let mut failure: Option<String> = None;
    let mut save_error: Option<String> = None;
    let store = env.store;

    for (i, step) in w.steps.iter().enumerate() {
        let idx = i as u32;
        let label = if step.name.trim().is_empty() { step.id.clone() } else { step.name.clone() };
        let skip_rest = |from: usize, why: &str| {
            for j in from..w.steps.len() {
                let _ = store.step_finished(run_id, j as u32, "skipped", "", why, now());
            }
        };
        if let Some(e) = save_error.take() {
            skip_rest(i, "the run's record could not be saved");
            failure = Some(format!("the record of the run could not be saved ({e}), so the run was stopped before step {} ({label})", i + 1));
            break;
        }
        if env.cancel.load(Ordering::SeqCst) {
            skip_rest(i, "the run was cancelled");
            let _ = store.finish_run(run_id, RunStatus::Cancelled, "cancelled", now());
            return RunStatus::Cancelled;
        }

        if let Some(cond) = step.only_if.as_deref().filter(|c| !c.trim().is_empty()) {
            match template::condition(cond, &ctx) {
                Ok(false) => {
                    kept(&mut save_error, run_id, "a skipped step", store.step_finished(run_id, idx, "skipped", "", "the condition was not met", now()));
                    set_step_result(&mut ctx, &step.id, "", &Value::Null, false, 0, true);
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    let msg = format!("could not evaluate the condition: {e}");
                    kept(&mut save_error, run_id, "a step that failed", store.step_finished(run_id, idx, "failed", "", &stored_text(&msg), now()));
                    set_step_result(&mut ctx, &step.id, "", &Value::Null, false, 0, false);
                    if step.on_failure == OnFailure::Stop {
                        skip_rest(i + 1, "an earlier step failed");
                        failure = Some(format!("step {} ({label}) failed: {msg}", i + 1));
                        break;
                    }
                    had_error = true;
                    continue;
                }
            }
        }

        // A step made from what a webhook sent is fed with text from outside, and so is the run from then on (card OA8-04, SE-12).
        if !payload.is_null() && step_uses_trigger(step) {
            env.exec.note_outside_text(&run_key(run_id));
        }
        // A step that cannot be written down as started is not started: a write done without a line that says so is how it would be done twice.
        kept(&mut save_error, run_id, "the start of a step", store.step_started(run_id, idx, now()));
        if let Some(e) = save_error.take() {
            skip_rest(i, "the run's record could not be saved");
            failure = Some(format!("step {} ({label}) was not started, because the record of the run could not be saved: {e}", i + 1));
            break;
        }
        let mut attempts = 0u32;
        let mut cancelled = false;
        let outcome = loop {
            attempts += 1;
            kept(&mut save_error, run_id, "an attempt", store.step_attempt(run_id, idx, attempts));
            if let Some(e) = &save_error {
                break Outcome::failure(format!("it was not run, because the record of the run could not be saved: {e}"), false);
            }
            let mut out = run_once(env, step, &ctx, &run_key(run_id));
            // A write whose answer was lost (a timeout, a connection dropped after the request went out) may have been done already: it is not sent a second time on the
            // strength of a guess. A tool that only reads is still tried again.
            if !out.ok && out.retryable && out.maybe_done && attempts <= step.retries && !is_read_only(env.exec, step) {
                out.retryable = false;
                out.error = format!("{} The request may already have been carried out, so it was not repeated.", out.error);
            }
            if out.ok || !out.retryable || attempts > step.retries {
                break out;
            }
            let delay = (step.retry_delay_secs.saturating_mul(1u64 << (attempts - 1).min(10))).min(MAX_BACKOFF_SECS);
            if !(env.sleep)(delay, env.cancel) {
                cancelled = true;
                break out;
            }
        };

        let status = if outcome.ok { "succeeded" } else { "failed" };
        kept(&mut save_error, run_id, "the result of a step", store.step_finished(run_id, idx, status, &stored_text(&outcome.output), &stored_text(&outcome.error), now()));
        set_step_result(&mut ctx, &step.id, &outcome.output, &outcome.data, outcome.ok, attempts, false);

        // A step that failed because the run was cancelled did not "fail".
        if cancelled || (!outcome.ok && env.cancel.load(Ordering::SeqCst)) {
            skip_rest(i + 1, "the run was cancelled");
            let _ = store.finish_run(run_id, RunStatus::Cancelled, "cancelled", now());
            return RunStatus::Cancelled;
        }
        if !outcome.ok {
            match step.on_failure {
                OnFailure::Stop => {
                    skip_rest(i + 1, "an earlier step failed");
                    failure = Some(format!("step {} ({label}) failed: {}", i + 1, stored_text(&outcome.error)));
                    break;
                }
                OnFailure::Continue => had_error = true,
            }
        }
    }

    // The last step's line could not be written: the run cannot say what it did, so it does not say that it succeeded.
    if let Some(e) = save_error.take() {
        failure.get_or_insert_with(|| format!("the record of the run could not be saved ({e}), so what the last step did is not written down"));
    }
    let (status, error) = match failure {
        Some(e) => (RunStatus::Failed, e),
        None if had_error => (RunStatus::Partial, "a step failed but the workflow was set to continue".to_string()),
        None => (RunStatus::Succeeded, String::new()),
    };
    // If even this cannot be written the run stays "running" in the database, and the next start marks it interrupted (`recover_interrupted`).
    if let Err(e) = store.finish_run(run_id, status, &error, now()) {
        tracing::error!("workflow run {run_id}: its end ({}) could not be saved: {e}", status.as_str());
    }
    status
}

/// Does this step read the trigger's payload (`{{trigger.payload.x}}`) in its arguments, its goal or its text?
fn step_uses_trigger(step: &Step) -> bool {
    fn json_uses_trigger(v: &Value) -> bool {
        match v {
            Value::String(s) => template::uses_trigger(s),
            Value::Array(items) => items.iter().any(json_uses_trigger),
            Value::Object(fields) => fields.values().any(json_uses_trigger),
            _ => false,
        }
    }
    match &step.kind {
        StepKind::Tool { arguments, .. } => json_uses_trigger(arguments),
        StepKind::Agent { goal, .. } => template::uses_trigger(goal),
        StepKind::Set { value } => template::uses_trigger(value),
    }
}

/// Is this step a call of a tool that only reads, as the executor knows it? Anything else (an agent, a tool it cannot place) is a step that may write.
fn is_read_only(exec: &dyn Executor, step: &Step) -> bool {
    matches!(&step.kind, StepKind::Tool { tool, .. } if exec.tool_is_read_only(tool))
}
