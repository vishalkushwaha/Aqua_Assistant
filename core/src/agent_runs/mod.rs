//! A durable record of every agent run (`data/agent_runs.db`), and the trace the API reads it back as.
//!
//! The agent loop already tells whoever is listening about each step (`browser_agent::report_step`). [`Recording`] listens on the
//! thread the run is on and writes what it hears, so no part of the loop has to know about the database:
//!
//! * a tool call is written as `started` **before** the tool runs (the loop announces it first) and completed after;
//! * every planner reply, every tool call and every answer is one row, with the arguments redacted and the observation cut to a
//!   digest and its first 500 characters (full text only with `AQUA_RUN_CAPTURE=full`);
//! * a failure to write is logged once and never stops a run: the record is a log, not a gate;
//! * on start-up, runs an earlier process left going are marked `interrupted` (see `store::AgentRunStore::mark_interrupted`).
//!
//! The same listener writes the **events** of the run (card OA10-01, `events.rs`): `run.started`, `step`, `tool.started` and `tool.finished` (with the number of the step row as
//! `call_id`), `usage`, `limit`, `question`, `text`, `run.finished`. Each is a row of `run_events` with its number inside the run, and `api_gateway::run_api` serves them as a page and as
//! a stream that can be taken up again from any number. The questions the person is asked reach the log through the confirmation broker (`note_question_opened`, `note_question_closed`).

pub mod bundle;
pub mod continuation;
pub mod events;
pub mod export;
pub mod live;
pub mod answer;
pub mod otel;
pub mod resume;
pub mod safety;
pub mod scope;
pub mod session;
pub mod sessions;
pub mod taxonomy;
pub mod store;
pub mod trace;
pub mod transcript;

pub use events::Event;
pub use store::{status, AgentRunStore, NewRun, NewStep, QuestionRecord, RunEnd, StepOutcome};

use crate::modules::browser_agent::AgentStep;
use aqua_mcp_adapters::browser::ConfirmRequest;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tracing::warn;

/// How much of an observation is kept in a row (the digest covers the rest).
pub const OBSERVATION_HEAD_CHARS: usize = 500;
const FULL_CAPTURE_CHARS: usize = 12_000;
const ARGS_CHARS: usize = 2_000;
const GOAL_CHARS: usize = 600;
/// The goal as `run.started` tells it: the first part only.
const GOAL_HEAD_CHARS: usize = 200;
const ANSWER_CHARS: usize = 600;
/// The answer as the `text` events carry it: more than the head the record keeps.
const TEXT_ANSWER_CHARS: usize = 4_000;
const REPLY_CHARS: usize = 800;

static STORE: OnceLock<Arc<AgentRunStore>> = OnceLock::new();

/// Opens `data/agent_runs.db`, marks the runs an earlier process left going as interrupted and prunes old ones. A database that cannot be
/// opened leaves runs unrecorded (and says so once); it never stops the core from starting.
pub fn init(data_dir: &Path) -> Result<(), String> {
    let store = Arc::new(AgentRunStore::open(data_dir.join("agent_runs.db"))?);
    let now = now_ms();
    match store.mark_interrupted(now) {
        Ok(0) => {}
        Ok(n) => warn!("{n} agent run(s) were still going when Aqua last stopped: marked interrupted"),
        Err(e) => warn!("{e}"),
    }
    // A run in the background that was stopped by the restart is offered to the person, not started: it may have changed something.
    if let Ok(runs) = store.list_resumable(50) {
        let background = runs.iter().filter(|r| r["route"] == "background").count();
        if background > 0 {
            warn!("{background} background run(s) stopped when Aqua last did and can be gone on from: GET /api/v1/runs?resumable=1");
        }
    }
    // A question an earlier process asked is gone from memory: nobody can answer it any more.
    match store.expire_waiting(now, 0) {
        Ok(0) => {}
        Ok(n) => warn!("{n} question(s) of an earlier run were never answered: marked expired"),
        Err(e) => warn!("{e}"),
    }
    if let Err(e) = store.prune(now, store::RETENTION_MS, store::RETENTION_RUNS) {
        warn!("{e}");
    }
    // The records of coding sessions that are older than thirty days, or beyond the newest fifty, are deleted for good (card OA10-04, CX-08).
    if let Err(e) = store.prune_coding_sessions(now) {
        warn!("{e}");
    }
    let _ = STORE.set(store);
    // The questions the person is asked are events of the run they are about.
    install_question_observer();
    // "New topic", "sleep" and the other things that clear the conversation close the open coding sessions (their records stay, closed, for the person to list or delete).
    crate::conversation::set_on_clear(close_coding_sessions_on_clear);
    Ok(())
}

/// What `conversation::clear` tells this module (see [`init`]).
fn close_coding_sessions_on_clear() {
    if let Some(store) = store() {
        if let Err(e) = store.close_open_coding_sessions() {
            warn!("{e}");
        }
    }
}

/// The confirmation broker tells the log when it opens a question and when it ends one (the broker lives in `aqua-mcp-adapters` and knows nothing of the log).
pub fn install_question_observer() {
    use aqua_mcp_adapters::browser::{set_question_observer, QuestionNote};
    set_question_observer(|note| {
        let Some(store) = store() else { return };
        match note {
            QuestionNote::Opened(request) => note_question_opened(store, request),
            QuestionNote::Closed { request, outcome } => note_question_closed(store, request, outcome),
        }
    });
}

pub fn store() -> Option<&'static Arc<AgentRunStore>> {
    STORE.get()
}

/// Something that needs the person, or that they would want to hear of (card OA10-03, UX-13): a question of the host opened or ended. The gateway installs a sink that turns these into short
/// spoken sentences (`api_gateway::attention`); a run does not know of it, and nothing here waits for it. (The end of a detached run, and with it a question of the planner that it ended with, is
/// told by the host of detached runs, which knows when one ends.)
#[derive(Debug, Clone, Copy)]
pub enum Attention<'a> {
    /// A question of the host about a call is open: the person's yes is waited for. `request.tool` says what the call is; `run_id` is the run it belongs to.
    ApprovalOpened { run_id: &'a str, request: &'a ConfirmRequest },
    /// The question ended (answered, timed out or taken back).
    ApprovalClosed { run_id: &'a str, request: &'a ConfirmRequest },
}

type AttentionSink = Box<dyn Fn(&Attention<'_>) + Send + Sync>;

static ATTENTION: OnceLock<AttentionSink> = OnceLock::new();

/// Sets the one sink of the process. The first call wins; `false` when one was set already.
pub fn set_attention_sink(sink: impl Fn(&Attention<'_>) + Send + Sync + 'static) -> bool {
    ATTENTION.set(Box::new(sink)).is_ok()
}

fn attend(what: Attention<'_>) {
    if let Some(sink) = ATTENTION.get() {
        sink(&what);
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Whether the full text of arguments and observations is kept (a debugging switch: `AQUA_RUN_CAPTURE=full`). Secrets are masked either way.
pub fn capture_full() -> bool {
    matches!(std::env::var("AQUA_RUN_CAPTURE").ok().as_deref().map(str::trim), Some("full"))
}

/// The state a run ends in, from why it stopped.
pub fn status_for(stopped_reason: &str) -> &'static str {
    match stopped_reason {
        // (`plan_ready`: a plan run wrote its plan and waits for the person to approve it, card OA10-04, AG-06; it is over, and what it came to is a plan, not a question.)
        "final" | "scripted_open" | "scripted_open_app" | "scripted_mail_send" | "local_search" | "local_answer" | "weather" | "learned_fast_path" | "app_fail_web_fallback" | "plan_ready" => status::COMPLETED,
        "cancelled" | "client_disconnected" | "user_declined" | "needs_login" => status::CANCELLED,
        "waiting_user" => status::WAITING_USER,
        _ => status::FAILED,
    }
}

fn hex_digest(s: &str, len: usize) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..len].to_string()
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Masks anything that looks like a credential, then cuts. In that order: a secret that straddles the cut must not survive as a prefix.
fn clean_text(s: &str, max: usize) -> String {
    cut(&aqua_tool_host::output::Redactor::new().redact(s), max)
}

fn clean_args(args: &Value, full: bool) -> String {
    let shaped = if full { args.clone() } else { crate::agent_tools::redact_args(args) };
    cut(&aqua_tool_host::output::Redactor::new().redact_json(&shaped).to_string(), if full { FULL_CAPTURE_CHARS } else { ARGS_CHARS })
}

fn is_denial(obs: &str) -> bool {
    ["ERROR [CONFIRMATION_DENIED]", "ERROR [PERMISSION_DENIED]", "ERROR [POLICY_"].iter().any(|p| obs.starts_with(p)) || obs.contains("[CONFIRMATION_DENIED]")
}

struct Open {
    seq: i64,
    since: Instant,
}

struct Active {
    store: Arc<AgentRunStore>,
    run_id: String,
    open: Option<Open>,
    plan_since: Option<Instant>,
    model_seen: bool,
    full: bool,
    tool_rows: u32,
    warned: bool,
    /// What was last written of what the run holds (read untrusted text, holds private data).
    marks_written: Option<(bool, bool)>,
    /// The number of the last event written to the log of the run (0: none yet).
    last_seq: i64,
    events_warned: bool,
    /// The session the run is in (card OA10-04): its own id, the session of the run it goes on from, or the one the request named.
    session_id: String,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    /// Who asked for the runs this thread starts (see [`with_origin`]).
    static ORIGIN: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
    /// The session the request named for the runs this thread starts (see [`with_session`]).
    static SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `f`, and every agent run it starts on this thread is put in the session `session_id` (a name the client gave, see `sessions::valid_session_id`), unless the run goes on from another
/// run, which is in that run's session. `None` leaves each run in its own session. The previous setting comes back when `f` ends, however it ends.
pub fn with_session<R>(session_id: Option<String>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SESSION.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SESSION.with(|s| std::mem::replace(&mut *s.borrow_mut(), session_id)));
    f()
}

/// The session the request that started this thread's run named, if it named one.
pub fn requested_session() -> Option<String> {
    SESSION.with(|s| s.borrow().clone())
}

/// The session of the run being recorded on this thread: what the loop asks to find the record of the session the run belongs to.
pub fn current_session_id() -> Option<String> {
    ACTIVE.with(|a| a.borrow().as_ref().map(|active| active.session_id.clone()))
}

/// The store the run being recorded on this thread is written in: a helper that the run starts (card OA11-04) is written in the same one, in the same session.
pub(crate) fn current_store() -> Option<Arc<AgentRunStore>> {
    ACTIVE.with(|a| a.borrow().as_ref().map(|active| active.store.clone()))
}

/// Moves the run being recorded on this thread to another session (a coding run that joins the open session of its project), and says so in its record.
pub fn move_to_session(session_id: &str) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_run_session(&active.run_id, session_id, "") {
                active.warn_once(e);
            } else {
                active.session_id = session_id.to_string();
            }
        }
    });
}

