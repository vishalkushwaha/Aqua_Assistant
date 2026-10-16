//! Runs that are detached from any connection and still ask the person (card OA10-03; tasks UX-03 and UX-11 of the OpenCode audit).
//!
//! `POST /api/v1/runs` starts an agent run and answers at once (202) with the id the run will be recorded under. The run holds the model's turn, like any run the person waits for, so
//! the person's voice and typed requests queue behind it (decision D-09, question 5: one model, one turn); but nothing holds the *connection*: the client may hang up, reload the app or
//! never come back, and the run goes on. Its questions are put as they always were (the confirmation broker, which the app lists and answers at `POST /api/v1/browser/confirm` with the
//! UI token: **a detached run has no other way to be approved**), and wait for `approval_wait_secs` (60 s unless the request says up to 600) before they count as a "no". What it does is
//! read from its log (`GET /api/v1/runs/{id}/events` or `/stream`, which show it as `queued` until it begins), asked about at `GET /api/v1/runs/{id}/wait`, and ended at
//! `POST /api/v1/runs/{id}/interrupt`.
//!
//! This file is the registry of such runs (what is queued, what is going, how each ended, which idempotency key started which) and the task that drives one. It holds no model and no tool:
//! what a run does is a closure the caller gives it, so that a test gives a scripted one.
//!
//! * **One at a time.** Detached runs take the turn in the order they were started (the gate is a queue); at most [`MAX_DETACHED`] are queued or going.
//! * **Lines for after the run.** What the person said to be kept for after a run (`steer::Delivery::Queue`), and what they said to it that it ended without reading, starts the next run when
//!   this one ended well ([`drive`]); when it did not, they stay where they were (`GET /api/v1/runs/{id}/queue`), never started on a guess.
//! * **Who may see what.** A run started with the UI token is the app's; one started with the API token is that token's, which can see it, steer it and end it, and no other run.

use super::attention;
use super::server::{for_client, run_turn, take_turn_within};
use crate::agent_runs;
use crate::modules::browser_agent::McpAgentResult;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How many detached runs may be queued or going at once.
pub const MAX_DETACHED: usize = 8;
/// The runs that ended that are still remembered, for `GET /api/v1/runs/{id}/wait` and the idempotency keys that started them.
const KEEP_ENDED: usize = 30;
/// How long a detached run waits for the turn behind the runs in front of it: two hours (the longest a run may take is two).
pub const TURN_WAIT: Duration = Duration::from_secs(2 * 3600);
/// The shortest `approval_wait_secs` a request may ask for, and the longest (ten minutes, as the host allows).
pub const MIN_APPROVAL_WAIT: u64 = 10;
pub const MAX_APPROVAL_WAIT: u64 = aqua_tool_host::MAX_APPROVAL_WAIT_SECS;
/// How long an idempotency key remembers its request.
const KEY_MEMORY_MS: i64 = 24 * 3600 * 1000;
const MAX_KEYS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for the model's turn behind the runs in front.
    Queued,
    Running,
    Ended,
}

/// How a detached run came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The state of the run's record (`completed`, `failed`, `cancelled`, `waiting_user` ...).
    pub status: String,
    pub stopped_reason: String,
    /// `done`, `needs_you` (it stopped to ask something, or nobody answered its question), `timed_out`, `cancelled` or `failed`.
    pub outcome: &'static str,
    /// The first part of the answer (masked, cut).
    pub answer_head: String,
}

impl Ended {
    fn cancelled(why: &str) -> Ended {
        Ended { status: "cancelled".into(), stopped_reason: "cancelled".into(), outcome: "cancelled", answer_head: why.into() }
    }

    fn failed(reason: &str, why: &str) -> Ended {
        Ended { status: "failed".into(), stopped_reason: reason.into(), outcome: "failed", answer_head: why.into() }
    }
}

struct Entry {
    id: String,
    origin: &'static str,
    goal_head: String,
    state: State,
    queued_ms: i64,
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    ended: Option<Ended>,
    approval_wait_secs: Option<u64>,
    /// Wakes the task that waits for its turn, when the run is taken out of the queue.
    cancel: Arc<tokio::sync::Notify>,
    /// Set when the run was ended on purpose (the person said stop), so that a run that was ended is not followed by what was queued behind it.
    interrupted: Arc<AtomicBool>,
}

struct KeyedStart {
    key: String,
    fingerprint: String,
    id: String,
    at_ms: i64,
}

