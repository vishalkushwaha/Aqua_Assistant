//! Background agent runs, and the two lanes they and the person's own requests share one model through.
//!
//! One model answers one planner call at a time. A person who is waiting (the **foreground**) must not wait for a long run that nobody is waiting
//! for (the **background**), so the two do not share a turn: a background run takes no turn at all and, at the step boundary before each planner
//! call, waits while any foreground turn is going on. It is paused there holding nothing (the call before it has finished and nothing of the next has
//! begun) and goes on when the foreground is done. The pause costs the background run one thing: the model has probably been used for something else
//! in the meantime, so its next call reads its prompt again (the card measured 15 to 25 s). The person does not pay that.
//!
//! Limits: one background run at a time (one 8B model; a 4B could take two), eight waiting. The queue is in memory: a run that was going when Aqua
//! stopped is in the run record (`route` is `background`; `GET /api/v1/runs?resumable=1` lists it and `POST /api/v1/runs/{id}/resume` goes on from it, when the
//! person says so: nothing is started by itself), a job still waiting is lost.
//!
//! A background run is unattended: a call that would have to ask a person is refused at once (`RequestContext::no_one_to_ask`) and the run says in
//! its answer what needs the person's yes. It keeps the caller class of the request that started it.

use serde::Serialize;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How many jobs may be waiting for the one that is running.
pub const MAX_WAITING: usize = 8;
/// How long a background run may take: it has no person waiting, so the clock of a spoken request does not apply (pauses do not count).
pub const BACKGROUND_RUN_LIMIT: Duration = Duration::from_secs(30 * 60);
/// The longest one pause lasts. A foreground that never ends would otherwise starve the background for ever.
pub const MAX_PAUSE: Duration = Duration::from_secs(30 * 60);
/// Finished jobs kept for `GET /api/v1/queue`.
const KEEP_FINISHED: usize = 30;
const POLL: Duration = Duration::from_millis(100);

static FOREGROUND: AtomicUsize = AtomicUsize::new(0);
static GATE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// A turn of the foreground is going on while this is alive (taken with the turn, dropped with it).
pub struct Foreground(());

pub fn foreground() -> Foreground {
    FOREGROUND.fetch_add(1, Ordering::SeqCst);
    Foreground(())
}

impl Drop for Foreground {
    fn drop(&mut self) {
        FOREGROUND.fetch_sub(1, Ordering::SeqCst);
        let _guard = GATE.0.lock().unwrap_or_else(PoisonError::into_inner);
        GATE.1.notify_all();
    }
}

/// How many foreground turns are going on now.
pub fn foreground_active() -> usize {
    FOREGROUND.load(Ordering::SeqCst)
}

#[derive(Clone)]
struct Ctl {
    cancel: Arc<AtomicBool>,
    paused_ms: Arc<AtomicU64>,
    /// The run is paused at a step because the person is using the model: it is waiting, not working.
    waiting: Arc<AtomicBool>,
}

thread_local! {
    static CURRENT: RefCell<Option<Ctl>> = const { RefCell::new(None) };
}

/// Is the run on this thread a background run?
pub fn in_background() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

/// Called by the agent loop before each planner call. A foreground run, or any other thread, passes at once. A background run waits here while a
/// foreground turn is going on, and ends the wait early when it is cancelled. Returns how long it waited (the run's clock does not count that time).
pub fn at_step_boundary() -> Duration {
    let Some(ctl) = CURRENT.with(|c| c.borrow().clone()) else { return Duration::ZERO };
    wait_while_foreground(&ctl, MAX_PAUSE)
}

fn wait_while_foreground(ctl: &Ctl, max: Duration) -> Duration {
    let started = Instant::now();
    let mut guard = GATE.0.lock().unwrap_or_else(PoisonError::into_inner);
    // The list says "waiting" while this lasts, and "running" again after it.
    if FOREGROUND.load(Ordering::SeqCst) > 0 && !ctl.cancel.load(Ordering::SeqCst) {
        ctl.waiting.store(true, Ordering::SeqCst);
    }
    while FOREGROUND.load(Ordering::SeqCst) > 0 && !ctl.cancel.load(Ordering::SeqCst) && started.elapsed() < max {
        guard = GATE.1.wait_timeout(guard, POLL).unwrap_or_else(PoisonError::into_inner).0;
    }
    ctl.waiting.store(false, Ordering::SeqCst);
    let waited = started.elapsed();
    if waited >= POLL {
        ctl.paused_ms.fetch_add(waited.as_millis() as u64, Ordering::SeqCst);
    }
    waited
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Running,
    /// Started, and paused at a step while the person is using the model: it is waiting, not working (shown in the list; the job is still the one that runs).
    Waiting,
    Done,
    Failed,
    Cancelled,
}