/// Writes the folder of the project the run on this thread works in into its record (the session list shows it, and can be asked for the sessions of one folder).
pub fn note_directory(directory: &str) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            let shown = clean_text(directory, 300);
            if let Err(e) = active.store.set_run_session(&active.run_id, &active.session_id, &shown) {
                active.warn_once(e);
            }
        }
    });
}

/// Keeps the checklist of the run on this thread (`todo.write`, card OA10-04, AG-15), so that a run that is gone on from has it.
pub fn note_checklist(items: &[(String, String)]) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_checklist(&active.run_id, items) {
                active.warn_once(e);
            }
        }
    });
}

/// Keeps what the person added to the run on this thread while it went (the lines `steer` gave it), so that a run that is gone on from has them (card OA10-04, AG-15).
pub fn note_steered(lines: &[String]) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_steered(&active.run_id, lines) {
                active.warn_once(e);
            }
        }
    });
}

/// The plan a plan run wrote is ready for the person's approval (card OA10-04, AG-06).
pub fn note_plan_ready(plan: &str) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_plan_ready(&active.run_id, plan, now_ms()) {
                active.warn_once(e);
            }
        }
    });
}

/// Runs `f`, and every agent run it starts on this thread is recorded as asked for by the person's own app (`ui_token`: the UI token) or by a script (the API token). The log of a run
/// asked for with the API token can be read with the API token; every other run's can be read with the UI token only. The previous setting comes back when `f` ends, however it ends.
pub fn with_origin<R>(ui_token: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(&'static str);
    impl Drop for Restore {
        fn drop(&mut self) {
            ORIGIN.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(ORIGIN.with(|o| o.replace(if ui_token { "ui" } else { "api" })));
    f()
}

pub(crate) fn current_origin() -> &'static str {
    ORIGIN.with(|o| o.get())
}

/// The run being recorded on this thread and the number of the last event of its log, if one is: what a client that is told of a step needs to take the log up again after it
/// (`GET /api/v1/runs/{id}/stream?after=`).
pub fn cursor() -> Option<(String, i64)> {
    ACTIVE.with(|a| a.borrow().as_ref().map(|active| (active.run_id.clone(), active.last_seq)))
}

/// The key under which [`tag_cursor`] puts the cursor into the detail of a step on its way to a stream, and [`take_cursor`] takes it out again.
const CURSOR_KEY: &str = "_cursor";

/// Marks a copy of a step that is about to be sent to a stream with the run and the number of the last event the step caused. The step the loop keeps is not marked.
pub fn tag_cursor(step: &mut AgentStep) {
    let Some((run, seq)) = cursor().filter(|(_, seq)| *seq > 0) else { return };
    if let Some(detail) = step.detail.as_object_mut() {
        detail.insert(CURSOR_KEY.into(), serde_json::json!({ "run": run, "seq": seq }));
    }
}

/// The mark [`tag_cursor`] put on a step, taken off it: the run and the number of the event.
pub fn take_cursor(step: &mut AgentStep) -> Option<(String, i64)> {
    let mark = step.detail.as_object_mut()?.remove(CURSOR_KEY)?;
    Some((mark.get("run")?.as_str()?.to_string(), mark.get("seq")?.as_i64()?))
}

/// The question the planner asked the person, for the log of the run on this thread (`question`, the UI token's only), and kept in the run's record (card OA10-03, AG-03): what was asked,
/// what was offered, how many questions the task has asked and who the run was, so that `POST /api/v1/runs/{id}/answer` can go on from it as that caller. Called when the run has ended
/// with it, before the record is closed. A failure to write the record is logged once and changes nothing (the record is a log, not a gate).
pub fn note_question(text: &str, choices: &[String]) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            let text = clean_text(text, ANSWER_CHARS);
            let choices: Vec<String> = events::question_choices(choices).iter().map(|c| clean_text(c, 200)).collect();
            let asked = crate::modules::browser_agent::questions_asked_before().saturating_add(1);
            let caller = crate::modules::browser_agent::windows_caller_name(crate::modules::browser_agent::windows_caller_now());
            if let Err(e) = active.store.set_question(&active.run_id, &text, &choices, asked, caller, now_ms()) {
                active.warn_once(e);
            }
            active.emit(Event::Question { text, choices });
        }
    });
}

/// The number of the step row of the tool call that is open in `run_id`, when this thread is recording that run (card OA7-04): what the live output of the call is shown under. The tool
/// host asks it on the thread that makes the call, which is the thread of the run.
pub(crate) fn open_call_of(run_id: &str) -> Option<i64> {
    ACTIVE.with(|a| a.borrow().as_ref().filter(|active| active.run_id == run_id).and_then(|active| active.open.as_ref().map(|open| open.seq)))
}

/// The run the thread is recording, if any.
fn current_run_id() -> Option<String> {
    ACTIVE.with(|a| a.borrow().as_ref().map(|active| active.run_id.clone()))
}

/// A question to the person opened (the confirmation broker says so): the log of the run it is about gets `approval.opened` and the run is `waiting_approval`. The run is the one the
/// question was asked for, else the one recorded on the thread that asked.
pub fn note_question_opened(store: &AgentRunStore, request: &ConfirmRequest) {
    let Some(run) = recorded_run_of(store, request) else { return };
    let shown = serde_json::to_value(request).unwrap_or(Value::Null);
    append_quietly(store, &run, &Event::ApprovalOpened { request: shown });
    append_quietly(store, &run, &Event::Status { state: events::RunState::WaitingApproval, position: None, reason: None });
    attend(Attention::ApprovalOpened { run_id: &run, request });
}

/// The question ended (`outcome` is the broker's word: `approved`, `approved_session`, `denied`, `timeout`, `cancelled`): `approval.closed`, and the run goes on.
pub fn note_question_closed(store: &AgentRunStore, request: &ConfirmRequest, outcome: &str) {
    let Some(run) = recorded_run_of(store, request) else { return };
    append_quietly(store, &run, &Event::ApprovalClosed { request_id: request.request_id.clone(), outcome: events::ApprovalOutcome::from_word(outcome) });
    append_quietly(store, &run, &Event::Status { state: events::RunState::Running, position: None, reason: None });
    attend(Attention::ApprovalClosed { run_id: &run, request });
}

/// The run a question is about, when that run is one this record holds (a question about a run that is not recorded here is no event of any run: it would be a row that nothing
/// ever prunes).
fn recorded_run_of(store: &AgentRunStore, request: &ConfirmRequest) -> Option<String> {
    let run = request.run_id.clone().or_else(current_run_id)?;
    store.log_head(&run).ok().flatten().map(|_| run)
}

/// Writes an event; a failure is a line in the log of the core and nothing else (the log is a record, not a gate).
fn append_quietly(store: &AgentRunStore, run_id: &str, event: &Event) -> Option<i64> {
    match store.append_event(run_id, event) {
        Ok(row) => row.map(|r| r.seq),
        Err(e) => {
            warn!("agent run events: {e}");
            None
        }
    }
}

impl Active {
    /// Writes an event to the log of the run and remembers its number. A failure is logged once.
    fn emit(&mut self, event: Event) {
        match self.store.append_event(&self.run_id, &event) {
            Ok(Some(row)) => self.last_seq = row.seq,
            Ok(None) => {}
            Err(e) => {
                crate::metrics::inc("aqua_errors_total", &[("kind", "run_events")]);
                if !self.events_warned {
                    self.events_warned = true;
                    warn!("agent run events: {e} (the events of this run are no longer written)");
                }
            }
        }
    }

    /// Writes what the run holds when it has changed since it was last written, so that a crash does not wash it off.
    fn persist_marks(&mut self) {
        let Some(now) = crate::agent_runs::scope::run_marks(&self.run_id) else { return };
        if self.marks_written == Some(now) {
            return;
        }
        match self.store.set_marks(&self.run_id, now.0, now.1) {
            Ok(()) => self.marks_written = Some(now),
            Err(e) => self.warn_once(e),
        }
    }

    fn warn_once(&mut self, e: String) {
        crate::metrics::inc("aqua_errors_total", &[("kind", "run_record")]);
        if !self.warned {
            self.warned = true;
            warn!("agent run record: {e} (this run is no longer written)");
        }
    }