/// The detached runs of a process, and the keys that started them. The process has one ([`global`]); a test makes its own, so that tests do not take each other's places.
pub struct Registry {
    entries: Mutex<Vec<Entry>>,
    keys: Mutex<Vec<KeyedStart>>,
    /// A keyed start looks the key up, registers the run and writes the key down as one step: two starts with one key cannot both start a run.
    keyed_start: Mutex<()>,
    /// Woken when a run has taken its turn or has ended: the runs that were registered after it may now ask for theirs.
    moved: tokio::sync::Notify,
}

static GLOBAL: Registry = Registry::new();

/// The registry of this process.
pub fn global() -> &'static Registry {
    &GLOBAL
}

fn now_ms() -> i64 {
    agent_runs::now_ms()
}

/// What starting a run came to.
#[derive(Debug, Clone)]
pub struct Registered {
    pub id: String,
    /// How many detached runs are in front of it: 0 when it is the one going (or about to).
    pub position: usize,
    /// The key had been used before for the same request: this is the run that was started then, and nothing new was.
    pub duplicate: bool,
    cancel: Arc<tokio::sync::Notify>,
    interrupted: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// [`MAX_DETACHED`] are queued or going already.
    Full,
    /// The key was used for another request.
    KeyMismatch,
    /// The key is not 1 to 128 of letters, digits and `_ . : -`.
    BadKey,
}

impl StartError {
    pub fn message(&self) -> String {
        match self {
            StartError::Full => format!("{MAX_DETACHED} runs are queued or going already: wait for one to end, or end one"),
            StartError::KeyMismatch => "this key was used for a different request: use a new key for a new request".into(),
            StartError::BadKey => "an idempotency key is 1 to 128 letters, digits and _ . : -".into(),
        }
    }
}

fn one_line(text: &str, max: usize) -> String {
    let masked = aqua_tool_host::output::Redactor::new().redact(text);
    let one: String = masked.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        format!("{}…", one.chars().take(max).collect::<String>())
    } else {
        one
    }
}

fn position_in(entries: &[Entry], id: &str) -> Option<usize> {
    let at = entries.iter().position(|e| e.id == id)?;
    if entries[at].state == State::Ended {
        return None;
    }
    Some(entries[..at].iter().filter(|e| e.state != State::Ended).count())
}

/// A run of the registry as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub id: String,
    /// `ui` or `api`.
    pub origin: &'static str,
    pub goal_head: String,
    pub state: State,
    /// How many detached runs are in front of it (0: it is the one going).
    pub position: usize,
    pub queued_ms: i64,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub ended: Option<Ended>,
    pub approval_wait_secs: Option<u64>,
}

/// What ending a run on purpose came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interrupted {
    /// It was waiting for its turn and was taken out of the queue: nothing ran.
    Dequeued,
    /// It is the one going: the caller stops the turn (`turn_control::stop_current`).
    Running,
    /// It had ended already.
    AlreadyOver,
    /// It is not a detached run.
    Unknown,
}