/// What a job's work gives back.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub run_id: Option<String>,
    pub answer: String,
    pub stopped_reason: String,
}

/// A job as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: u64,
    pub goal: String,
    pub state: State,
    pub queued_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub run_id: Option<String>,
    pub answer: String,
    pub stopped_reason: String,
    pub error: String,
    /// How the run came out, once it has ended: `done`, `timed_out`, `needs_you` (it stopped to ask something nobody was there to answer), `cancelled` or `failed`
    /// (`error` says why). A run that ran out of its time or stopped on an error is not a `done` one.
    pub outcome: String,
    /// How long the run waited for the foreground, in all.
    pub paused_ms: u64,
    /// While the state is `waiting`: what it waits for.
    pub waiting_for: String,
}

/// How a run that came back ended, from why it stopped.
pub fn outcome_of(stopped_reason: &str) -> &'static str {
    match stopped_reason {
        // A run that reported no reason came back with what it had: it is not a failure.
        "" => "done",
        "deadline" => "timed_out",
        // A plan run that wrote its plan waits for the person to approve it: what was kept for after it is not started on a guess (card OA10-04, AG-06).
        "plan_ready" => "needs_you",
        other => match crate::agent_runs::status_for(other) {
            "completed" => "done",
            "cancelled" => "cancelled",
            "waiting_user" => "needs_you",
            _ => "failed",
        },
    }
}

type Work = Box<dyn FnOnce() -> Outcome + Send + 'static>;

struct Job {
    view: JobView,
    ctl: Ctl,
    work: Option<Work>,
    /// Its end is to be told to the person at their next turn.
    announce: bool,
}

struct Queue {
    jobs: VecDeque<Job>,
    worker: bool,
    next_id: u64,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: VecDeque::new(), worker: false, next_id: 1 });

fn queue() -> std::sync::MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn clean_goal(goal: &str) -> String {
    let masked = aqua_tool_host::output::Redactor::new().redact(goal);
    let one: String = masked.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 160 {
        format!("{}…", one.chars().take(160).collect::<String>())
    } else {
        one
    }
}

/// How long a key remembers the request it was first used for.
const KEY_MEMORY_MS: i64 = 24 * 3600 * 1000;
/// The most keys remembered at once (the oldest go first).
const MAX_KEYS: usize = 500;
/// The most finished jobs waiting to be told to the person at their next turn.
const MAX_UNANNOUNCED: usize = 8;

struct KeyedStart {
    key: String,
    fingerprint: String,
    job_id: u64,
    at_ms: i64,
}

static KEYS: Mutex<Vec<KeyedStart>> = Mutex::new(Vec::new());
/// A keyed start looks the key up, starts the job and writes the key down as one step: two starts with one key cannot both start a job.
static KEYED_START: Mutex<()> = Mutex::new(());
static UNANNOUNCED: Mutex<VecDeque<Announcement>> = Mutex::new(VecDeque::new());