    fn on_step(&mut self, step: &AgentStep) {
        let d = &step.detail;
        let now = now_ms();
        // A phase (a step with a title and a status) is told to the log as it is told to the API token's stream and to the voice turn: by the one reduction, `events::PhaseView`.
        if let Some(phase) = events::PhaseView::of(step).step_event() {
            self.emit(phase);
        }
        match step.kind.as_str() {
            "phase" => {
                if d["id"] == "plan" && d["message"].as_str().is_some_and(|m| m.starts_with("Round ")) {
                    self.plan_since = Some(Instant::now());
                }
            }
            "llm" => {
                let ms = self.plan_since.take().map(|t| t.elapsed().as_millis() as i64).unwrap_or(0);
                if !self.model_seen {
                    if let Some(m) = d["model"].as_str() {
                        self.model_seen = self.store.set_model(&self.run_id, m).is_ok();
                    }
                }
                let reply = clean_text(d["raw"].as_str().unwrap_or(""), REPLY_CHARS);
                // What the model server measured for this reply, when it said.
                let t = &d["timings"];
                let (read, cached, written) = (t["prompt_tokens_read"].as_i64(), t["prompt_tokens_cached"].as_i64(), t["tokens_written"].as_i64());
                let tps10 = match (written, t["predicted_ms"].as_i64()) {
                    (Some(n), Some(ms)) if ms > 0 => Some(n * 10_000 / ms),
                    _ => None,
                };
                crate::metrics::inc("aqua_planner_replies_total", &[]);
                crate::metrics::observe_seconds("aqua_planner_seconds", &[], ms as f64 / 1000.0);
                // The same numbers by model (card OA9-05, task PR-10): which model read and wrote how much. A name and a count, never text.
                let by_model = [("model", d["model"].as_str().filter(|m| !m.is_empty()).unwrap_or("unknown")), ("role", "agent")];
                crate::metrics::inc("aqua_model_calls_total", &by_model);
                if let (Some(r), Some(c), Some(w)) = (read, cached, written) {
                    crate::metrics::add("aqua_planner_tokens_total", &[("kind", "prompt_read")], r as f64);
                    crate::metrics::add("aqua_planner_tokens_total", &[("kind", "prompt_cached")], c as f64);
                    crate::metrics::add("aqua_planner_tokens_total", &[("kind", "generated")], w as f64);
                    for (kind, n) in [("prompt_read", r), ("prompt_cached", c), ("generated", w)] {
                        crate::metrics::add("aqua_model_tokens_total", &[by_model[0], by_model[1], ("kind", kind)], n as f64);
                    }
                }
                if let Some(v) = tps10.filter(|v| *v > 0) {
                    crate::metrics::gauge("aqua_planner_decode_tokens_per_second", &[], v as f64 / 10.0);
                }
                let outcome = StepOutcome {
                    ok: true,
                    status: "completed".into(),
                    ms,
                    tokens_in: read.zip(cached).map(|(r, c)| r + c),
                    tokens_out: written,
                    cache_read: cached,
                    ttft_ms: t["prompt_ms"].as_i64(),
                    decode_tps_x10: tps10,
                    ..Default::default()
                };
                // What the server measured is told to the log when it measured anything: the window it was read against is the one the server was started with.
                let usage = (outcome.tokens_in.is_some() || outcome.tokens_out.is_some()).then(|| Event::Usage {
                    tokens_in: outcome.tokens_in,
                    tokens_out: outcome.tokens_out,
                    ttft_ms: outcome.ttft_ms,
                    decode_tps: outcome.decode_tps_x10.map(|v| v as f64 / 10.0),
                    ctx_used: outcome.tokens_in,
                    ctx_window: Some(crate::modules::llm_router::llama_server::context_tokens()),
                });
                self.write_complete(NewStep { step: step.step, kind: "plan".into(), planner_reply: reply, ts: now, ..Default::default() }, outcome);
                if let Some(usage) = usage {
                    self.emit(usage);
                }
            }
            "tool_start" => {
                // Whatever the last call left behind is not this call's.
                let _ = crate::agent_tools::take_last_call();
                let tool = d["tool"].as_str().unwrap_or("").to_string();
                let args = d.get("arguments").cloned().unwrap_or(Value::Null);
                let s = NewStep {
                    step: step.step,
                    kind: "tool".into(),
                    idempotency_digest: aqua_tool_host::short_digest(&tool, &args),
                    args_redacted: clean_args(&args, self.full),
                    tool,
                    ts: now,
                    ..Default::default()
                };
                match self.store.add_step(&self.run_id, &s) {
                    Ok(seq) => {
                        self.open = Some(Open { seq, since: Instant::now() });
                        self.emit(Event::ToolStarted { call_id: seq, tool: s.tool.clone(), args_head: s.args_redacted.clone() });
                    }
                    Err(e) => self.warn_once(e),
                }
                self.tool_rows += 1;
            }
            "tool" | "tool_error" => {
                let ok = step.kind == "tool";
                // What the run now holds is written before the result is: a result that is in the record (and so in a run that goes on from
                // this one) must never be there without the taint it brought.
                self.persist_marks();
                let obs = d["observation"].as_str().unwrap_or("");
                let outcome = self.tool_outcome(ok, obs);
                crate::metrics::inc("aqua_tool_calls_total", &[("tool", d["tool"].as_str().unwrap_or("?")), ("decision", outcome.decision.as_str())]);
                match self.open.take() {
                    Some(o) => {
                        let mut outcome = outcome;
                        outcome.ms = outcome.ms.max(o.since.elapsed().as_millis() as i64);
                        match self.store.finish_step(&self.run_id, o.seq, &outcome) {
                            Ok(()) => self.emit(tool_finished(o.seq, &outcome)),
                            Err(e) => self.warn_once(e),
                        }
                        self.persist_marks();
                    }
                    // A call the loop made without announcing it first (a fallback): written whole, after the fact.
                    None => {
                        let tool = d["tool"].as_str().unwrap_or("").to_string();
                        let args = d.get("arguments").cloned().unwrap_or(Value::Null);
                        let s = NewStep { step: step.step, kind: "tool".into(), idempotency_digest: aqua_tool_host::short_digest(&tool, &args), args_redacted: clean_args(&args, self.full), tool, ts: now, ..Default::default() };
                        self.tool_rows += 1;
                        let (name, args_head, finished) = (s.tool.clone(), s.args_redacted.clone(), outcome.clone());
                        if let Some(seq) = self.write_complete(s, outcome) {
                            self.emit(Event::ToolStarted { call_id: seq, tool: name, args_head });
                            self.emit(tool_finished(seq, &finished));
                        }
                        self.persist_marks();
                    }
                }
            }
            "auto_observe" => {
                let obs = d["observation"].as_str().unwrap_or("");
                let s = NewStep { step: step.step, kind: "observe".into(), tool: d["tool"].as_str().unwrap_or("").to_string(), ts: now, ..Default::default() };
                let o = StepOutcome { ok: true, status: "completed".into(), observation_digest: hex_digest(obs, 32), observation_head: self.head(obs), decision: "allowed".into(), ..Default::default() };
                self.write_complete(s, o);
            }
            "final" | "final_rejected" => {
                let text = d["answer"].as_str().unwrap_or("");
                let kind = if step.kind == "final" { "final" } else { "final_rejected" };
                let s = NewStep { step: step.step, kind: kind.into(), ts: now, planner_reply: String::new(), ..Default::default() };
                let note = if kind == "final" { text.to_string() } else { format!("{} | {}", d["reason"].as_str().unwrap_or(""), text) };
                let o = StepOutcome { ok: kind == "final", status: "completed".into(), observation_digest: hex_digest(&note, 32), observation_head: self.head(&note), ..Default::default() };
                self.write_complete(s, o);
            }
            _ => {}
        }
    }

    fn head(&self, text: &str) -> String {
        clean_text(text, if self.full { FULL_CAPTURE_CHARS } else { OBSERVATION_HEAD_CHARS })
    }

    fn tool_outcome(&self, ok: bool, obs: &str) -> StepOutcome {
        let call = crate::agent_tools::take_last_call();
        let denied = is_denial(obs);
        let decision = if denied {
            "denied"
        } else if ok {
            "allowed"
        } else {
            "error"
        };
        // What the person was asked and said. The host reports it; for a tool that did not go through the host, only a refusal is known.
        let approval = match call.as_ref().map(|c| c.approval.as_str()) {
            Some(a) if !a.is_empty() => a.to_string(),
            _ if obs.contains("[CONFIRMATION_DENIED]") => "denied".to_string(),
            _ => String::new(),
        };
        let ms = call.map(|c| c.duration_ms as i64).unwrap_or(0);
        crate::metrics::observe_seconds("aqua_tool_seconds", &[], ms as f64 / 1000.0);
        // (A project check the person allowed ahead of time was no question put to them, and is not counted as one.)
        if !approval.is_empty() && approval != "preapproved" {
            crate::metrics::inc("aqua_approvals_total", &[("outcome", approval.as_str())]);
        }
        StepOutcome {
            ok,
            status: if ok { "completed" } else { "failed" }.into(),
            observation_digest: hex_digest(obs, 32),
            observation_head: self.head(obs),
            decision: decision.into(),
            approval,
            ms,
            ..Default::default()
        }
    }

    /// Writes a step whole (started and completed) and gives its number, or `None` when it could not be written.
    fn write_complete(&mut self, s: NewStep, o: StepOutcome) -> Option<i64> {
        match self.store.add_step(&self.run_id, &s).and_then(|seq| self.store.finish_step(&self.run_id, seq, &o).map(|()| seq)) {
            Ok(seq) => Some(seq),
            Err(e) => {
                self.warn_once(e);
                None
            }
        }
    }
}

/// The `error` event for a run that ended with a failure that has a name: a run that returned an error (`error: ...`), or whose model could not be reached (`llm_error: ...`). A
/// stop by the person, a cap, a refusal and the other ways a run ends are `run.finished` with a state and a reason, not errors.
fn error_of(stopped_reason: &str) -> Option<Event> {
    let (code, retryable, message) = if let Some(rest) = stopped_reason.strip_prefix("llm_error") {
        ("model_unavailable", true, rest.trim_start_matches(':').trim())
    } else if let Some(rest) = stopped_reason.strip_prefix("error:") {
        ("internal", false, rest.trim())
    } else {
        return None;
    };
    Some(Event::Error { code: code.into(), message: clean_text(message, 300), retryable })
}

/// The `tool.finished` event of a call whose step was completed with `o`.
fn tool_finished(call_id: i64, o: &StepOutcome) -> Event {
    Event::ToolFinished { call_id, ok: o.ok, status: o.status.clone(), ms: o.ms, decision: o.decision.clone(), approval: o.approval.clone(), observation_head: o.observation_head.clone() }
}

/// The record of one run. Dropping it without calling [`Recording::finish`] ends the run as `failed` (`aborted`), so a panic does not
/// leave a run that looks as if it were still going.
pub struct Recording {
    store: Arc<AgentRunStore>,
    run_id: String,
    previous: Option<Active>,
    finished: bool,
    started: Instant,
}

impl Recording {
    /// Starts recording `run_id` on this thread, in the global store. `None` when runs are not being recorded.
    pub fn begin(run_id: &str, goal: &str, caller: &str, route: &str, parent: Option<&str>) -> Option<Self> {
        Self::begin_in(store()?.clone(), run_id, goal, caller, route, parent)
    }