impl Registry {
    pub const fn new() -> Self {
        Registry { entries: Mutex::new(Vec::new()), keys: Mutex::new(Vec::new()), keyed_start: Mutex::new(()), moved: tokio::sync::Notify::const_new() }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts a run in the registry as queued, with the id it will be recorded under. With a `key` the same key and the same request (`fingerprint` says what the request was) give back the
    /// run that was started the first time, and start nothing: a client that did not hear the answer (a time-out, a retry) cannot start the work twice.
    pub fn register(&self, origin: &'static str, goal: &str, key: Option<&str>, fingerprint: &str, approval_wait_secs: Option<u64>) -> Result<Registered, StartError> {
        if let Some(key) = key {
            if !crate::queue::valid_idempotency_key(key) {
                return Err(StartError::BadKey);
            }
        }
        let _one = self.keyed_start.lock().unwrap_or_else(PoisonError::into_inner);
        let now = now_ms();
        if let Some(key) = key {
            let seen = {
                let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
                keys.retain(|k| now - k.at_ms < KEY_MEMORY_MS);
                keys.iter().find(|k| k.key == key).map(|k| (k.fingerprint.clone(), k.id.clone()))
            };
            if let Some((first, id)) = seen {
                if first != fingerprint {
                    return Err(StartError::KeyMismatch);
                }
                let entries = self.entries();
                if let Some(e) = entries.iter().find(|e| e.id == id) {
                    return Ok(Registered { position: position_in(&entries, &id).unwrap_or(0), id, duplicate: true, cancel: e.cancel.clone(), interrupted: e.interrupted.clone() });
                }
                // The run has been forgotten (many ended since): the key is spent with it, and this is a new start.
            }
        }
        let mut entries = self.entries();
        if entries.iter().filter(|e| e.state != State::Ended).count() >= MAX_DETACHED {
            return Err(StartError::Full);
        }
        let id = crate::agent_runs::scope::new_run_id();
        let (cancel, interrupted) = (Arc::new(tokio::sync::Notify::new()), Arc::new(AtomicBool::new(false)));
        entries.push(Entry {
            id: id.clone(),
            origin,
            goal_head: one_line(goal, 160),
            state: State::Queued,
            queued_ms: now,
            started_ms: None,
            ended_ms: None,
            ended: None,
            approval_wait_secs,
            cancel: cancel.clone(),
            interrupted: interrupted.clone(),
        });
        // Ended runs beyond the last few are forgotten.
        let ended: Vec<String> = entries.iter().filter(|e| e.state == State::Ended).map(|e| e.id.clone()).collect();
        for old in ended.iter().take(ended.len().saturating_sub(KEEP_ENDED)) {
            entries.retain(|e| &e.id != old);
        }
        let position = position_in(&entries, &id).unwrap_or(0);
        drop(entries);
        if let Some(key) = key {
            let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
            keys.push(KeyedStart { key: key.to_string(), fingerprint: fingerprint.to_string(), id: id.clone(), at_ms: now });
            while keys.len() > MAX_KEYS {
                keys.remove(0);
            }
        }
        Ok(Registered { id, position, duplicate: false, cancel, interrupted })
    }

    /// The run `id`, if it is one that was started detached.
    pub fn lookup(&self, id: &str) -> Option<View> {
        let entries = self.entries();
        let e = entries.iter().find(|e| e.id == id)?;
        Some(View {
            id: e.id.clone(),
            origin: e.origin,
            goal_head: e.goal_head.clone(),
            state: e.state,
            position: position_in(&entries, id).unwrap_or(0),
            queued_ms: e.queued_ms,
            started_ms: e.started_ms,
            ended_ms: e.ended_ms,
            ended: e.ended.clone(),
            approval_wait_secs: e.approval_wait_secs,
        })
    }

    /// The detached runs that are not over, oldest first.
    pub fn live(&self) -> Vec<View> {
        let ids: Vec<String> = self.entries().iter().filter(|e| e.state != State::Ended).map(|e| e.id.clone()).collect();
        ids.iter().filter_map(|id| self.lookup(id)).collect()
    }

    fn mark_running(&self, id: &str) {
        if let Some(e) = self.entries().iter_mut().find(|e| e.id == id) {
            e.state = State::Running;
            e.started_ms = Some(now_ms());
        }
        self.moved.notify_waiters();
    }

    /// How many runs registered before `id` have not yet taken their turn or ended.
    fn waiting_before(&self, id: &str) -> usize {
        let entries = self.entries();
        let Some(at) = entries.iter().position(|e| e.id == id) else { return 0 };
        entries[..at].iter().filter(|e| e.state == State::Queued).count()
    }

    /// Returns when every run registered before `id` has taken its turn or ended, so that the runs take the one turn in the order they were started: the tasks that drive them are
    /// scheduled in any order, and asking for the turn in that order would let a later run go first.
    async fn wait_for_my_place(&self, id: &str) {
        loop {
            // Armed before the registry is looked at: a run that moves between the look and the wait still wakes the wait.
            let moved = self.moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            if self.waiting_before(id) == 0 {
                return;
            }
            // (A second is the longest a missed wake-up can cost: the look is cheap.)
            let _ = tokio::time::timeout(Duration::from_secs(1), moved).await;
        }
    }

    /// Writes down how the run ended; `false` when it is not in the registry.
    fn mark_ended(&self, id: &str, ended: Ended) -> bool {
        let mut entries = self.entries();
        let Some(e) = entries.iter_mut().find(|e| e.id == id) else { return false };
        e.state = State::Ended;
        e.ended_ms = Some(now_ms());
        e.ended = Some(ended);
        drop(entries);
        self.moved.notify_waiters();
        true
    }

    /// Ends the run on purpose: one that waits is taken out of the queue, one that is going is marked so that nothing queued behind it starts, and the caller stops it. What was kept
    /// for after it, and what it did not read, is let go: a person who ends a run does not want the next one it would have started.
    pub fn interrupt(&self, id: &str) -> Interrupted {
        let (what, cancel) = {
            let mut entries = self.entries();
            let Some(e) = entries.iter_mut().find(|e| e.id == id) else { return Interrupted::Unknown };
            match e.state {
                State::Ended => return Interrupted::AlreadyOver,
                State::Queued => {
                    e.interrupted.store(true, Ordering::SeqCst);
                    (Interrupted::Dequeued, Some(e.cancel.clone()))
                }
                State::Running => {
                    e.interrupted.store(true, Ordering::SeqCst);
                    (Interrupted::Running, None)
                }
            }
        };
        let _ = crate::steer::take_run_end(id);
        if let Some(cancel) = cancel {
            // The task that waits for the turn wakes and ends the entry (a permit is kept if it is not waiting yet).
            cancel.notify_one();
        }
        what
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

/// [`Registry::register`] on the registry of the process.
pub fn register(origin: &'static str, goal: &str, key: Option<&str>, fingerprint: &str, approval_wait_secs: Option<u64>) -> Result<Registered, StartError> {
    global().register(origin, goal, key, fingerprint, approval_wait_secs)
}

/// The run `id` of the registry of the process: `None` for a run that is not one (a run the voice or a plain request made).
pub fn lookup(id: &str) -> Option<View> {
    global().lookup(id)
}

/// Is this a detached run that is waiting for the model's turn (so that no record of it exists yet)?
pub fn is_queued(id: &str) -> bool {
    lookup(id).is_some_and(|v| v.state == State::Queued)
}

/// Is this a run that was started detached (queued, going or ended)? Only such a run has someone to start what is kept for after it.
pub fn is_detached(id: &str) -> bool {
    lookup(id).is_some()
}

/// The detached runs of the process that are not over, oldest first.
pub fn live() -> Vec<View> {
    global().live()
}

/// Ends the detached run `id` on purpose (see [`Registry::interrupt`]).
pub fn interrupt(id: &str) -> Interrupted {
    global().interrupt(id)
}

/// The answer of a run that never had a record of its own (the weather, a refusal of the gate, an error before the loop began): made so that its events and its answer can be read like
/// any run's. Nothing is written for a run that has one.
fn ensure_record(id: &str, goal: &str, ui: bool, ended: &Ended) {
    let Some(store) = agent_runs::store() else { return };
    if store.log_head(id).ok().flatten().is_some() {
        return;
    }
    agent_runs::with_origin(ui, || {
        if let Some(rec) = agent_runs::Recording::begin_in(store.clone(), id, goal, "api", "detached", None) {
            rec.finish(&ended.stopped_reason, 0, &ended.answer_head, &[]);
        }
    });
}

/// How a run that returned a result came out: the record's state and the sentence for the outcome. A run that stopped because its question went unanswered is `needs_you`, not a
/// refusal the person made: the log says when an approval ended by timeout.
fn ended_of(result: &McpAgentResult) -> Ended {
    let mut outcome = crate::queue::outcome_of(&result.stopped_reason);
    if result.stopped_reason == "user_declined" && result.run_id.as_deref().is_some_and(had_an_approval_time_out) {
        outcome = "needs_you";
    }
    Ended { status: agent_runs::status_for(&result.stopped_reason).to_string(), stopped_reason: result.stopped_reason.clone(), outcome, answer_head: one_line(&result.final_answer, 600) }
}

/// Did a question of this run end because nobody answered it?
fn had_an_approval_time_out(run_id: &str) -> bool {
    let Some(store) = agent_runs::store() else { return false };
    store.events_after(run_id, 0, 1_000).unwrap_or_default().iter().any(|row| row.kind == "approval.closed" && row.data.get("outcome").and_then(Value::as_str) == Some("timeout"))
}

/// What a detached run does: the closure for a thread that has the model's turn.
pub type Work = Box<dyn FnOnce() -> anyhow::Result<McpAgentResult> + Send + 'static>;
/// Makes the work of a run from its goal (the first goal, and the goal of each run started from what was queued).
pub type MakeWork = Arc<dyn Fn(&str) -> Work + Send + Sync>;

/// The parts of a start that the next run (started from what was queued) takes over.
#[derive(Clone)]
pub struct Drive {
    pub origin: &'static str,
    pub approval_wait_secs: Option<u64>,
    pub make: MakeWork,
}

/// Drives one registered run to its end, and the runs that were queued behind it after it. It is the body of the task the route spawns: the connection of the request is not in it.
pub async fn drive(first: Registered, goal: String, how: Drive) {
    let (mut current, mut goal) = (first, goal);
    while let Some((next, next_goal)) = drive_one(&current, &goal, &how).await {
        current = next;
        goal = next_goal;
    }
}

/// One run: waits for the turn (or for the person to take it out of the queue), runs, records how it came out, and says what to start next, if anything.
async fn drive_one(run: &Registered, goal: &str, how: &Drive) -> Option<(Registered, String)> {
    let id = run.id.clone();
    let ui = how.origin == "ui";
    let end = |ended: Ended| {
        // What the person pre-approved for this run (card OA10-05) is over with it.
        if let Some(host) = crate::agent_tools::host() {
            host.preapprovals().release_run(&id);
        }
        ensure_record(&id, goal, ui, &ended);
        if global().mark_ended(&id, ended.clone()) {
            attention::run_ended(&id, &ended);
        }
    };
    let turn = tokio::select! {
        biased;
        _ = run.cancel.notified() => {
            end(Ended::cancelled("The run was taken out of the queue before it started."));
            return None;
        }
        t = async {
            // In the order the runs were started: the one in front has its turn (or is gone) before this one asks for its own.
            global().wait_for_my_place(&id).await;
            take_turn_within(TURN_WAIT).await
        } => t,
    };
    let Ok(turn) = turn else {
        end(Ended::failed("busy", "Aqua stayed busy with other requests, so this run never started."));
        return None;
    };
    // It was taken out of the queue at the moment it got its turn: it does not start.
    if run.interrupted.load(Ordering::SeqCst) {
        drop(turn);
        end(Ended::cancelled("The run was ended before it started."));
        return None;
    }
    global().mark_running(&id);
    let work = (how.make)(goal);
    let (planned, wait, ended_before) = (id.clone(), how.approval_wait_secs, run.interrupted.clone());
    let result = run_turn(move || {
        // The person ended it between its turn and its first step: it does not begin.
        if ended_before.load(Ordering::SeqCst) {
            return Err("stopped: the run was ended before it began".to_string());
        }
        for_client(ui, || crate::agent_runs::scope::with_planned_run_id(&planned, || crate::agent_runs::scope::with_approval_wait(wait, || work().map_err(|e| format!("{e:#}")))))
    })
    .await;
    // The turn goes when the run has really ended, as it does for a run the person waits for.
    drop(turn);
    let ended = match &result {
        Ok(r) => ended_of(r),
        Err(e) if e.trim_start().to_lowercase().starts_with("stopped") => Ended::cancelled("The run was stopped."),
        Err(e) => Ended::failed("error", &one_line(e, 300)),
    };
    let interrupted = run.interrupted.load(Ordering::SeqCst);
    end(ended.clone());
    if interrupted || ended.outcome != "done" {
        // Not started on a guess: what was kept for after a run that did not end well stays under it (`GET /api/v1/runs/{id}/queue`).
        return None;
    }
    next_from_queue(&id, how)
}

/// What starts after a run that ended well: the first of the steered lines it did not read and the lines kept for after it, as a run of its own, with the rest kept for that one.
fn next_from_queue(ended_run: &str, how: &Drive) -> Option<(Registered, String)> {
    let mut lines = crate::steer::take_run_end(ended_run);
    if lines.is_empty() {
        return None;
    }
    let goal = lines.remove(0);
    match register(how.origin, &goal, None, "", how.approval_wait_secs) {
        Ok(next) => {
            // What is left is kept for the run that goes on; a line that does not fit is not lost silently: it is said.
            let dropped = crate::steer::queue_for(&next.id, lines);
            if dropped > 0 {
                tracing::warn!("{dropped} line(s) kept for after run {ended_run} did not fit the next run and were dropped");
            }
            Some((next, goal))
        }
        Err(why) => {
            // Too many runs: the lines go back under the run they were kept for, to be read and said again.
            let _ = crate::steer::queue_for(ended_run, std::iter::once(goal).chain(lines).collect());
            tracing::warn!("what was kept for after run {ended_run} was not started: {}", why.message());
            None
        }
    }
}

/// Forgets a run that no task drives (a test registered it by hand and ended it by hand).
#[cfg(test)]
pub(crate) fn forget_for_test(id: &str) {
    global().entries().retain(|e| e.id != id);
    global().moved.notify_waiters();
}

/// A short account of a run of the registry, for the lists and the wait.
pub fn summary(view: &View) -> Value {
    json!({
        "run_id": view.id,
        "origin": view.origin,
        "goal": view.goal_head,
        "state": view.state,
        "position": view.position,
        "queued_at": view.queued_ms,
        "started_at": view.started_ms,
        "ended_at": view.ended_ms,
        "approval_wait_secs": view.approval_wait_secs,
        "outcome": view.ended.as_ref().map(|e| e.outcome),
        "stopped_reason": view.ended.as_ref().map(|e| e.stopped_reason.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_registered_queued_behind_the_ones_in_front_and_a_ninth_is_refused() {
        let r = Registry::new();
        let first = r.register("ui", "tidy the notes", None, "", None).unwrap();
        let second = r.register("api", "write the changelog", None, "", Some(120)).unwrap();
        assert_eq!((first.position, second.position), (0, 1), "the second is behind the first");
        let view = r.lookup(&second.id).unwrap();
        assert_eq!((view.origin, view.state, view.goal_head.as_str(), view.approval_wait_secs), ("api", State::Queued, "write the changelog", Some(120)));
        assert!(r.lookup("run_nobody").is_none());
        // The first begins and ends: the second moves up.
        r.mark_running(&first.id);
        assert_eq!(r.lookup(&second.id).unwrap().position, 1, "behind the one that is going");
        assert!(r.mark_ended(&first.id, Ended::cancelled("x")));
        assert_eq!(r.lookup(&second.id).unwrap().position, 0);
        assert_eq!(r.live().iter().map(|v| v.id.clone()).collect::<Vec<_>>(), vec![second.id.clone()]);
        // Eight at once, then no more until one is over.
        let more: Vec<_> = (0..MAX_DETACHED - 1).map(|i| r.register("api", &format!("run {i}"), None, "", None).unwrap()).collect();
        assert_eq!(r.register("api", "one too many", None, "", None).unwrap_err(), StartError::Full);
        assert!(r.mark_ended(&more[0].id, Ended::cancelled("x")));
        assert!(r.register("api", "now there is room", None, "", None).is_ok());
    }

    #[test]
    fn the_same_key_with_the_same_request_is_the_same_run_and_with_another_request_is_refused() {
        let r = Registry::new();
        let a = r.register("api", "do the thing", Some("key.oa10-03.a"), "fp1", None).unwrap();
        assert!(!a.duplicate);
        let again = r.register("api", "do the thing", Some("key.oa10-03.a"), "fp1", None).unwrap();
        assert_eq!((again.id.as_str(), again.duplicate), (a.id.as_str(), true), "a retry gets the run that was started the first time");
        assert_eq!(r.live().len(), 1, "and nothing new was started");
        assert_eq!(r.register("api", "something else", Some("key.oa10-03.a"), "fp2", None).unwrap_err(), StartError::KeyMismatch);
        assert_eq!(r.register("api", "x", Some("not a key!"), "fp", None).unwrap_err(), StartError::BadKey);
        assert_eq!(r.register("api", "x", Some(&"k".repeat(129)), "fp", None).unwrap_err(), StartError::BadKey);
    }

    #[test]
    fn ending_a_run_on_purpose_takes_a_waiting_one_out_of_the_queue_and_marks_the_one_that_is_going() {
        let r = Registry::new();
        let going = r.register("ui", "going", None, "", None).unwrap();
        let waiting = r.register("api", "waiting", None, "", None).unwrap();
        r.mark_running(&going.id);
        assert_eq!(r.interrupt(&waiting.id), Interrupted::Dequeued);
        assert!(waiting.interrupted.load(Ordering::SeqCst), "the task that waits for the turn sees it");
        assert_eq!(r.interrupt(&going.id), Interrupted::Running);
        assert!(going.interrupted.load(Ordering::SeqCst), "and the one that is going is marked, so that nothing queued behind it starts");
        r.mark_ended(&going.id, Ended::cancelled("stopped"));
        assert_eq!(r.interrupt(&going.id), Interrupted::AlreadyOver);
        assert_eq!(r.interrupt("run_nobody"), Interrupted::Unknown);
    }

    #[test]
    fn what_was_said_to_a_run_is_let_go_when_the_person_ends_it() {
        let r = Registry::new();
        let run = r.register("ui", "going", None, "", None).unwrap();
        crate::steer::accept_for(&run.id, "also add tests", crate::steer::Delivery::Steer, None).unwrap();
        crate::steer::accept_for(&run.id, "run clippy", crate::steer::Delivery::Queue, None).unwrap();
        assert_eq!(r.interrupt(&run.id), Interrupted::Dequeued);
        assert_eq!(crate::steer::waiting_for(&run.id), (Vec::new(), Vec::new()), "nothing is left to start");
    }
}