/// What a start gave back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub id: u64,
    /// 0 is the run that is going (or one that has ended).
    pub place: usize,
    /// The key had been used before for the same request: this is the job that was started then, and nothing new was started.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// Eight are waiting already.
    Full(String),
    /// The key was used for another request.
    KeyMismatch(String),
    /// The key is not 1 to 128 of letters, digits and `_ . : -`.
    BadKey(String),
    Thread(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Full(m) | SubmitError::KeyMismatch(m) | SubmitError::BadKey(m) | SubmitError::Thread(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SubmitError {}

/// A key a caller may send with a start (1 to 128 of letters, digits and `_ . : -`).
pub fn valid_idempotency_key(key: &str) -> bool {
    (1..=128).contains(&key.chars().count()) && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Puts a job in the background queue and answers at once with its number and its place (0 is the run that is going, when this job is next).
pub fn submit(goal: &str, work: impl FnOnce() -> Outcome + Send + 'static) -> Result<(u64, usize), String> {
    submit_inner(goal, work, false).map(|s| (s.id, s.place)).map_err(|e| e.to_string())
}

/// [`submit`] with a key from the caller. The same key with the same request (`fingerprint` says what the request was) starts nothing new: it answers with the job that was
/// started the first time and says so, so a caller that did not hear the first answer (a timeout, a retry) cannot start the work twice. The same key with another request is
/// refused. A key is remembered for a day, in memory (a restart forgets it). `announce` asks for the job's end to be told to the person at their next turn.
pub fn submit_keyed(key: Option<&str>, fingerprint: &str, announce: bool, goal: &str, work: impl FnOnce() -> Outcome + Send + 'static) -> Result<Submission, SubmitError> {
    let Some(key) = key else { return submit_inner(goal, work, announce) };
    if !valid_idempotency_key(key) {
        return Err(SubmitError::BadKey("an idempotency key is 1 to 128 letters, digits and _ . : -".into()));
    }
    let _one = KEYED_START.lock().unwrap_or_else(PoisonError::into_inner);
    let now = now_ms();
    let seen = {
        let mut keys = KEYS.lock().unwrap_or_else(PoisonError::into_inner);
        keys.retain(|k| now - k.at_ms < KEY_MEMORY_MS);
        keys.iter().find(|k| k.key == key).map(|k| (k.fingerprint.clone(), k.job_id))
    };
    if let Some((first, id)) = seen {
        if first != fingerprint {
            return Err(SubmitError::KeyMismatch("this key was used for a different request: use a new key for a new request".into()));
        }
        return Ok(Submission { id, place: place_of(id), duplicate: true });
    }
    let started = submit_inner(goal, work, announce)?;
    let mut keys = KEYS.lock().unwrap_or_else(PoisonError::into_inner);
    keys.push(KeyedStart { key: key.to_string(), fingerprint: fingerprint.to_string(), job_id: started.id, at_ms: now });
    while keys.len() > MAX_KEYS {
        keys.remove(0);
    }
    Ok(started)
}

/// The place of a job: how many wait before it, and one more when a run is going. 0 for one that is running or has ended.
fn place_of(id: u64) -> usize {
    let q = queue();
    let Some(position) = q.jobs.iter().position(|j| j.view.id == id) else { return 0 };
    if q.jobs[position].view.state != State::Queued {
        return 0;
    }
    q.jobs.iter().take(position).filter(|j| j.view.state == State::Queued).count() + usize::from(q.jobs.iter().any(|j| j.view.state == State::Running))
}

/// How a job stands, as the list shows it.
pub fn job_state(id: u64) -> Option<State> {
    list().into_iter().find(|j| j.id == id).map(|j| j.state)
}

fn submit_inner(goal: &str, work: impl FnOnce() -> Outcome + Send + 'static, announce: bool) -> Result<Submission, SubmitError> {
    let mut q = queue();
    let waiting = q.jobs.iter().filter(|j| j.view.state == State::Queued).count();
    if waiting >= MAX_WAITING {
        return Err(SubmitError::Full(format!("{MAX_WAITING} background runs are already waiting: wait for one to finish, or cancel one")));
    }
    let id = q.next_id;
    q.next_id += 1;
    let ctl = Ctl { cancel: Arc::new(AtomicBool::new(false)), paused_ms: Arc::new(AtomicU64::new(0)), waiting: Arc::new(AtomicBool::new(false)) };
    let view = JobView { id, goal: clean_goal(goal), state: State::Queued, queued_at_ms: now_ms(), started_at_ms: None, ended_at_ms: None, run_id: None, answer: String::new(), stopped_reason: String::new(), error: String::new(), outcome: String::new(), paused_ms: 0, waiting_for: String::new() };
    q.jobs.push_back(Job { view, ctl, work: Some(Box::new(work)), announce });
    // Finished jobs beyond the last few are forgotten.
    let finished: Vec<u64> = q.jobs.iter().filter(|j| !matches!(j.view.state, State::Queued | State::Running)).map(|j| j.view.id).collect();
    for old in finished.iter().take(finished.len().saturating_sub(KEEP_FINISHED)) {
        q.jobs.retain(|j| j.view.id != *old);
    }
    let place = waiting + usize::from(q.jobs.iter().any(|j| j.view.state == State::Running));
    if !q.worker {
        q.worker = true;
        if std::thread::Builder::new().name("aqua-background-runs".into()).spawn(worker_loop).is_err() {
            q.worker = false;
            q.jobs.retain(|j| j.view.id != id);
            return Err(SubmitError::Thread("could not start the thread for background runs".into()));
        }
    }
    Ok(Submission { id, place, duplicate: false })
}

/// A background job that has ended, to be told to the person at their next turn.
#[derive(Debug, Clone)]
pub struct Announcement {
    pub job_id: u64,
    pub goal: String,
    /// `done`, `needs_you`, `timed_out` or `failed`.
    pub outcome: String,
    pub answer: String,
}

fn announce_end(job: &Job, outcome: &str, answer: &str) {
    if !job.announce || !matches!(outcome, "done" | "needs_you" | "timed_out" | "failed") {
        return;
    }
    let mut u = UNANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner);
    u.push_back(Announcement { job_id: job.view.id, goal: job.view.goal.clone(), outcome: outcome.to_string(), answer: answer.to_string() });
    while u.len() > MAX_UNANNOUNCED {
        u.pop_front();
    }
}

/// The ends of background jobs that nobody has been told about yet, oldest first. Taken once: they are not told again.
pub fn take_announcements() -> Vec<Announcement> {
    UNANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect()
}

/// What to say at the person's next turn about the background jobs that have ended since the last one, in a sentence or two, or `None` when there is nothing to say. It takes what
/// it tells: it is said once. A job the person cancelled is not told to them.
pub fn announcement_text() -> Option<String> {
    let all = take_announcements();
    if all.is_empty() {
        return None;
    }
    let one_line = |s: &str, n: usize| s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect::<String>();
    let parts: Vec<String> = all
        .iter()
        .map(|a| {
            let goal = one_line(&a.goal, 80);
            let answer = one_line(&a.answer, 300);
            match a.outcome.as_str() {
                "done" if answer.is_empty() => format!("A background task has finished: \u{201c}{goal}\u{201d}."),
                "done" => format!("A background task has finished: \u{201c}{goal}\u{201d}. {answer}"),
                "needs_you" => format!("A background task stopped because it needs your yes: \u{201c}{goal}\u{201d}. {answer}"),
                other => format!("A background task did not finish ({}): \u{201c}{goal}\u{201d}.", other.replace('_', " ")),
            }
        })
        .collect();
    Some(parts.join(" "))
}

/// How full the queue is, for the list and for a caller that wants to know before it starts something.
#[derive(Debug, Clone, Serialize)]
pub struct Depth {
    /// Jobs waiting for the one that runs.
    pub queued: usize,
    pub running: bool,
    /// The run that is going is paused at a step because the person is using the model.
    pub waiting_for_the_person: bool,
    pub max_waiting: usize,
    /// How many more may be started now before one is refused.
    pub free_places: usize,
    /// Ends of jobs that the person has not been told about yet.
    pub to_tell: usize,
}

pub fn depth() -> Depth {
    let q = queue();
    let queued = q.jobs.iter().filter(|j| j.view.state == State::Queued).count();
    Depth {
        queued,
        running: q.jobs.iter().any(|j| j.view.state == State::Running),
        waiting_for_the_person: q.jobs.iter().any(|j| j.view.state == State::Running && j.ctl.waiting.load(Ordering::SeqCst)),
        max_waiting: MAX_WAITING,
        free_places: MAX_WAITING.saturating_sub(queued),
        to_tell: UNANNOUNCED.lock().unwrap_or_else(PoisonError::into_inner).len(),
    }
}

fn worker_loop() {
    loop {
        let (id, ctl, work) = {
            let mut q = queue();
            let Some(job) = q.jobs.iter_mut().find(|j| j.view.state == State::Queued) else {
                q.worker = false;
                return;
            };
            job.view.state = State::Running;
            job.view.started_at_ms = Some(now_ms());
            (job.view.id, job.ctl.clone(), job.work.take())
        };
        let Some(work) = work else { continue };
        CURRENT.with(|c| *c.borrow_mut() = Some(ctl.clone()));
        let _bound = crate::modules::browser_agent::bind_turn_cancel(ctl.cancel.clone());
        // Nobody is there: a question from the browser (an unknown site, Send, Pay, Delete) is refused at once, as a host call's is.
        let unattended = aqua_mcp_adapters::browser::unattended();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        drop(unattended);
        CURRENT.with(|c| *c.borrow_mut() = None);
        let mut q = queue();
        if let Some(job) = q.jobs.iter_mut().find(|j| j.view.id == id) {
            job.view.ended_at_ms = Some(now_ms());
            job.view.paused_ms = ctl.paused_ms.load(Ordering::SeqCst);
            match result {
                Ok(o) => {
                    let outcome = if ctl.cancel.load(Ordering::SeqCst) { "cancelled" } else { outcome_of(&o.stopped_reason) };
                    job.view.state = match outcome {
                        "done" | "needs_you" => State::Done,
                        "cancelled" => State::Cancelled,
                        _ => State::Failed,
                    };
                    if matches!(outcome, "failed" | "timed_out") {
                        job.view.error = o.stopped_reason.clone();
                    }
                    job.view.outcome = outcome.into();
                    job.view.run_id = o.run_id;
                    job.view.answer = o.answer.chars().take(1_200).collect();
                    job.view.stopped_reason = o.stopped_reason;
                    announce_end(job, outcome, &job.view.answer);
                }
                Err(_) => {
                    job.view.state = State::Failed;
                    job.view.outcome = "failed".into();
                    job.view.error = "the run stopped unexpectedly".into();
                    announce_end(job, "failed", "");
                }
            }
        }
    }
}

/// The jobs, newest first.
pub fn list() -> Vec<JobView> {
    let q = queue();
    let mut out: Vec<JobView> = q
        .jobs
        .iter()
        .map(|j| {
            // A run that is paused for the person is shown as waiting, apart from one that is working.
            let waiting = j.view.state == State::Running && j.ctl.waiting.load(Ordering::SeqCst);
            JobView {
                paused_ms: j.ctl.paused_ms.load(Ordering::SeqCst),
                state: if waiting { State::Waiting } else { j.view.state },
                waiting_for: if waiting { "your own request: it goes first, and this run goes on when it is answered".into() } else { String::new() },
                ..j.view.clone()
            }
        })
        .collect();
    out.reverse();
    out
}

/// A job waiting is taken out; one that is running is told to stop at its next step. `Err` says why not.
pub fn cancel(id: u64) -> Result<State, String> {
    let mut q = queue();
    let Some(job) = q.jobs.iter_mut().find(|j| j.view.id == id) else { return Err("there is no such background run".into()) };
    match job.view.state {
        State::Queued => {
            job.view.state = State::Cancelled;
            job.view.ended_at_ms = Some(now_ms());
            job.work = None;
            Ok(State::Cancelled)
        }
        State::Running => {
            job.ctl.cancel.store(true, Ordering::SeqCst);
            Ok(State::Running)
        }
        other => Err(format!("this background run has already ended ({other:?})").to_ascii_lowercase()),
    }
}

/// How many jobs wait, whether one runs, and how many foreground turns are going on.
pub fn counts() -> (usize, bool, usize) {
    let q = queue();
    (q.jobs.iter().filter(|j| j.view.state == State::Queued).count(), q.jobs.iter().any(|j| j.view.state == State::Running), foreground_active())
}

/// Takes the queue's jobs out of the way (tests).
#[cfg(test)]
pub(crate) fn clear_for_test() {
    let mut q = queue();
    q.jobs.retain(|j| j.view.state == State::Running);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The queue is one for the process: tests of it take turns.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn wait_for(what: &str, f: impl FnMut() -> bool) {
        crate::test_support::wait_until(Duration::from_secs(10), what, f);
    }

    fn state_of(id: u64) -> State {
        list().into_iter().find(|j| j.id == id).map(|j| j.state).expect("the job is listed")
    }

    #[test]
    fn jobs_run_one_at_a_time_in_the_order_they_came_and_say_what_they_gave_back() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let (release, gate) = mpsc::channel::<()>();
        let (a, place_a) = submit("research the thing with password=hunter2secret99", move || {
            gate.recv_timeout(Duration::from_secs(10)).ok();
            Outcome { run_id: Some("run_a".into()), answer: "A is done".into(), stopped_reason: "final".into() }
        })
        .unwrap();
        let (b, place_b) = submit("second", || Outcome { run_id: Some("run_b".into()), answer: "B is done".into(), stopped_reason: "final".into() }).unwrap();
        assert_eq!(place_a, 0);
        assert_eq!(place_b, 1, "after the one that runs");
        wait_for("the first job to start", || state_of(a) == State::Running);
        assert_eq!(state_of(b), State::Queued, "one at a time");
        let first = list().into_iter().find(|j| j.id == a).unwrap();
        assert!(first.goal.starts_with("research the thing") && !first.goal.contains("hunter2secret99"), "the goal is shown masked: {}", first.goal);
        release.send(()).unwrap();
        wait_for("both to finish", || state_of(b) == State::Done);
        let done = list();
        assert_eq!(done[0].id, b, "newest first");
        assert_eq!((done[1].state, done[1].run_id.as_deref(), done[1].answer.as_str(), done[1].stopped_reason.as_str()), (State::Done, Some("run_a"), "A is done", "final"));
        assert!(done[1].started_at_ms.is_some() && done[1].ended_at_ms >= done[1].started_at_ms);
    }

    #[test]
    fn a_run_that_ran_out_of_time_or_stopped_on_an_error_is_not_listed_as_done() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let mut ids = Vec::new();
        for reason in ["final", "deadline", "llm_error: the model server stopped", "waiting_user"] {
            let reason = reason.to_string();
            let (id, _) = submit(&format!("a job that ends with {reason}"), move || Outcome { run_id: None, answer: "text".into(), stopped_reason: reason }).unwrap();
            ids.push(id);
        }
        let (panicking, _) = submit("a job that panics", || panic!("the work broke")).unwrap();
        wait_for("the jobs to end", || state_of(panicking) == State::Failed);
        let view = |id: u64| list().into_iter().find(|j| j.id == id).unwrap();
        let seen: Vec<(State, String, String)> = ids.iter().map(|id| view(*id)).map(|j| (j.state, j.outcome, j.error)).collect();
        assert_eq!(seen[0], (State::Done, "done".to_string(), String::new()));
        assert_eq!(seen[1], (State::Failed, "timed_out".to_string(), "deadline".to_string()));
        assert_eq!(seen[2], (State::Failed, "failed".to_string(), "llm_error: the model server stopped".to_string()));
        assert_eq!((seen[3].0, seen[3].1.as_str()), (State::Done, "needs_you"), "it stopped to ask something nobody was there to answer");
        assert_eq!(view(panicking).outcome, "failed");
        assert_eq!(outcome_of("cancelled"), "cancelled");
        assert_eq!(outcome_of("final"), "done");
    }

    #[test]
    fn a_background_run_waits_at_the_step_boundary_while_the_person_is_being_served_and_the_wait_is_not_counted_against_it() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let person = foreground();
        assert_eq!(foreground_active(), 1);
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (passed_tx, passed_rx) = mpsc::channel::<Duration>();
        let (id, _) = submit("long task", move || {
            started_tx.send(()).unwrap();
            // The boundary before the next planner call.
            let waited = at_step_boundary();
            passed_tx.send(waited).unwrap();
            Outcome { answer: "done".into(), stopped_reason: "final".into(), ..Default::default() }
        })
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(passed_rx.recv_timeout(Duration::from_millis(400)).is_err(), "it is paused while the foreground is going on");
        assert_eq!(state_of(id), State::Waiting, "and it is listed as waiting for the person, not as working");
        // The person is done: the run goes on within a poll.
        let released = Instant::now();
        drop(person);
        let waited = passed_rx.recv_timeout(Duration::from_secs(5)).expect("it goes on");
        assert!(released.elapsed() < Duration::from_secs(2) && waited >= Duration::from_millis(400), "{waited:?}");
        wait_for("the job to finish", || state_of(id) == State::Done);
        assert!(list().into_iter().find(|j| j.id == id).unwrap().paused_ms >= 400, "the wait is on the record");
        // Other threads (the foreground's own runs) never wait.
        let _again = foreground();
        assert_eq!(at_step_boundary(), Duration::ZERO);
        assert!(!in_background());
    }

    #[test]
    fn a_waiting_job_is_taken_out_a_running_one_is_told_to_stop_and_a_full_queue_says_so() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let (release, gate) = mpsc::channel::<()>();
        let (running, _) = submit("runs", move || {
            gate.recv_timeout(Duration::from_secs(10)).ok();
            Outcome { answer: "stopped".into(), stopped_reason: "cancelled".into(), ..Default::default() }
        })
        .unwrap();
        wait_for("it to start", || state_of(running) == State::Running);
        let ids: Vec<u64> = (0..MAX_WAITING).map(|i| submit(&format!("waits {i}"), || Outcome::default()).unwrap().0).collect();
        let full = submit("one too many", || Outcome::default());
        assert!(full.unwrap_err().contains("8 background runs are already waiting"));
        assert_eq!(cancel(ids[3]), Ok(State::Cancelled));
        assert_eq!(state_of(ids[3]), State::Cancelled);
        assert!(submit("room again", || Outcome::default()).is_ok(), "a cancelled job made room");
        // A running one is flagged (the agent loop sees it at its next step), and ends as cancelled when it returns.
        assert_eq!(cancel(running), Ok(State::Running));
        release.send(()).unwrap();
        wait_for("it to end", || state_of(running) == State::Cancelled);
        wait_for("the rest to finish", || counts().0 == 0 && !counts().1);
        assert!(cancel(running).unwrap_err().contains("already ended"));
        assert!(cancel(9_999_999).unwrap_err().contains("no such background run"));
        assert_eq!(state_of(ids[3]), State::Cancelled, "the one that was cancelled never ran");
        assert_eq!(state_of(ids[0]), State::Done);
    }

    #[test]
    fn a_background_job_runs_unattended_so_a_question_from_the_browser_is_refused_at_once() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let (tx, rx) = mpsc::channel::<(bool, bool)>();
        let (id, _) = submit("looks", move || {
            tx.send((aqua_mcp_adapters::browser::is_unattended(), in_background())).unwrap();
            Outcome::default()
        })
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (true, true));
        wait_for("the job to finish", || state_of(id) == State::Done);
        assert!(!aqua_mcp_adapters::browser::is_unattended(), "only the worker's thread is unattended");
    }

    #[test]
    fn a_job_that_panics_is_failed_and_the_next_one_still_runs() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let (bad, _) = submit("breaks", || panic!("boom")).unwrap();
        let (good, _) = submit("fine", || Outcome { answer: "ok".into(), ..Default::default() }).unwrap();
        wait_for("both to end", || state_of(good) == State::Done && state_of(bad) == State::Failed);
        assert!(list().into_iter().find(|j| j.id == bad).unwrap().error.contains("stopped unexpectedly"));
    }

    #[test]
    fn a_run_that_waits_for_the_person_is_listed_as_waiting_and_not_as_running_and_goes_on_when_they_are_done() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let person = foreground();
        let (reached, at_boundary) = mpsc::channel::<()>();
        let (id, _) = submit("a long errand", move || {
            reached.send(()).ok();
            at_step_boundary();
            Outcome { run_id: None, answer: "done at last".into(), stopped_reason: "final".into() }
        })
        .unwrap();
        at_boundary.recv_timeout(Duration::from_secs(10)).expect("the job reached its step boundary");
        wait_for("the job to be shown as waiting", || state_of(id) == State::Waiting);
        let view = list().into_iter().find(|j| j.id == id).unwrap();
        assert!(view.waiting_for.contains("your own request") && view.started_at_ms.is_some(), "{view:?}");
        let d = depth();
        assert!(d.running && d.waiting_for_the_person && d.queued == 0, "{d:?}");
        // (A wait shorter than one poll is not counted as a pause.)
        std::thread::sleep(Duration::from_millis(250));
        drop(person);
        wait_for("the job to finish", || state_of(id) == State::Done);
        let done = list().into_iter().find(|j| j.id == id).unwrap();
        assert!(done.waiting_for.is_empty() && done.paused_ms > 0, "{done:?}");
        assert!(!depth().waiting_for_the_person);
    }

    #[test]
    fn a_start_with_a_key_that_was_used_for_the_same_request_starts_nothing_new_and_one_for_another_request_is_refused() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let ran = Arc::new(AtomicUsize::new(0));
        let job = |ran: &Arc<AtomicUsize>| {
            let ran = ran.clone();
            move || {
                ran.fetch_add(1, Ordering::SeqCst);
                Outcome { run_id: None, answer: "answer".into(), stopped_reason: "final".into() }
            }
        };
        let first = submit_keyed(Some("order-42"), "research the thing|8", false, "research the thing", job(&ran)).unwrap();
        assert!(!first.duplicate);
        wait_for("the first start to end", || state_of(first.id) == State::Done);
        // A retry (the caller never heard the first answer): the same job, nothing new, nothing run again.
        let again = submit_keyed(Some("order-42"), "research the thing|8", false, "research the thing", job(&ran)).unwrap();
        assert_eq!((again.id, again.duplicate), (first.id, true));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the work was done once");
        assert_eq!(list().iter().filter(|j| j.goal == "research the thing").count(), 1);
        // The same key for another request is not the same request.
        let other = submit_keyed(Some("order-42"), "something else|8", false, "something else", job(&ran)).unwrap_err();
        assert!(matches!(other, SubmitError::KeyMismatch(_)), "{other:?}");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // A key that is not a key, and no key at all.
        for bad in ["", "has space", "x".repeat(129).as_str(), "semi;colon", "caf\u{e9}"] {
            assert!(matches!(submit_keyed(Some(bad), "f", false, "g", job(&ran)), Err(SubmitError::BadKey(_))), "{bad:?}");
        }
        let (a, _) = submit("no key", job(&ran)).unwrap();
        let (b, _) = submit("no key", job(&ran)).unwrap();
        assert_ne!(a, b, "without a key every start is a new job");
        wait_for("the unkeyed jobs", || state_of(b) == State::Done && state_of(a) == State::Done);
        assert!(valid_idempotency_key("order-42") && valid_idempotency_key("a.b:c_d-1") && !valid_idempotency_key(""));
    }

    #[test]
    fn two_starts_with_one_key_at_the_same_moment_start_one_job() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let ran = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let ran = ran.clone();
                std::thread::spawn(move || {
                    submit_keyed(Some("race-1"), "same", false, "same request", move || {
                        ran.fetch_add(1, Ordering::SeqCst);
                        Outcome { run_id: None, answer: String::new(), stopped_reason: "final".into() }
                    })
                    .unwrap()
                })
            })
            .collect();
        let results: Vec<Submission> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|s| !s.duplicate).count(), 1, "{results:?}");
        assert!(results.iter().all(|s| s.id == results[0].id));
        wait_for("the job to end", || state_of(results[0].id) == State::Done);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_end_of_a_job_that_asked_for_it_is_told_once_at_the_next_turn_and_one_that_was_cancelled_is_not() {
        let _g = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        clear_for_test();
        let _ = take_announcements();
        let finish = |goal: &str, answer: &str, reason: &str, announce: bool| {
            let (answer, reason) = (answer.to_string(), reason.to_string());
            let s = submit_keyed(None, "", announce, goal, move || Outcome { run_id: None, answer, stopped_reason: reason }).unwrap();
            wait_for("a job to end", || !matches!(state_of(s.id), State::Queued | State::Running | State::Waiting));
        };
        finish("quiet one", "not told", "final", false);
        assert!(announcement_text().is_none(), "a job that did not ask is not told");
        finish("research the thing", "The thing is blue.", "final", true);
        finish("needs a yes", "I could not write the file: it needs your yes.", "waiting_user", true);
        finish("went wrong", "", "llm_error: the model stopped", true);
        let said = announcement_text().expect("three ends to tell");
        assert!(said.contains("finished: \u{201c}research the thing\u{201d}. The thing is blue."), "{said}");
        assert!(said.contains("needs your yes: \u{201c}needs a yes\u{201d}. I could not write the file"), "{said}");
        assert!(said.contains("did not finish (failed): \u{201c}went wrong\u{201d}"), "{said}");
        assert!(announcement_text().is_none() && depth().to_tell == 0, "told once");
        // One the person cancelled is not told to them.
        let (release, hold) = mpsc::channel::<()>();
        let s = submit_keyed(None, "", true, "cancelled by the person", move || {
            hold.recv_timeout(Duration::from_secs(10)).ok();
            Outcome { run_id: None, answer: "x".into(), stopped_reason: "cancelled".into() }
        })
        .unwrap();
        wait_for("it to start", || state_of(s.id) == State::Running);
        cancel(s.id).unwrap();
        release.send(()).unwrap();
        wait_for("it to end", || state_of(s.id) == State::Cancelled);
        assert!(announcement_text().is_none(), "the person cancelled it themselves");
    }
}