    /// `parent` is the run this one continues (the run that ended with a question).
    pub fn begin_in(store: Arc<AgentRunStore>, run_id: &str, goal: &str, caller: &str, route: &str, parent: Option<&str>) -> Option<Self> {
        let started_ms = now_ms();
        let run = NewRun { id: run_id.to_string(), started_at: started_ms, goal_redacted: clean_text(goal, GOAL_CHARS), goal_full: clean_text(goal, store::GOAL_RESUME_CHARS), caller: caller.to_string(), route: route.to_string(), parent_run_id: parent.map(str::to_string) };
        if let Err(e) = store.create_run(&run) {
            warn!("agent run record: {e} (runs are not written)");
            return None;
        }
        // Who asked for the run decides who may read its log (`run_api`): a run nobody marked is the UI token's only.
        let origin = current_origin();
        if !origin.is_empty() {
            if let Err(e) = store.set_origin(run_id, origin) {
                warn!("agent run record: {e} (the log of this run can be read with the UI token only)");
            }
        }
        // The tools a command allowed the run are kept with it, so that a run that goes on after a question is narrowed the same way when this process no longer remembers the run
        // (`agent_tools::restrictions_from_record`): the process keeps the newest 64.
        // (Card OA11-04: the tools of the agent the run is are kept with it too, as a second list: a call has to be in every list the run is narrowed by, so a run that both a command and an agent
        // narrowed keeps both, and the run that goes on after a restart is narrowed by both. A run with one list of either kind is kept as it always was.)
        let mut kept: Vec<Vec<String>> = Vec::new();
        for list in [crate::agent_runs::scope::options_of(run_id).map(|o| o.allowed_tools), crate::agents::of_run(run_id).map(|u| u.allowed_tools)].into_iter().flatten().filter(|l| !l.is_empty()) {
            if !kept.contains(&list) {
                kept.push(list);
            }
        }
        if !kept.is_empty() {
            if let Err(e) = store.set_allowed_lists(run_id, &kept) {
                warn!("agent run record: {e} (a run that goes on from this one after a question may lose the list of tools its command or agent allowed)");
            }
        }
        // The session the run is in (card OA10-04, UX-10): the one the request named, else the one the run it goes on from is in, else its own.
        let session_id = requested_session().filter(|s| sessions::valid_session_id(s)).or_else(|| parent.map(|p| store.session_id_of(p))).unwrap_or_else(|| run_id.to_string());
        if let Err(e) = store.set_run_session(run_id, &session_id, "") {
            warn!("agent run record: {e} (the session of this run is not written)");
        }
        let mut active = Active { store: store.clone(), run_id: run_id.to_string(), open: None, plan_since: None, model_seen: false, full: capture_full(), tool_rows: 0, warned: false, marks_written: None, last_seq: 0, events_warned: false, session_id: session_id.clone() };
        active.emit(Event::RunStarted { session_id: Some(session_id), goal_head: clean_text(goal, GOAL_HEAD_CHARS), caller: caller.to_string(), route: route.to_string(), directory: None, started_ms });
        active.emit(Event::Status { state: events::RunState::Running, position: None, reason: None });
        let previous = ACTIVE.with(|a| a.borrow_mut().replace(active));
        Some(Self { store, run_id: run_id.to_string(), previous, finished: false, started: Instant::now() })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Ends the run. `steps` are the loop's own steps: a run that was replayed from a recipe made its calls without announcing them
    /// first, and they are written now.
    pub fn finish(self, stopped_reason: &str, rounds: u32, answer: &str, steps: &[AgentStep]) {
        // The cap of the autonomy setting that stopped the run, if one did (card R7-18). The counters are the run's until its scope ends, which is after this.
        let capped = crate::autonomy::stopped_by_run(&self.run_id).map(|cap| cap.id());
        self.finish_capped(stopped_reason, rounds, answer, steps, capped);
    }

    /// [`finish`](Self::finish) with the cap that stopped the run given (`max_seconds`, `max_tool_calls`, `max_files_changed`, `max_changed_bytes`). The record names it, and a run
    /// that a cap stopped is `failed` even if the model then wrote an answer: it did not finish its task, so it is not counted as a success.
    pub fn finish_capped(mut self, stopped_reason: &str, rounds: u32, answer: &str, steps: &[AgentStep], capped: Option<&str>) {
        self.finished = true;
        let mut active = ACTIVE.with(|a| a.borrow_mut().take());
        if let Some(a) = active.as_mut() {
            // A call that was announced and then refused ends the run without a result step: say what happened to it.
            if let Some(o) = a.open.take() {
                let refused = matches!(stopped_reason, "user_declined" | "policy_denied" | "voice_cannot_act");
                if refused {
                    let approval = if stopped_reason == "user_declined" { "denied" } else { "" };
                    let out = StepOutcome { ok: false, status: "failed".into(), decision: "denied".into(), approval: approval.into(), ms: o.since.elapsed().as_millis() as i64, ..Default::default() };
                    crate::metrics::inc("aqua_tool_calls_total", &[("tool", "refused"), ("decision", "denied")]);
                    if !approval.is_empty() {
                        crate::metrics::inc("aqua_approvals_total", &[("outcome", approval)]);
                    }
                    if a.store.finish_step(&a.run_id, o.seq, &out).is_ok() {
                        a.emit(tool_finished(o.seq, &out));
                    }
                } else if stopped_reason == "needs_login" {
                    // The page was read: that call worked. What ended the run was what the page said (it asks for a sign-in), so the step is not left as interrupted.
                    let out = StepOutcome { ok: true, status: "completed".into(), ms: o.since.elapsed().as_millis() as i64, ..Default::default() };
                    if a.store.finish_step(&a.run_id, o.seq, &out).is_ok() {
                        a.emit(tool_finished(o.seq, &out));
                    }
                }
            }
            if a.tool_rows == 0 {
                for s in steps.iter().filter(|s| matches!(s.kind.as_str(), "tool" | "tool_error")) {
                    a.on_step(&AgentStep { step: s.step, kind: "tool_start".into(), detail: s.detail.clone() });
                    a.on_step(s);
                }
            }
        }
        let marks = crate::agent_runs::scope::run_marks(&self.run_id).map(|(t, p)| serde_json::json!({"tainted": t, "private": p}).to_string());
        let mut state = status_for(stopped_reason);
        if capped.is_some() && state == status::COMPLETED {
            state = status::FAILED;
        }
        // What the run says last: why it failed, which cap stopped it, its answer sentence by sentence. `run.finished` comes after the record is closed (below), so that nothing follows it.
        let usage = crate::autonomy::usage_of_run(&self.run_id);
        let mut put = |event: Event| match active.as_mut() {
            Some(a) => a.emit(event),
            None => {
                append_quietly(&self.store, &self.run_id, &event);
            }
        };
        if let Some(error) = error_of(stopped_reason) {
            put(error);
        }
        if let (Some(cap), Some((view, caps))) = (capped, usage.as_ref()) {
            if let Some(cap) = crate::autonomy::Cap::from_id(cap) {
                let used = match cap {
                    crate::autonomy::Cap::Seconds => view.seconds,
                    crate::autonomy::Cap::ToolCalls => view.tool_calls,
                    crate::autonomy::Cap::FilesChanged => view.files_changed,
                    crate::autonomy::Cap::ChangedBytes => view.bytes_changed,
                };
                put(Event::Limit { stopped_by: cap.id().into(), used, cap: caps.get(cap) });
            }
        }
        if state == status::WAITING_USER {
            put(Event::Status { state: events::RunState::WaitingUser, position: None, reason: None });
        } else {
            for (index, piece) in events::text_pieces(&clean_text(answer, TEXT_ANSWER_CHARS)).into_iter().enumerate() {
                put(Event::Text { index: index as u32, text: piece });
            }
        }
        let end = RunEnd {
            status: state.into(),
            stopped_reason: cut(stopped_reason, 200),
            rounds,
            final_answer_head: clean_text(answer, ANSWER_CHARS),
            taint_json: marks,
            stopped_by: capped.unwrap_or("").to_string(),
            ..Default::default()
        };
        crate::metrics::inc("aqua_agent_runs_total", &[("status", end.status.as_str())]);
        if let Err(e) = self.store.end_run(&self.run_id, now_ms(), &end) {
            warn!("agent run record: {e}");
        } else {
            // A run that did not go well gets a sentence on why it stopped (card R7-06). It describes the end; it changes nothing about it.
            crate::triage::on_run_end(&self.store, &self.run_id, &end.status);
            if export::enabled() {
                // The finished run as spans in a file, for a trace viewer (off unless AQUA_TRACE_EXPORT=1).
                if let Ok(Some(run)) = self.store.get(&self.run_id) {
                    if let Err(e) = export::append_run(&crate::runtime::data_dir().join("traces"), &run, now_ms()) {
                        warn!("agent run record: the trace could not be exported: {e}");
                    }
                }
            }
        }
        // The last event of the log: nothing follows it. It is written whether or not the record could be closed, so that a stream waiting for the end of the run is told.
        let why = self.store.triage_json(&self.run_id).ok().flatten().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|t| t["why"].as_str().map(str::to_string));
        append_quietly(
            &self.store,
            &self.run_id,
            &Event::RunFinished {
                status: end.status.clone(),
                stopped_reason: end.stopped_reason.clone(),
                // What the planner asked is the UI token's (the `question` event); the head of the answer of a run that waits for one is that question, so it is not repeated here.
                answer_head: if end.status == status::WAITING_USER { String::new() } else { end.final_answer_head.clone() },
                why_it_stopped: why.map(|w| cut(&w, ANSWER_CHARS)),
                files_changed: usage.as_ref().map_or(0, |(view, _)| view.files_changed),
                ms: self.started.elapsed().as_millis() as i64,
            },
        );
        ACTIVE.with(|a| *a.borrow_mut() = self.previous.take());
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let end = RunEnd { status: status::FAILED.into(), stopped_reason: "aborted".into(), ..Default::default() };
        let _ = self.store.end_run(&self.run_id, now_ms(), &end);
        append_quietly(&self.store, &self.run_id, &Event::RunFinished { status: end.status.clone(), stopped_reason: end.stopped_reason.clone(), answer_head: String::new(), why_it_stopped: None, files_changed: 0, ms: self.started.elapsed().as_millis() as i64 });
        ACTIVE.with(|a| *a.borrow_mut() = self.previous.take());
    }
}

/// The calls a run made and what they returned (as the record kept them: the first 500 characters), oldest first and at most `max`, for a run
/// that goes on after a question. Empty when the run is unknown or runs are not recorded.
pub fn resume_history(run_id: &str, max: usize) -> Vec<(String, String)> {
    let Some(store) = store() else { return Vec::new() };
    resume::history_of(store, run_id, max)
}

/// What kind of task the run on this thread is (`coding`, `files`, `web` or `desktop`, card OA4-01), for its record: a run that goes on from it is the same kind. The loop calls it when
/// the run starts and again with `coding` when a run that began as something else changes code.
pub fn note_profile(profile: &str) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_profile(&active.run_id, profile) {
                active.warn_once(e);
            }
        }
    });
}

/// Keeps the coding record of the run on this thread (`modules::coding_driver::CodingRecord` as JSON, card OA4-04, WF-06) in its record, replacing the one written before: what the goal is,
/// what was read and changed, what the newest check of each kind said and what is left to do, as the model was shown it. The loop calls it each round of a coding run.
pub fn note_coding_record(record_json: &str) {
    // The record is made of what the checks printed and of the names of files the model wrote, and it is kept with the run for whoever reads the run: every string in it is masked here, by
    // the masking the record of a run applies to everything else it keeps (`clean_text`, `clean_args`), whatever made it. Text that is not JSON is not kept.
    let Ok(parsed) = serde_json::from_str::<Value>(record_json) else { return };
    let record_json = aqua_tool_host::output::Redactor::new().redact_json(&parsed).to_string();
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if let Err(e) = active.store.set_coding_record(&active.run_id, &record_json, now_ms()) {
                active.warn_once(e);
            }
        }
    });
}

/// Writes what the run on this thread holds (read untrusted text, holds private data) into its record, if that changed. The loop calls it when a
/// run starts from another run's marks and when a browser or desktop call marks the run; a tool call through the host is followed by it on its own.
pub fn note_marks() {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            active.persist_marks();
        }
    });
}

/// The host is about to dispatch a call that changes something: the open step of the run on this thread is this call, by the host's own name for
/// the tool and digest of the arguments. (The loop announced the step first, with the name the model used.)
pub fn note_dispatch(run_id: &str, tool: &str, short_digest: &str) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            if active.run_id == run_id {
                if let Some(open) = &active.open {
                    if let Err(e) = active.store.set_step_call(run_id, open.seq, tool, short_digest) {
                        active.warn_once(e);
                    }
                }
            }
        }
    });
}

/// A run that has written nothing for this long is stalled (see `AgentRunStore::mark_stalled`).
pub const STALL_AFTER_MS: i64 = 5 * 60 * 1000;

/// One look of the watchdog: marks the runs that are going but silent, and counts them. Called every minute from the core's main.
pub fn watchdog_tick() {
    let Some(store) = store() else { return };
    watchdog_tick_in(store, now_ms(), &crate::agent_runs::scope::is_active);
}

/// [`watchdog_tick`] on a given record, at a given time, with `alive` saying which runs this process is working on (the core gives its own; a test gives a fake clock).
/// Returns the ids it marked stalled.
pub fn watchdog_tick_in(store: &AgentRunStore, now: i64, alive: &dyn Fn(&str) -> bool) -> Vec<String> {
    let stalled = match store.mark_stalled(now, STALL_AFTER_MS, alive) {
        Ok(ids) => {
            for id in &ids {
                warn!("agent run {id} has written nothing for five minutes: marked stalled");
                crate::metrics::inc("aqua_agent_runs_stalled_total", &[]);
            }
            ids
        }
        Err(e) => {
            warn!("{e}");
            Vec::new()
        }
    };
    // The question the agent asked is kept in memory for `conversation::IDLE`; after that nobody can answer it.
    if let Err(e) = store.expire_waiting(now, crate::conversation::IDLE.as_millis() as i64) {
        warn!("{e}");
    }
    stalled
}

/// The plan the agent wrote for the run that is going on in this thread (`AQUA_PLAN`), kept in its record.
pub fn note_plan(plan: &[String]) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow().as_ref() {
            let _ = active.store.set_plan(&active.run_id, &serde_json::to_string(plan).unwrap_or_default());
        }
    });
}

/// Tells the recording of this thread (if any) about a step of the agent loop.
pub fn observe_step(step: &AgentStep) {
    ACTIVE.with(|a| {
        if let Some(active) = a.borrow_mut().as_mut() {
            active.on_step(step);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(name: &str) -> Arc<AgentRunStore> {
        let dir = std::env::temp_dir().join(format!("aqua_agent_runs_rec_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(AgentRunStore::open(dir.join("agent_runs.db")).unwrap())
    }

    fn step(n: u32, kind: &str, detail: Value) -> AgentStep {
        AgentStep { step: n, kind: kind.into(), detail }
    }

    #[test]
    fn the_steps_the_loop_announces_become_rows_with_the_call_written_before_it_ran() {
        let s = store("rows");
        let rec = Recording::begin_in(s.clone(), "run_a", "read notes.txt with password=hunter2secret99", "api", "agent", None).unwrap();
        observe_step(&step(1, "phase", json!({"id": "plan", "status": "running", "message": "Round 1/12: deciding"})));
        observe_step(&step(1, "llm", json!({"raw": "{\"tool\":\"fs.read\"}", "model": "qwen3-8b"})));
        observe_step(&step(1, "tool_start", json!({"tool": "fs.read", "arguments": {"path": "notes.txt", "content": "top secret body"}})));
        // What a crash at this moment would leave behind:
        let mid = s.get("run_a").unwrap().unwrap();
        let tool = mid["steps"].as_array().unwrap().iter().find(|x| x["kind"] == "tool").unwrap().clone();
        assert_eq!((tool["status"].as_str(), tool["ok"].clone()), (Some("started"), Value::Null), "written before the tool ran");
        observe_step(&step(1, "tool", json!({"tool": "fs.read", "observation": "1  Project Aurora"})));
        observe_step(&step(2, "final", json!({"answer": "It says Project Aurora."})));
        rec.finish("final", 1, "It says Project Aurora.", &[]);

        let run = s.get("run_a").unwrap().unwrap();
        assert_eq!((run["status"].as_str(), run["stopped_reason"].as_str(), run["model"].as_str(), run["rounds"].as_i64()), (Some("completed"), Some("final"), Some("qwen3-8b"), Some(1)));
        assert!(!run["goal"].as_str().unwrap().contains("hunter2secret99"), "{}", run["goal"]);
        let kinds: Vec<&str> = run["steps"].as_array().unwrap().iter().map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["plan", "tool", "final"]);
        let t = &run["steps"][1];
        assert_eq!((t["status"].as_str(), t["ok"].clone(), t["decision"].as_str(), t["observation"].as_str()), (Some("completed"), json!(true), Some("allowed"), Some("1  Project Aurora")));
        assert!(!t["arguments"].to_string().contains("top secret body"), "bodies are not stored: {}", t["arguments"]);
        assert_eq!(t["arguments"]["path"], "notes.txt");
        assert_eq!(t["idempotency_digest"].as_str().unwrap().len(), 16);
    }

    #[test]
    fn a_planner_reply_is_on_the_disk_the_moment_it_comes_and_before_the_call_it_names_is_started() {
        let s = store("reply_now");
        let _rec = Recording::begin_in(s.clone(), "run_r", "read notes.txt", "api", "agent", None).unwrap();
        observe_step(&step(1, "phase", json!({"id": "plan", "status": "running", "message": "Round 1/12: deciding"})));
        observe_step(&step(1, "llm", json!({"raw": "{\"tool\":\"fs.read\",\"arguments\":{\"path\":\"notes.txt\"}}", "model": "qwen3-8b"})));
        // A process that died here has nothing but the disk: a connection of its own, as a restarted process opens, finds the reply.
        let reopened = AgentRunStore::open(s.path()).unwrap();
        let run = reopened.get("run_r").unwrap().unwrap();
        let steps = run["steps"].as_array().unwrap();
        let plan = steps.iter().find(|x| x["kind"] == "plan").expect("the reply of the model is in the record already");
        assert!(plan["planner_reply"].as_str().unwrap().contains("notes.txt") && plan["status"] == "completed", "{plan}");
        assert!(steps.iter().all(|x| x["kind"] != "tool"), "the call it names has not started");
    }

    #[test]
    fn a_failure_and_a_refusal_are_told_apart_and_a_declined_question_is_recorded_as_one() {
        let s = store("refusal");
        let rec = Recording::begin_in(s.clone(), "run_b", "delete the folder", "agent", "agent", None).unwrap();
        observe_step(&step(1, "tool_start", json!({"tool": "fs.read", "arguments": {"path": "x"}})));
        observe_step(&step(1, "tool_error", json!({"tool": "fs.read", "observation": "ERROR [NOT_FOUND] fs.read: no such file"})));
        observe_step(&step(2, "tool_start", json!({"tool": "fs.delete", "arguments": {"path": "x"}})));
        // The question was declined: the loop returns without a result step.
        rec.finish("user_declined", 2, "I stopped without doing it because you declined: Delete x.", &[]);
        let run = s.get("run_b").unwrap().unwrap();
        assert_eq!(run["status"], "cancelled");
        let tools: Vec<&Value> = run["steps"].as_array().unwrap().iter().filter(|x| x["kind"] == "tool").collect();
        assert_eq!((tools[0]["decision"].as_str(), tools[0]["ok"].clone()), (Some("error"), json!(false)));
        assert_eq!((tools[1]["decision"].as_str(), tools[1]["approval"].as_str(), tools[1]["status"].as_str()), (Some("denied"), Some("denied"), Some("failed")), "{}", tools[1]);
    }

    #[test]
    fn a_run_that_stopped_at_a_sign_in_page_keeps_the_page_it_read_as_a_completed_step() {
        let s = store("sign_in");
        let rec = Recording::begin_in(s.clone(), "run_s", "post on facebook", "agent", "agent", None).unwrap();
        observe_step(&step(1, "tool_start", json!({"tool": "browser_navigate", "arguments": {"url": "https://www.facebook.com"}})));
        rec.finish("needs_login", 1, "facebook.com is asking you to sign in.", &[]);
        let run = s.get("run_s").unwrap().unwrap();
        assert_eq!((run["status"].as_str(), run["stopped_reason"].as_str()), (Some("cancelled"), Some("needs_login")));
        let tool = run["steps"].as_array().unwrap().iter().find(|x| x["kind"] == "tool").unwrap();
        assert_eq!((tool["status"].as_str(), tool["ok"].clone()), (Some("completed"), json!(true)), "{tool}");
    }

    #[test]
    fn a_recipe_replay_that_never_announced_its_calls_is_written_from_the_result() {
        let s = store("replay");
        let rec = Recording::begin_in(s.clone(), "run_c", "open the weather page", "api", "agent", None).unwrap();
        let replayed = vec![step(1, "tool", json!({"tool": "browser_navigate", "arguments": {"url": "https://example.com"}, "observation": "Navigated."}))];
        rec.finish("learned_fast_path", 1, "Done.", &replayed);
        let run = s.get("run_c").unwrap().unwrap();
        assert_eq!((run["status"].as_str(), run["tool_calls"].as_i64()), (Some("completed"), Some(1)));
        assert_eq!(run["steps"][0]["tool"], "browser_navigate");
    }

    #[test]
    fn a_run_that_is_dropped_without_finishing_does_not_look_as_if_it_were_still_going() {
        let s = store("drop");
        {
            let _rec = Recording::begin_in(s.clone(), "run_d", "g", "api", "agent", None).unwrap();
            observe_step(&step(1, "tool_start", json!({"tool": "term.run", "arguments": {}})));
        } // a panic would unwind to here
        let run = s.get("run_d").unwrap().unwrap();
        assert_eq!((run["status"].as_str(), run["stopped_reason"].as_str(), run["steps"][0]["status"].as_str()), (Some("failed"), Some("aborted"), Some("interrupted")));
        // And nothing is heard after it ends:
        observe_step(&step(9, "tool_start", json!({"tool": "x", "arguments": {}})));
        assert_eq!(s.get("run_d").unwrap().unwrap()["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn secrets_are_masked_before_anything_is_written_and_the_observation_is_cut() {
        let s = store("mask");
        let rec = Recording::begin_in(s.clone(), "run_e", "g", "api", "agent", None).unwrap();
        observe_step(&step(1, "tool_start", json!({"tool": "web.fetch", "arguments": {"url": "https://x.test/a", "headers": {"Authorization": "Bearer abcdefghijklmnop123456"}, "api_key": "sk-live-abcdef123456"}})));
        let long = format!("token=ghp_abcdefghijklmnopqrstuvwxyz0123456789 {}", "x".repeat(2_000));
        observe_step(&step(1, "tool", json!({"tool": "web.fetch", "observation": long})));
        rec.finish("final", 1, "ok", &[]);
        let t = s.get("run_e").unwrap().unwrap()["steps"][0].clone();
        let all = t.to_string();
        assert!(!all.contains("abcdefghijklmnop123456") && !all.contains("sk-live-abcdef123456") && !all.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "{all}");
        assert!(t["observation"].as_str().unwrap().chars().count() <= OBSERVATION_HEAD_CHARS + 1, "cut to the head");
        assert_eq!(t["observation_digest"].as_str().unwrap().len(), 32, "the digest still stands for all of it");
    }

    #[test]
    fn what_the_model_server_measured_is_kept_with_the_planner_reply_and_counted() {
        let s = store("timings");
        let before_replies = crate::metrics::counter_total("aqua_planner_replies_total");
        let before_read = crate::metrics::counter_value("aqua_planner_tokens_total", &[("kind", "prompt_read")]);
        let rec = Recording::begin_in(s.clone(), "run_t", "g", "api", "agent", None).unwrap();
        observe_step(&step(1, "phase", json!({"id": "plan", "status": "running", "message": "Round 1/12: deciding"})));
        observe_step(&step(1, "llm", json!({"raw": "{}", "model": "qwen3-8b", "timings": {"prompt_tokens_read": 12, "prompt_tokens_cached": 800, "tokens_written": 40, "prompt_ms": 310, "predicted_ms": 10000}})));
        rec.finish("final", 1, "ok", &[]);
        let plan = s.get("run_t").unwrap().unwrap()["steps"][0].clone();
        assert_eq!((plan["tokens_in"].as_i64(), plan["tokens_out"].as_i64(), plan["cache_read"].as_i64()), (Some(812), Some(40), Some(800)));
        assert_eq!((plan["ttft_ms"].as_i64(), plan["decode_tokens_per_second"].as_f64()), (Some(310), Some(4.0)));
        assert!(crate::metrics::counter_total("aqua_planner_replies_total") > before_replies);
        assert!(crate::metrics::counter_value("aqua_planner_tokens_total", &[("kind", "prompt_read")]) >= before_read + 12.0);
    }

    /// Card OA9-05, task PR-10: the same numbers are counted by model, and the run's usage view reads the rows the loop wrote and says what the metric says.
    #[test]
    fn the_planner_tokens_are_counted_by_model_and_the_usage_of_the_run_says_the_same() {
        let s = store("usage_by_model");
        let series = |name: &str, kind: Option<&str>| {
            let mut labels = vec![("model", "usage-test-model"), ("role", "agent")];
            labels.extend(kind.map(|k| ("kind", k)));
            crate::metrics::counter_value(name, &labels)
        };
        let before = (series("aqua_model_calls_total", None), series("aqua_model_tokens_total", Some("prompt_read")), series("aqua_model_tokens_total", Some("prompt_cached")), series("aqua_model_tokens_total", Some("generated")));
        let rec = Recording::begin_in(s.clone(), "run_u", "g", "api", "agent", None).unwrap();
        for (round, (read, cached, written)) in [(1400, 0, 40), (30, 1400, 25)].into_iter().enumerate() {
            observe_step(&step(round as u32 + 1, "phase", json!({"id": "plan", "status": "running", "message": format!("Round {}/12: deciding", round + 1)})));
            observe_step(&step(round as u32 + 1, "llm", json!({"raw": "{}", "model": "usage-test-model", "timings": {"prompt_tokens_read": read, "prompt_tokens_cached": cached, "tokens_written": written, "prompt_ms": 300, "predicted_ms": 5000}})));
        }
        // A reply the server gave no numbers for is a call, and moves no token count.
        observe_step(&step(3, "llm", json!({"raw": "{}", "model": "usage-test-model"})));
        rec.finish("final", 3, "ok", &[]);
        assert_eq!(series("aqua_model_calls_total", None) - before.0, 3.0);
        assert_eq!(series("aqua_model_tokens_total", Some("prompt_read")) - before.1, 1430.0);
        assert_eq!(series("aqua_model_tokens_total", Some("prompt_cached")) - before.2, 1400.0);
        assert_eq!(series("aqua_model_tokens_total", Some("generated")) - before.3, 65.0);
        let usage = crate::model::ledger::RunUsage::from_run(&s.get("run_u").unwrap().unwrap());
        assert_eq!((usage.totals.calls, usage.totals.tokens_in, usage.totals.cache_read, usage.totals.tokens_out), (2, 2830, 1400, 65), "what the metric counted is what the run's record sums to");
        assert_eq!(usage.unmeasured_replies, 1);
        assert_eq!(usage.by_model.keys().cloned().collect::<Vec<_>>(), vec!["usage-test-model".to_string()]);
    }

    #[test]
    fn the_watchdog_marks_a_silent_run_stalled_or_interrupted_by_whether_this_process_works_on_it_and_expires_a_question_nobody_answered() {
        let s = store("watchdog_tick");
        let run = |id: &str, started: i64| s.create_run(&store::NewRun { id: id.into(), started_at: started, goal_full: "g".into(), ..Default::default() }).unwrap();
        run("silent_and_going", 1_000);
        run("silent_and_gone", 1_000);
        run("recent", 1_000);
        s.add_step("recent", &store::NewStep { step: 1, kind: "plan".into(), ts: 590_000, ..Default::default() }).unwrap();
        run("asked", 1_000);
        s.end_run("asked", 2_000, &store::RunEnd { status: store::status::WAITING_USER.into(), stopped_reason: "waiting_user".into(), ..Default::default() }).unwrap();
        let alive = |id: &str| id != "silent_and_gone";
        let state = |id: &str| s.get(id).unwrap().unwrap()["status"].as_str().unwrap().to_string();
        // Four minutes in: nothing has been silent for five.
        assert!(watchdog_tick_in(&s, 241_000, &alive).is_empty());
        assert_eq!((state("silent_and_going"), state("silent_and_gone"), state("asked")), ("running".to_string(), "running".to_string(), "waiting_user".to_string()));
        // Ten minutes and a half in: the run this process works on is stalled, the one it does not is interrupted, a recent one is left alone, and the question is past its idle time.
        let stalled = watchdog_tick_in(&s, 631_000, &alive);
        assert_eq!(stalled, vec!["silent_and_going".to_string()]);
        assert_eq!((state("silent_and_going"), state("silent_and_gone"), state("recent"), state("asked")), ("stalled".to_string(), "interrupted".to_string(), "running".to_string(), "expired".to_string()));
        // The tick of the core is this one with the real clock and the real set of runs: nothing is marked in a record that is not there.
        watchdog_tick();
    }

    #[test]
    fn what_ended_a_run_decides_its_state() {
        assert_eq!(status_for("final"), "completed");
        assert_eq!(status_for("scripted_open"), "completed");
        assert_eq!(status_for("cancelled"), "cancelled");
        assert_eq!(status_for("client_disconnected"), "cancelled");
        assert_eq!(status_for("waiting_user"), "waiting_user");
        assert_eq!(status_for("needs_login"), "cancelled");
        for r in ["deadline", "max_steps", "llm_error: boom", "policy_denied", "voice_cannot_act"] {
            assert_eq!(status_for(r), "failed", "{r}");
        }
    }

    /// Card R7-18: a run that a cap of the autonomy setting stopped is written with the cap, and it is `failed` even when the model then gave an answer; the run no cap stopped is as before.
    #[test]
    fn a_run_that_a_cap_stopped_names_the_cap_and_is_not_counted_as_finished() {
        let s = store("capped");
        let rec = Recording::begin_in(s.clone(), "run_cap", "write ten files", "api", "agent", None).unwrap();
        rec.finish_capped("final", 3, "I wrote four files; the limit was reached.", &[], Some("max_files_changed"));
        let run = s.get("run_cap").unwrap().unwrap();
        assert_eq!((run["status"].as_str(), run["stopped_reason"].as_str(), run["stopped_by"].as_str()), (Some("failed"), Some("final"), Some("max_files_changed")), "the reason the loop gave is kept");
        let rec = Recording::begin_in(s.clone(), "run_free", "read a file", "api", "agent", None).unwrap();
        rec.finish_capped("final", 1, "Done.", &[], None);
        let free = s.get("run_free").unwrap().unwrap();
        assert_eq!((free["status"].as_str(), free["stopped_by"].as_str()), (Some("completed"), Some("")), "no cap, as before");
        // The cap changes only a run that would have counted as a success: one the person stopped stays stopped.
        let rec = Recording::begin_in(s.clone(), "run_stop", "g", "api", "agent", None).unwrap();
        rec.finish_capped("cancelled", 1, "Stopped.", &[], Some("max_seconds"));
        let stopped = s.get("run_stop").unwrap().unwrap();
        assert_eq!((stopped["status"].as_str(), stopped["stopped_by"].as_str()), (Some("cancelled"), Some("max_seconds")));
        // `finish` asks the process's counters, which know nothing of a run that never made a call.
        let rec = Recording::begin_in(s.clone(), "run_plain", "g", "api", "agent", None).unwrap();
        rec.finish("final", 1, "Done.", &[]);
        assert_eq!(s.get("run_plain").unwrap().unwrap()["stopped_by"], "");
    }

    /// Card OA3-01 (AQ-04): this was a search of this file for the call to `autonomy::stopped_by_run`. A run that made more calls than its cap allowed is ended with `finish`, and its
    /// record names the cap the counters say stopped it, without anyone passing the cap in.
    #[test]
    fn finish_writes_the_cap_that_the_limits_counters_say_stopped_the_run() {
        use crate::autonomy::{Autonomy, Caps, Settings, DEFAULT_CAPS};
        let s = store("finish_asks_the_counters");
        let counters = Arc::new(Autonomy::new(Settings { caps: Caps { max_tool_calls: 1, ..DEFAULT_CAPS }, ..Settings::default() }));
        crate::autonomy::with_autonomy(counters.clone(), || {
            let args = serde_json::json!({});
            let rec = Recording::begin_in(s.clone(), "run_counted", "read two files", "api", "agent", None).unwrap();
            assert!(counters.check_before_call("run_counted", "fs.read", &args).is_ok(), "the one call the cap allows");
            assert!(counters.check_before_call("run_counted", "fs.read", &args).is_err(), "the second is over it");
            rec.finish("final", 2, "I read one of the two files.", &[]);
            // A run the counters never refused is written as it always was.
            let rec = Recording::begin_in(s.clone(), "run_within", "read a file", "api", "agent", None).unwrap();
            assert!(counters.check_before_call("run_within", "fs.read", &args).is_ok());
            rec.finish("final", 1, "Done.", &[]);
        });
        let capped = s.get("run_counted").unwrap().unwrap();
        assert_eq!((capped["status"].as_str(), capped["stopped_by"].as_str()), (Some("failed"), Some("max_tool_calls")), "named, and not counted as a success");
        let within = s.get("run_within").unwrap().unwrap();
        assert_eq!((within["status"].as_str(), within["stopped_by"].as_str()), (Some("completed"), Some("")));
    }

    // ---- the events of a run (card OA10-01) ----------------------------------------------------------------------------------

    fn kinds(s: &AgentRunStore, run: &str) -> Vec<String> {
        s.events_after(run, 0, 1_000).unwrap().into_iter().map(|e| e.kind).collect()
    }

    /// A scripted run, as the loop would say it: the events come in the order things happened, numbered 1, 2, 3 ..., with the number of the step row as `call_id`, and what a call was given
    /// and returned is apart for the UI token.
    #[test]
    fn a_run_writes_its_events_in_the_order_things_happen_and_keeps_what_a_call_touched_for_the_ui() {
        let s = store("events_order");
        let rec = Recording::begin_in(s.clone(), "run_ev", "read notes.txt with password=hunter2secret99", "api", "agent", None).unwrap();
        observe_step(&step(1, "phase", json!({"id": "plan", "phase": "plan", "title": "Plan with llama.cpp", "status": "running", "message": "Round 1/12: deciding"})));
        observe_step(&step(1, "llm", json!({"raw": "{\"tool\":\"fs.read\"}", "model": "qwen3-8b", "title": "llama.cpp (qwen3-8b)", "status": "completed", "timings": {"prompt_tokens_read": 12, "prompt_tokens_cached": 800, "tokens_written": 40, "prompt_ms": 310, "predicted_ms": 10000}})));
        observe_step(&step(1, "tool_start", json!({"tool": "fs.read", "arguments": {"path": "notes.txt", "content": "top secret body"}})));
        observe_step(&step(1, "tool", json!({"tool": "fs.read", "observation": "1  Project Aurora"})));
        observe_step(&step(2, "final", json!({"answer": "It says Project Aurora. Call Pune."})));
        rec.finish("final", 1, "It says Project Aurora. Call Pune.", &[]);

        let rows = s.events_after("run_ev", 0, 100).unwrap();
        let names: Vec<&str> = rows.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(names, ["run.started", "status", "step", "step", "usage", "tool.started", "tool.finished", "text", "text", "run.finished"]);
        assert_eq!(rows.iter().map(|e| e.seq).collect::<Vec<_>>(), (1..=10).collect::<Vec<i64>>());
        // The first event: who, how, and the goal with its secret masked.
        assert_eq!((rows[0].data["caller"].as_str(), rows[0].data["route"].as_str()), (Some("api"), Some("agent")));
        assert!(!rows[0].data["goal_head"].as_str().unwrap().contains("hunter2secret99"), "{}", rows[0].data["goal_head"]);
        assert_eq!((rows[1].data["state"].as_str(), rows[2].data["title"].as_str(), rows[2].data["status"].as_str()), (Some("running"), Some("Plan with llama.cpp"), Some("running")));
        // What the model server measured, against the window the server was started with.
        assert_eq!((rows[4].data["tokens_in"].as_i64(), rows[4].data["tokens_out"].as_i64(), rows[4].data["ttft_ms"].as_i64(), rows[4].data["decode_tps"].as_f64(), rows[4].data["ctx_used"].as_i64()), (Some(812), Some(40), Some(310), Some(4.0), Some(812)));
        assert!(rows[4].data["ctx_window"].as_u64().unwrap() >= 2048);
        // The call: `call_id` is the number of its step row; the arguments (bodies masked) and the result are the UI's.
        let steps = s.get("run_ev").unwrap().unwrap()["steps"].clone();
        let tool_row = steps.as_array().unwrap().iter().find(|x| x["kind"] == "tool").unwrap();
        assert_eq!(rows[5].data["call_id"], tool_row["seq"]);
        assert_eq!(rows[6].data["call_id"], tool_row["seq"]);
        assert_eq!((rows[6].data["ok"].as_bool(), rows[6].data["decision"].as_str(), rows[6].data["status"].as_str()), (Some(true), Some("allowed"), Some("completed")));
        assert!(rows[5].data.get("args_head").is_none() && rows[6].data.get("observation_head").is_none(), "not in the part everybody reads");
        assert!(rows[5].ui["args_head"].as_str().unwrap().contains("notes.txt") && !rows[5].ui["args_head"].as_str().unwrap().contains("top secret body"), "{}", rows[5].ui["args_head"]);
        assert_eq!(rows[6].ui["observation_head"], "1  Project Aurora");
        // The answer, sentence by sentence, and the end with what ended it.
        assert_eq!((rows[7].data["text"].as_str(), rows[8].data["text"].as_str(), rows[7].data["index"].as_i64(), rows[8].data["index"].as_i64()), (Some("It says Project Aurora."), Some("Call Pune."), Some(0), Some(1)));
        assert_eq!((rows[9].data["status"].as_str(), rows[9].data["stopped_reason"].as_str(), rows[9].data["answer_head"].as_str()), (Some("completed"), Some("final"), Some("It says Project Aurora. Call Pune.")));
        assert!(rows[9].data["ms"].as_i64().unwrap() >= 0);
        assert!(s.log_head("run_ev").unwrap().unwrap().finished);
    }

    /// The cursor a step carries to a stream is the number of the last event the step caused; the step the loop keeps is not marked.
    #[test]
    fn a_step_sent_to_a_stream_carries_the_number_of_its_place_in_the_log() {
        let s = store("events_cursor");
        let mut lone = step(1, "phase", json!({"title": "t", "status": "running"}));
        tag_cursor(&mut lone);
        assert_eq!(take_cursor(&mut lone), None, "no run is being recorded on this thread: nothing to say");
        let rec = Recording::begin_in(s.clone(), "run_cur", "g", "api", "agent", None).unwrap();
        let first = step(1, "phase", json!({"title": "Plan", "status": "running"}));
        observe_step(&first);
        let (run, seq) = cursor().expect("a run is being recorded");
        assert_eq!((run.as_str(), seq), ("run_cur", 3), "run.started, status, step");
        let mut sent = first.clone();
        tag_cursor(&mut sent);
        assert!(sent.detail.get("_cursor").is_some() && first.detail.get("_cursor").is_none(), "only the copy is marked");
        assert_eq!(take_cursor(&mut sent), Some(("run_cur".to_string(), 3)));
        assert_eq!(sent.detail, first.detail, "and taking the mark off leaves the step as it was");
        observe_step(&step(1, "tool_start", json!({"tool": "fs.read", "arguments": {}})));
        assert_eq!(cursor().unwrap().1, 4);
        rec.finish("final", 1, "Done.", &[]);
        assert_eq!(cursor(), None, "after the run");
    }

    /// Card OA10-01: a cap that stopped the run is an event with how much was used, and the run is `failed`.
    #[test]
    fn a_run_that_a_cap_stopped_says_so_in_a_limit_event_with_what_it_used() {
        use crate::autonomy::{Autonomy, Caps, Settings, DEFAULT_CAPS};
        let s = store("events_limit");
        let counters = Arc::new(Autonomy::new(Settings { caps: Caps { max_tool_calls: 1, ..DEFAULT_CAPS }, ..Settings::default() }));
        crate::autonomy::with_autonomy(counters.clone(), || {
            let args = serde_json::json!({});
            let rec = Recording::begin_in(s.clone(), "run_lim", "read two files", "api", "agent", None).unwrap();
            assert!(counters.check_before_call("run_lim", "fs.read", &args).is_ok());
            assert!(counters.check_before_call("run_lim", "fs.read", &args).is_err());
            rec.finish("final", 2, "I read one of the two files.", &[]);
        });
        let rows = s.events_after("run_lim", 0, 100).unwrap();
        let limit = rows.iter().find(|e| e.kind == "limit").expect("a limit event");
        assert_eq!((limit.data["stopped_by"].as_str(), limit.data["used"].as_u64(), limit.data["cap"].as_u64()), (Some("max_tool_calls"), Some(1), Some(1)));
        let end = rows.last().unwrap();
        assert_eq!((end.kind.as_str(), end.data["status"].as_str(), end.data["files_changed"].as_u64()), ("run.finished", Some("failed"), Some(0)));
    }

    #[test]
    fn a_failure_with_a_name_a_question_and_a_stop_are_each_told_as_what_they_are() {
        let s = store("events_error_question");
        // A run that returned an error: an `error` event, then the end.
        let rec = Recording::begin_in(s.clone(), "run_err", "g", "api", "agent", None).unwrap();
        rec.finish("error: the model server went away with password=hunter2secret99", 0, "", &[]);
        let rows = s.events_after("run_err", 0, 100).unwrap();
        assert_eq!(rows.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["run.started", "status", "error", "run.finished"]);
        assert_eq!((rows[2].data["code"].as_str(), rows[2].data["retryable"].as_bool()), (Some("internal"), Some(false)));
        assert!(!rows[2].data["message"].as_str().unwrap().contains("hunter2secret99"));
        let rec = Recording::begin_in(s.clone(), "run_llm", "g", "api", "agent", None).unwrap();
        rec.finish("llm_error: no reply", 0, "", &[]);
        let rows = s.events_after("run_llm", 0, 100).unwrap();
        assert_eq!((rows[2].data["code"].as_str(), rows[2].data["retryable"].as_bool()), (Some("model_unavailable"), Some(true)));
        // The person's stop is the end of the run with the state `cancelled`, not an error.
        let rec = Recording::begin_in(s.clone(), "run_stop", "g", "api", "agent", None).unwrap();
        rec.finish("cancelled", 1, "Stopped.", &[]);
        let names = kinds(&s, "run_stop");
        assert!(!names.contains(&"error".to_string()), "{names:?}");
        assert_eq!(s.events_after("run_stop", 0, 100).unwrap().last().unwrap().data["status"], "cancelled");
        // A question: the UI token's, then the state, and no sentences of the answer (the question is not an answer).
        let rec = Recording::begin_in(s.clone(), "run_ask", "g", "api", "agent", None).unwrap();
        note_question("Which of the two budget files do you mean?", &["a.xlsx".to_string(), "b.xlsx".to_string()]);
        rec.finish("waiting_user", 1, "Which of the two budget files do you mean?", &[]);
        let rows = s.events_after("run_ask", 0, 100).unwrap();
        assert_eq!(rows.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), ["run.started", "status", "question", "status", "run.finished"]);
        assert!(rows[2].data.is_empty() && rows[2].ui["text"] == "Which of the two budget files do you mean?" && rows[2].ui["choices"] == json!(["a.xlsx", "b.xlsx"]), "{:?}", rows[2]);
        assert_eq!((rows[3].data["state"].as_str(), rows[4].data["status"].as_str()), (Some("waiting_user"), Some("waiting_user")));
    }

    #[test]
    fn a_run_that_is_dropped_without_finishing_ends_its_log_as_aborted() {
        let s = store("events_drop");
        {
            let _rec = Recording::begin_in(s.clone(), "run_drop", "g", "api", "agent", None).unwrap();
            observe_step(&step(1, "tool_start", json!({"tool": "term.run", "arguments": {}})));
        }
        let rows = s.events_after("run_drop", 0, 100).unwrap();
        let end = rows.last().unwrap();
        assert_eq!((end.kind.as_str(), end.data["status"].as_str(), end.data["stopped_reason"].as_str()), ("run.finished", Some("failed"), Some("aborted")));
        assert_eq!(rows.iter().filter(|e| e.kind == "run.finished").count(), 1);
    }

    fn question(run: Option<&str>, id: &str) -> ConfirmRequest {
        use aqua_mcp_adapters::browser::ConfirmKind;
        ConfirmRequest {
            request_id: id.into(),
            kind: ConfirmKind::Tool,
            tool: "term.run".into(),
            reason: "run a command on this computer".into(),
            url: None,
            site: None,
            created_unix_ms: 1,
            expires_in_secs: 60,
            always_ask: false,
            classes: vec!["EXECUTE".into()],
            risk: "it could change files".into(),
            rule: "commands ask".into(),
            data: vec![],
            expires_unix_ms: 61_000,
            run_id: run.map(str::to_string),
            preview: None,
            run_grant: None,
        }
    }

    /// The questions the person is asked are events of the run: the whole question is the UI's, the run is `waiting_approval` while it is open, and how it ended is told.
    #[test]
    fn the_questions_the_person_is_asked_are_events_of_the_run_they_are_about() {
        let s = store("events_questions");
        let _rec = Recording::begin_in(s.clone(), "run_q", "g", "api", "agent", None).unwrap();
        // A question the host asked for the run (it carries the run) and one the browser asked on the run's own thread (it does not).
        let hosted = question(Some("run_q"), "c_1");
        note_question_opened(&s, &hosted);
        note_question_closed(&s, &hosted, "approved_session");
        let browser = question(None, "c_2");
        note_question_opened(&s, &browser);
        note_question_closed(&s, &browser, "timeout");
        let rows = s.events_after("run_q", 0, 100).unwrap();
        let names: Vec<&str> = rows.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(names, ["run.started", "status", "approval.opened", "status", "approval.closed", "status", "approval.opened", "status", "approval.closed", "status"]);
        assert!(rows[2].data.is_empty() && rows[2].ui["request"]["request_id"] == "c_1" && rows[2].ui["request"]["tool"] == "term.run", "{:?}", rows[2]);
        assert_eq!(rows[3].data["state"], "waiting_approval");
        assert_eq!((rows[4].data["request_id"].as_str(), rows[4].data["outcome"].as_str()), (Some("c_1"), Some("approved_session")));
        assert_eq!(rows[5].data["state"], "running");
        assert_eq!(rows[6].ui["request"]["request_id"], "c_2", "the question the browser asked on the thread of the run is the run's");
        assert_eq!(rows[8].data["outcome"], "timeout");
        // A question about a run that is not recorded here writes nothing (it would be a row that nothing prunes), and nothing fails.
        note_question_opened(&s, &question(Some("run_unknown"), "c_3"));
        assert!(s.events_after("run_unknown", 0, 10).unwrap().is_empty());
        assert_eq!(kinds(&s, "run_q").len(), 10);
    }

    /// Card OA6-04: the change a question shows is part of the question in the run's log, so it is the app's alone (the API token is told that a question is open and nothing of what it is
    /// about), and it is masked there as everything else in the row is.
    #[test]
    fn the_preview_of_a_question_is_in_the_part_of_the_run_log_only_the_app_reads() {
        let s = store("events_preview");
        let _rec = Recording::begin_in(s.clone(), "run_p", "g", "api", "agent", None).unwrap();
        let mut asked = question(Some("run_p"), "c_1");
        asked.preview = Some("--- a.env\n+++ a.env\n@@ -1 +1 @@\n-x\n+token: Bearer abcdefghijklmnop1234567890".into());
        note_question_opened(&s, &asked);
        let rows = s.events_after("run_p", 0, 100).unwrap();
        let opened = rows.iter().find(|e| e.kind == "approval.opened").unwrap();
        assert!(opened.data.is_empty(), "the API token's view of the row has nothing of the question: {:?}", opened.data);
        let shown = opened.ui["request"]["preview"].as_str().unwrap();
        assert!(shown.contains("@@ -1 +1 @@") && !shown.contains("abcdefghijklmnop1234567890"), "{shown}");
    }

    /// Who asked decides who may read the log: a run started inside `with_origin(false)` is the API token's, one started inside `with_origin(true)` the app's, any other nobody's.
    #[test]
    fn a_run_is_recorded_with_who_asked_for_it() {
        let s = store("events_origin");
        let origin = |id: &str| s.log_head(id).unwrap().unwrap().origin;
        drop(Recording::begin_in(s.clone(), "by_default", "g", "voice", "agent", None).unwrap());
        with_origin(false, || drop(Recording::begin_in(s.clone(), "by_script", "g", "agent", "agent", None).unwrap()));
        with_origin(true, || {
            drop(Recording::begin_in(s.clone(), "by_app", "g", "agent", "agent", None).unwrap());
            with_origin(false, || drop(Recording::begin_in(s.clone(), "nested", "g", "agent", "agent", None).unwrap()));
            drop(Recording::begin_in(s.clone(), "after_nested", "g", "agent", "agent", None).unwrap());
        });
        drop(Recording::begin_in(s.clone(), "after_all", "g", "voice", "agent", None).unwrap());
        assert_eq!((origin("by_default"), origin("by_script"), origin("by_app"), origin("nested"), origin("after_nested"), origin("after_all")), (String::new(), "api".into(), "ui".into(), "api".into(), "ui".into(), String::new()));
    }

    /// Security review of OA4-04: the coding record is made of what a check printed and the names of files the model wrote, and every string of it is masked where the record is kept, by the
    /// masking the rest of the record of a run gets, whatever made it. Before the fix the text was kept as it came, so a secret in it (for a call that did not go through the host) was kept.
    #[test]
    fn the_coding_record_is_masked_where_it_is_kept_whatever_made_it() {
        let s = store("coding_record_masked");
        let (token_body, bearer_body) = ("Qx7".repeat(12), "A1b2C3d4".repeat(4));
        let token = format!("{}{}", "ghp_", token_body);
        let rec = Recording::begin_in(s.clone(), "run_masked", "fix it", "agent", "agent", None).unwrap();
        let record = json!({
            "objective": format!("fix the login with {token}"),
            "files_changed": [format!("src/{token}.rs")],
            "last_check": {"kind": "tests", "passed": false, "first_error": format!("401 with Bearer {bearer_body} sent"), "failing": ["login::refuses"], "summary": "exit code 1", "stale": false},
            "next": "run the check again",
        });
        note_coding_record(&record.to_string());
        let kept = s.coding_record_of("run_masked").expect("the record is kept").to_string();
        assert!(!kept.contains(&token_body) && !kept.contains(&bearer_body), "{kept}");
        assert!(kept.contains("[REDACTED]") && kept.contains("login::refuses") && kept.contains("run the check again"), "what is not a secret stays: {kept}");
        // Text that is not JSON is not kept, and does not replace what is.
        note_coding_record("password=hunter2hunter2 {not json");
        assert_eq!(s.coding_record_of("run_masked").unwrap().to_string(), kept);
        drop(rec);
    }
}
