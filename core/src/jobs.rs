//! The registry of Aqua's background jobs (card OA3-04; task EX-14 of the OpenCode audit).
//!
//! The core runs a few dozen long-lived threads: the flow scheduler that also fires reminders, the clock that wakes waiting flow runs, the history prune, the workflow scheduler,
//! the self-repair watch, the nightly skill tidy, the run watchdog, the sweep of old scratch files, the start of the MCP servers. Each was a bare `thread::spawn`: nobody could list
//! them, a panic in one that was not supervised left no trace, a loop that had stopped looking was indistinguishable from one that was waiting, and on exit nothing stopped them in
//! any order. A job started through [`spawn`] (or [`spawn_supervised`], for a loop that must be started again after a panic, see `supervise`) is in this registry for as long as
//! it lives and after:
//!
//! * [`list`] says what is running, since when, when it last showed a sign of life, and how it ended (finished, cancelled, failed with a reason, panicked with a masked message). `GET
//!   /api/v1/jobs` gives the same to the app (UI token), the gauge `aqua_jobs{kind,state}` to a scraper, and the doctor reads a small record of the jobs that are stuck or ended badly.
//! * a job is **cooperative**: it gets a [`JobCtx`] and asks it [`cancelled`](JobCtx::cancelled), and it waits with [`sleep`](JobCtx::sleep), which wakes at once when the job is
//!   cancelled (a loop that slept five seconds with `thread::sleep` could not be stopped under five seconds, and one that slept an hour could not be stopped at all). Nothing kills a
//!   thread: one that is inside a long database or network call keeps running until the call returns, and [`shutdown`] says so by name instead of waiting for it.
//! * a job that promises a sign of life every `expect_beat` (it calls [`beat`](JobCtx::beat), and `sleep` does it on the way in and out) is **stuck** when it has shown none for five
//!   times that. A job with no promise (a one-shot, such as the start of the MCP servers) is never called stuck.
//! * [`shutdown`] cancels every job that is still going, waits up to the grace period for them in one go, and returns the names of those that did not end. The core calls it when the
//!   server has returned, and (with a shorter grace) before it leaves because the desktop app is gone. After it no new job is started.
//!
//! Nothing here reads or writes what a job works on: only its name, kind, times and the one-line reason it ended. That is why the list can be shown without telling anyone what was done.

use crate::supervise::{masked, payload_text, Backoff};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// A job's number, from 1, in the order the jobs were started. A job that is started again after the core restarts has a number of its own: the number says "this run of this job".
pub type JobId = u64;

/// A job is stuck when it has shown no sign of life for this many times what it promised.
pub const STUCK_AFTER_BEATS: u32 = 5;
/// The most jobs that are over that the registry still lists; the oldest are forgotten first (a job that ran is not worth a place for ever).
pub const KEEP_OVER: usize = 100;
/// How often the monitor looks at the list.
const MONITOR_EVERY: Duration = Duration::from_secs(30);
/// A record the monitor wrote is believed for this long.
pub const RECORD_FRESH_MS: i64 = 3 * 60 * 1000;

/// What a job is for. The kind is what a person sorts by and what the gauge is labelled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Looks at what is due and starts it (the flow scheduler, the workflow scheduler, the clock that wakes flow runs).
    Scheduler,
    /// Looks at the state of something and notes a change (the run watchdog, the self-repair watch, the parent app).
    Watcher,
    /// Reads folders or files into an index.
    Indexer,
    /// Tidies after itself: prunes, sweeps, starts what was left to start.
    Maintenance,
    /// One agent run (not used yet: the runs have their own record).
    AgentRun,
    /// One long tool call (not used yet).
    Tool,
}

impl JobKind {
    pub const ALL: [JobKind; 6] = [JobKind::Scheduler, JobKind::Watcher, JobKind::Indexer, JobKind::Maintenance, JobKind::AgentRun, JobKind::Tool];

    pub fn word(self) -> &'static str {
        match self {
            JobKind::Scheduler => "scheduler",
            JobKind::Watcher => "watcher",
            JobKind::Indexer => "indexer",
            JobKind::Maintenance => "maintenance",
            JobKind::AgentRun => "agent_run",
            JobKind::Tool => "tool",
        }
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// It was told to stop and has not yet.
    Stopping,
    /// It returned by itself.
    Finished,
    /// It returned after it was told to stop.
    Cancelled,
    /// It returned an error (the reason is masked and short).
    Failed(String),
    /// It panicked (the message is masked and short). A supervised job that panics is started again and does not end here until it gives up.
    Panicked(String),
}

impl JobState {
    /// The words of the states, for the gauge and the docs.
    pub const WORDS: [&'static str; 6] = ["running", "stopping", "finished", "cancelled", "failed", "panicked"];

    pub fn word(&self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Stopping => "stopping",
            JobState::Finished => "finished",
            JobState::Cancelled => "cancelled",
            JobState::Failed(_) => "failed",
            JobState::Panicked(_) => "panicked",
        }
    }

    /// The thread has ended.
    pub fn is_over(&self) -> bool {
        !matches!(self, JobState::Running | JobState::Stopping)
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            JobState::Failed(why) | JobState::Panicked(why) => Some(why),
            _ => None,
        }
    }
}

/// Milliseconds since 1970 (wall clock): what the list and the beats are written in.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// What the registry and the job's own thread both hold.
struct Shared {
    cancel: AtomicBool,
    /// The last sign of life, in [`now_ms`]. Starts as the start time.
    beat: AtomicI64,
    /// How many times a supervisor started the body again after a panic.
    restarts: AtomicU32,
    /// When the thread ended, in [`now_ms`]; 0 while it has not.
    ended: AtomicI64,
    state: Mutex<JobState>,
    /// Signalled (with `state` held) when the state becomes one that is over.
    over: Condvar,
    /// A sleeping job waits on `woken` with this held, and a cancel wakes it.
    wake: Mutex<()>,
    woken: Condvar,
}

impl Shared {
    fn new(now: i64) -> Shared {
        Shared {
            cancel: AtomicBool::new(false),
            beat: AtomicI64::new(now),
            restarts: AtomicU32::new(0),
            ended: AtomicI64::new(0),
            state: Mutex::new(JobState::Running),
            over: Condvar::new(),
            wake: Mutex::new(()),
            woken: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the job to stop: sets the flag, says "stopping" and wakes it if it sleeps.
    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        {
            let mut state = self.state();
            if *state == JobState::Running {
                *state = JobState::Stopping;
            }
        }
        // The lock is taken so that a job between "I am not cancelled" and "I wait" cannot miss the wake-up.
        let _held = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        self.woken.notify_all();
    }

    /// The thread has ended: say how, once.
    fn finish(&self, how: JobState) {
        let mut state = self.state();
        if state.is_over() {
            return;
        }
        *state = how;
        self.ended.store(now_ms(), Ordering::SeqCst);
        self.over.notify_all();
    }

    /// Waits until the job is over or `deadline` passes; true when it is over.
    fn wait_over(&self, deadline: Instant) -> bool {
        let mut state = self.state();
        while !state.is_over() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.over.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        true
    }
}

/// What a job holds: it asks whether it was cancelled, shows a sign of life, and sleeps in a way that cancelling cuts short.
#[derive(Clone)]
pub struct JobCtx {
    id: JobId,
    shared: Arc<Shared>,
}

impl JobCtx {
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Has the job been told to stop? A loop looks at this each time round and returns.
    pub fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::SeqCst)
    }

    /// A sign of life: "I am still looking". Cheap; call it where the loop turns if it does not [`sleep`](JobCtx::sleep) there.
    pub fn beat(&self) {
        self.shared.beat.store(now_ms(), Ordering::SeqCst);
    }

    /// Waits `d` (and shows a sign of life on the way in and on the way out). Returns **true** when the whole time passed and the job should go on, **false** when the job was
    /// cancelled, at once (it wakes the moment [`JobRegistry::cancel`] is called, not at the end of the wait): `while ctx.sleep(every) { work(); }`.
    pub fn sleep(&self, d: Duration) -> bool {
        self.beat();
        let end = Instant::now().checked_add(d);
        let mut held = self.shared.wake.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.cancelled() {
                return false;
            }
            // An absurd duration that cannot be added to the clock is an hour at a time, and that is as good as for ever.
            let left = end.map_or(Duration::from_secs(3600), |end| end.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                break;
            }
            held = self.shared.woken.wait_timeout(held, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        drop(held);
        self.beat();
        !self.cancelled()
    }
}

/// One job as the list shows it. Times are milliseconds since 1970.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub id: JobId,
    pub kind: JobKind,
    pub name: String,
    /// `running`, `stopping`, `finished`, `cancelled`, `failed` or `panicked`.
    pub state: &'static str,
    pub started_ms: i64,
    pub last_beat_ms: i64,
    /// When it ended, if it has.
    pub ended_ms: Option<i64>,
    /// What it promised: a sign of life this often (none for a one-shot).
    pub expect_beat_ms: Option<u64>,
    /// No sign of life for five times what it promised, and it has not ended.
    pub stuck: bool,
    /// How often a supervisor started it again after a panic.
    pub restarts: u32,
    /// Why it failed or panicked (masked, one line, at most 300 characters).
    pub error: Option<String>,
}

/// Why [`cancel`] did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelError {
    /// No job has this number (it never had, or it is over and was forgotten).
    Unknown,
    /// The job ended before; here is how.
    AlreadyOver(Box<JobView>),
}

struct Entry {
    id: JobId,
    kind: JobKind,
    name: &'static str,
    expect_beat: Option<Duration>,
    started_ms: i64,
    shared: Arc<Shared>,
    thread: Option<ThreadId>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    /// The (kind, state) pairs the gauge has been set for, so that one that falls to nothing is set to 0 and does not stay at its last value.
    seen: HashSet<(JobKind, &'static str)>,
}

/// The jobs of one process. The core has one ([`global`]); a test makes its own so that it sees only its own jobs.
pub struct JobRegistry {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    /// [`shutdown`](JobRegistry::shutdown) was called: nothing new is started.
    closed: AtomicBool,
    /// Whether the gauge `aqua_jobs` is kept (only the registry of the process).
    publish: bool,
}

impl JobRegistry {
    /// A registry of its own that does not touch the process's metrics.
    pub fn new() -> Arc<JobRegistry> {
        Arc::new(JobRegistry { inner: Mutex::new(Inner::default()), next_id: AtomicU64::new(1), closed: AtomicBool::new(false), publish: false })
    }

    fn for_process() -> Arc<JobRegistry> {
        Arc::new(JobRegistry { inner: Mutex::new(Inner::default()), next_id: AtomicU64::new(1), closed: AtomicBool::new(false), publish: true })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts `f` on a thread named `name` and registers it. `f` gets its [`JobCtx`] and returns `Ok(())` when it is done or an error that is the reason it could not do its work. A
    /// panic in `f` is caught, logged, masked and listed as `panicked`; the thread ends (use [`spawn_supervised`](JobRegistry::spawn_supervised) for a loop that must go on).
    /// `expect_beat` is the interval at which the job promises a sign of life (see the module's description), or `None` for a job that makes no promise. An error here means the
    /// thread could not be made, or the registry is shut down; nothing is registered then.
    pub fn spawn<F>(self: &Arc<Self>, kind: JobKind, name: &'static str, expect_beat: Option<Duration>, f: F) -> std::io::Result<JobId>
    where
        F: FnOnce(JobCtx) -> Result<(), String> + Send + 'static,
    {
        if self.closed.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("Aqua is shutting down, so no new background job is started"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let started_ms = now_ms();
        let shared = Arc::new(Shared::new(started_ms));
        let ctx = JobCtx { id, shared: shared.clone() };
        // The job is in the list before its thread runs: a job that ends at once is still seen to have been there.
        self.lock().entries.push(Entry { id, kind, name, expect_beat, started_ms, shared: shared.clone(), thread: None, handle: None });
        // A shutdown that began between the check above and the entry being in the list did not see this job: it is told to stop at once.
        if self.closed.load(Ordering::SeqCst) {
            shared.request_cancel();
        }
        let registry = self.clone();
        let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || run_job(&registry, kind, name, ctx, f));
        match spawned {
            Ok(handle) => {
                if let Some(entry) = self.lock().entries.iter_mut().find(|e| e.id == id) {
                    entry.handle = Some(handle);
                }
                self.changed();
                Ok(id)
            }
            Err(e) => {
                self.lock().entries.retain(|entry| entry.id != id);
                Err(e)
            }
        }
    }

    /// A loop that is started again when it panics (`supervise`: 1, 2, 4 ... seconds, and given up after five panics in ten minutes). `body` is the whole loop: it reads what it
    /// needs each time it is entered, looks at `ctx.cancelled()`, waits with `ctx.sleep`, and returns when it is cancelled. A job that was given up is `failed`.
    pub fn spawn_supervised<F>(self: &Arc<Self>, kind: JobKind, name: &'static str, expect_beat: Option<Duration>, policy: Backoff, mut body: F) -> std::io::Result<JobId>
    where
        F: FnMut(&JobCtx) + Send + 'static,
    {
        // The record of a panic goes to the crash folder of the data folder, for the registry of the process (a registry a test makes writes nothing).
        let crash_dir = self.publish.then(|| crate::runtime::data_dir().join("crash"));
        self.spawn(kind, name, expect_beat, move |ctx| {
            // The supervisor's waits are the job's sleeps, so that a cancel cuts a wait short; its clock is monotonic, as `Env::real`'s is.
            let wake = ctx.clone();
            let clock = Instant::now();
            let env = crate::supervise::Env::new(move || clock.elapsed(), move |d| {
                let _ = wake.sleep(d);
            }, crash_dir);
            let mut entered = 0u32;
            crate::supervise::supervise(name, policy, &env, || {
                // Told to stop while it waited to be started again: nothing more to do.
                if ctx.cancelled() {
                    return;
                }
                if entered > 0 {
                    ctx.shared.restarts.fetch_add(1, Ordering::SeqCst);
                }
                entered += 1;
                ctx.beat();
                body(&ctx);
            });
            if !ctx.cancelled() && crate::supervise::background_failed().iter().any(|n| n == name) {
                return Err("it panicked too often and was stopped; it stays stopped until Aqua restarts".to_string());
            }
            Ok(())
        })
    }

    fn view_of(entry: &Entry, now: i64) -> JobView {
        let state = entry.shared.state().clone();
        let beat = entry.shared.beat.load(Ordering::SeqCst);
        let ended = entry.shared.ended.load(Ordering::SeqCst);
        let silent = now.saturating_sub(beat).max(0) as u128;
        let stuck = !state.is_over() && entry.expect_beat.is_some_and(|every| silent > every.as_millis() * STUCK_AFTER_BEATS as u128);
        JobView {
            id: entry.id,
            kind: entry.kind,
            name: entry.name.to_string(),
            state: state.word(),
            started_ms: entry.started_ms,
            last_beat_ms: beat,
            ended_ms: (ended > 0).then_some(ended),
            expect_beat_ms: entry.expect_beat.map(|d| d.as_millis() as u64),
            stuck,
            restarts: entry.shared.restarts.load(Ordering::SeqCst),
            error: state.error().map(str::to_string),
        }
    }

    /// Every job, in the order they were started, judged as of `now` (milliseconds since 1970). A test gives a time of its own.
    pub fn list_at(&self, now: i64) -> Vec<JobView> {
        self.lock().entries.iter().map(|e| Self::view_of(e, now)).collect()
    }

    pub fn list(&self) -> Vec<JobView> {
        self.list_at(now_ms())
    }

    /// The jobs that are stuck.
    pub fn stuck(&self) -> Vec<JobView> {
        self.list().into_iter().filter(|j| j.stuck).collect()
    }

    /// Tells a job to stop (see [`JobCtx::cancelled`]). It answers at once with the job as it was when it was told (`stopping`: a job that wakes and ends in the next instant is not
    /// reported as over here); the job ends when it next looks. Nothing is killed.
    pub fn cancel(&self, id: JobId) -> Result<JobView, CancelError> {
        let now = now_ms();
        let (shared, mut view) = {
            let inner = self.lock();
            let Some(entry) = inner.entries.iter().find(|e| e.id == id) else { return Err(CancelError::Unknown) };
            if entry.shared.state().is_over() {
                return Err(CancelError::AlreadyOver(Box::new(Self::view_of(entry, now))));
            }
            (entry.shared.clone(), Self::view_of(entry, now))
        };
        shared.request_cancel();
        view.state = "stopping";
        self.changed();
        Ok(view)
    }

    /// Stops everything: cancels every job that is going (except the one this is called from), waits for all of them together up to `grace`, and returns the names of those that
    /// were still going (a job in a long database or network call cannot be interrupted: it is named, not waited for). After this no job is started. Calling it again waits again.
    pub fn shutdown(self: &Arc<Self>, grace: Duration) -> Vec<String> {
        self.closed.store(true, Ordering::SeqCst);
        let me = std::thread::current().id();
        let targets: Vec<(&'static str, Arc<Shared>)> = self.lock().entries.iter().filter(|e| !e.shared.state().is_over() && e.thread != Some(me)).map(|e| (e.name, e.shared.clone())).collect();
        for (_, shared) in &targets {
            shared.request_cancel();
        }
        let deadline = Instant::now() + grace;
        for (_, shared) in &targets {
            shared.wait_over(deadline);
        }
        let stragglers: Vec<String> = targets.iter().filter(|(_, shared)| !shared.state().is_over()).map(|(name, _)| (*name).to_string()).collect();
        if !targets.is_empty() {
            if stragglers.is_empty() {
                info!("{} background job(s) stopped", targets.len());
            } else {
                warn!("{} background job(s) were told to stop; still going after {} ms: {}", targets.len(), grace.as_millis(), stragglers.join(", "));
            }
        }
        self.changed();
        stragglers
    }

    /// After any change: forgets the oldest jobs that are over beyond [`KEEP_OVER`], and sets the gauge.
    fn changed(&self) {
        let mut forgotten: Vec<JoinHandle<()>> = Vec::new();
        let counts = {
            let mut inner = self.lock();
            let over: Vec<(i64, JobId)> = inner.entries.iter().filter(|e| e.shared.state().is_over()).map(|e| (e.shared.ended.load(Ordering::SeqCst), e.id)).collect();
            if over.len() > KEEP_OVER {
                let mut order = over;
                order.sort();
                let drop_ids: HashSet<JobId> = order.iter().take(order.len() - KEEP_OVER).map(|(_, id)| *id).collect();
                inner.entries.retain_mut(|e| {
                    if drop_ids.contains(&e.id) {
                        forgotten.extend(e.handle.take());
                        false
                    } else {
                        true
                    }
                });
            }
            let mut counts: BTreeMap<(JobKind, &'static str), usize> = BTreeMap::new();
            for e in &inner.entries {
                *counts.entry((e.kind, e.shared.state().word())).or_insert(0) += 1;
            }
            if self.publish {
                let stale: Vec<(JobKind, &'static str)> = inner.seen.iter().filter(|k| !counts.contains_key(*k)).copied().collect();
                inner.seen.extend(counts.keys().copied());
                Some((counts, stale))
            } else {
                None
            }
        };
        // A thread that has ended is reaped here, outside the lock.
        for handle in forgotten {
            let _ = handle.join();
        }
        if let Some((counts, stale)) = counts {
            crate::metrics::describe("aqua_jobs", "Background jobs the core has started, by kind and state (running, stopping, finished, cancelled, failed, panicked).");
            for ((kind, state), n) in counts {
                crate::metrics::gauge("aqua_jobs", &[("kind", kind.word()), ("state", state)], n as f64);
            }
            for (kind, state) in stale {
                crate::metrics::gauge("aqua_jobs", &[("kind", kind.word()), ("state", state)], 0.0);
            }
        }
    }
}

/// Says the job is over if its thread leaves by a way nothing else caught, so that a job is never listed as running when its thread is gone.
struct EndGuard<'a> {
    shared: &'a Shared,
    registry: &'a Arc<JobRegistry>,
}

impl Drop for EndGuard<'_> {
    fn drop(&mut self) {
        self.shared.finish(JobState::Failed("the thread ended without saying how".to_string()));
        self.registry.changed();
    }
}

/// The body of a job's thread: runs `f`, and records how it ended.
fn run_job<F>(registry: &Arc<JobRegistry>, kind: JobKind, name: &'static str, ctx: JobCtx, f: F)
where
    F: FnOnce(JobCtx) -> Result<(), String>,
{
    let guard = EndGuard { shared: &ctx.shared, registry };
    // The job says which thread it is before it does anything: a job that calls `shutdown` must find itself and not wait for itself.
    if let Some(entry) = registry.lock().entries.iter_mut().find(|e| e.id == ctx.id) {
        entry.thread = Some(std::thread::current().id());
    }
    debug!(job = name, id = ctx.id, kind = kind.word(), "background job started");
    let outcome = catch_unwind(AssertUnwindSafe(|| f(ctx.clone())));
    let how = match outcome {
        Ok(Ok(())) => {
            debug!(job = name, id = ctx.id, "background job ended");
            if ctx.cancelled() {
                JobState::Cancelled
            } else {
                JobState::Finished
            }
        }
        Ok(Err(why)) => {
            let why = masked(&why);
            warn!(job = name, id = ctx.id, reason = %why, "background job failed");
            JobState::Failed(why)
        }
        Err(payload) => {
            let why = masked(&payload_text(&*payload));
            error!(job = name, id = ctx.id, payload = %why, "background job panicked and has ended");
            // What a panic carried may itself panic when it is dropped: that must not keep the job from being listed as over.
            let _ = catch_unwind(AssertUnwindSafe(move || drop(payload)));
            JobState::Panicked(why)
        }
    };
    ctx.shared.finish(how);
    registry.changed();
    std::mem::forget(guard);
}

static GLOBAL: OnceLock<Arc<JobRegistry>> = OnceLock::new();

/// The registry of this process.
pub fn global() -> &'static Arc<JobRegistry> {
    GLOBAL.get_or_init(JobRegistry::for_process)
}

/// [`JobRegistry::spawn`] on the registry of this process.
pub fn spawn<F>(kind: JobKind, name: &'static str, expect_beat: Option<Duration>, f: F) -> std::io::Result<JobId>
where
    F: FnOnce(JobCtx) -> Result<(), String> + Send + 'static,
{
    global().spawn(kind, name, expect_beat, f)
}

/// [`JobRegistry::spawn_supervised`] on the registry of this process.
pub fn spawn_supervised<F>(kind: JobKind, name: &'static str, expect_beat: Option<Duration>, policy: Backoff, body: F) -> std::io::Result<JobId>
where
    F: FnMut(&JobCtx) + Send + 'static,
{
    global().spawn_supervised(kind, name, expect_beat, policy, body)
}

/// Every job of this process (see [`JobView`]).
pub fn list() -> Vec<JobView> {
    global().list()
}

/// The jobs of this process that are stuck.
pub fn stuck() -> Vec<JobView> {
    global().stuck()
}

/// [`JobRegistry::cancel`] on the registry of this process.
pub fn cancel(id: JobId) -> Result<JobView, CancelError> {
    global().cancel(id)
}

/// [`JobRegistry::shutdown`] on the registry of this process: the names of the jobs still going after `grace`.
pub fn shutdown(grace: Duration) -> Vec<String> {
    global().shutdown(grace)
}

// ---- what the doctor reads --------------------------------------------------------------------------------------

/// A job in the record for the doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedJob {
    pub name: String,
    pub kind: String,
    pub state: String,
    /// How long it has shown no sign of life.
    pub silent_ms: i64,
    pub expect_beat_ms: Option<u64>,
    pub error: Option<String>,
}

/// `data/crash/jobs.json`: the jobs that are stuck or that ended in an error, as of `at_unix_ms`. The doctor runs in a process of its own and cannot see the core's memory, so the
/// core's monitor writes this while there is something to say, and removes it when there is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub at_unix_ms: i64,
    pub jobs: Vec<RecordedJob>,
}

fn record_path(data_dir: &Path) -> PathBuf {
    data_dir.join("crash").join("jobs.json")
}

/// What the record holds for `jobs` as of `now`: the stuck ones and the ones that ended by an error or a panic (a supervised job that was given up is already in the record of
/// `supervise`, so it is left out here).
pub fn record_for(jobs: &[JobView], now: i64) -> Record {
    let given_up = crate::supervise::background_failed();
    let jobs = jobs
        .iter()
        .filter(|j| j.stuck || (matches!(j.state, "failed" | "panicked") && !given_up.contains(&j.name)))
        .map(|j| RecordedJob { name: j.name.clone(), kind: j.kind.word().to_string(), state: j.state.to_string(), silent_ms: now.saturating_sub(j.last_beat_ms).max(0), expect_beat_ms: j.expect_beat_ms, error: j.error.clone() })
        .collect();
    Record { at_unix_ms: now, jobs }
}

/// Writes the record, or removes the file when there is nothing to say.
pub fn write_record(data_dir: &Path, record: &Record) {
    let path = record_path(data_dir);
    if record.jobs.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let written = serde_json::to_vec_pretty(record).map_err(std::io::Error::other).and_then(|bytes| aqua_tool_host::fsutil::write_atomic_durable(&path, &bytes));
    if let Err(e) = written {
        warn!("the record of stuck jobs {} could not be written: {e}", path.display());
    }
}

/// The record the core last wrote, if there is one that can be read.
pub fn read_record(data_dir: &Path) -> Option<Record> {
    serde_json::from_str(&std::fs::read_to_string(record_path(data_dir)).ok()?).ok()
}

/// At start-up and at a clean end: what an earlier run said is not true of this one.
pub fn forget_record(data_dir: &Path) {
    let _ = std::fs::remove_file(record_path(data_dir));
}

/// Starts the monitor (once per call): every 30 seconds it looks at the list, says in the log (once per job) when one has become stuck, and keeps the record for the doctor. It is
/// a job of its own, supervised, and it promises a sign of life every 30 seconds.
pub fn start_monitor(data_dir: PathBuf) -> std::io::Result<JobId> {
    let mut told: HashSet<JobId> = HashSet::new();
    spawn_supervised(JobKind::Maintenance, "aqua-jobs-monitor", Some(MONITOR_EVERY), Backoff::STANDARD, move |ctx| {
        while ctx.sleep(MONITOR_EVERY) {
            let now = now_ms();
            let jobs = global().list_at(now);
            for j in jobs.iter().filter(|j| j.stuck) {
                if told.insert(j.id) {
                    warn!(job = %j.name, id = j.id, silent_s = now.saturating_sub(j.last_beat_ms) / 1000, "background job shows no sign of life: it may be stuck");
                }
            }
            write_record(&data_dir, &record_for(&jobs, now));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A job that sits until it is cancelled.
    fn waits_for_cancel(ctx: JobCtx) -> Result<(), String> {
        while ctx.sleep(Duration::from_secs(60)) {}
        Ok(())
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < end, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn state_of(reg: &JobRegistry, id: JobId) -> &'static str {
        reg.list().into_iter().find(|j| j.id == id).map(|j| j.state).unwrap_or("forgotten")
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_jobs_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_job_is_listed_while_it_runs_with_its_name_kind_and_promise_and_as_finished_after() {
        let reg = JobRegistry::new();
        let (release, held) = mpsc::channel::<()>();
        let id = reg.spawn(JobKind::Maintenance, "aqua-test-listed", Some(Duration::from_secs(7)), move |_ctx| {
            held.recv().ok();
            Ok(())
        })
        .unwrap();
        let running = reg.list();
        assert_eq!(running.len(), 1);
        let job = &running[0];
        assert_eq!((job.id, job.kind, job.name.as_str(), job.state, job.expect_beat_ms, job.stuck, job.restarts), (id, JobKind::Maintenance, "aqua-test-listed", "running", Some(7000), false, 0));
        assert!(job.started_ms > 0 && job.last_beat_ms >= job.started_ms && job.ended_ms.is_none() && job.error.is_none());
        release.send(()).unwrap();
        wait_for("the job to finish", || state_of(&reg, id) == "finished");
        let done = &reg.list()[0];
        assert!(done.ended_ms.is_some_and(|e| e >= done.started_ms) && !done.stuck, "{done:?}");
        // The thread had the name it was given.
        let named = Arc::new(Mutex::new(String::new()));
        let n = named.clone();
        let id2 = reg.spawn(JobKind::Tool, "aqua-test-thread-name", None, move |_| {
            *n.lock().unwrap() = std::thread::current().name().unwrap_or("").to_string();
            Ok(())
        })
        .unwrap();
        wait_for("the second job", || state_of(&reg, id2) == "finished");
        assert_eq!(*named.lock().unwrap(), "aqua-test-thread-name");
        assert_eq!(reg.list().iter().map(|j| j.id).collect::<Vec<_>>(), vec![id, id2], "in the order they were started");
    }

    #[test]
    fn a_panic_is_caught_masked_and_listed_and_an_error_is_listed_as_failed() {
        let reg = JobRegistry::new();
        let panicked = reg.spawn(JobKind::Watcher, "aqua-test-panics", None, |_| -> Result<(), String> { panic!("the loop broke with password=hunter2secret99 in it") }).unwrap();
        let failed = reg.spawn(JobKind::Indexer, "aqua-test-fails", None, |_| Err(format!("the folder is gone, key={} and more", "x".repeat(900)))).unwrap();
        wait_for("both jobs to end", || state_of(&reg, panicked) == "panicked" && state_of(&reg, failed) == "failed");
        let list = reg.list();
        let p = list.iter().find(|j| j.id == panicked).unwrap();
        let why = p.error.as_deref().unwrap();
        assert!(why.contains("the loop broke") && !why.contains("hunter2secret99"), "masked: {why}");
        let f = list.iter().find(|j| j.id == failed).unwrap();
        let why = f.error.as_deref().unwrap();
        assert!(why.starts_with("the folder is gone") && why.chars().count() <= 303, "one short line: {} characters", why.chars().count());
        // A payload that is not text, and one whose drop panics, still end the job as panicked.
        struct PanicsWhenDropped;
        impl Drop for PanicsWhenDropped {
            fn drop(&mut self) {
                panic!("a panic inside Drop");
            }
        }
        let odd = reg.spawn(JobKind::Tool, "aqua-test-odd-payload", None, |_| -> Result<(), String> { std::panic::panic_any(PanicsWhenDropped) }).unwrap();
        let number = reg.spawn(JobKind::Tool, "aqua-test-number-payload", None, |_| -> Result<(), String> { std::panic::panic_any(42u32) }).unwrap();
        wait_for("the odd jobs", || state_of(&reg, odd) == "panicked" && state_of(&reg, number) == "panicked");
        assert!(reg.list().iter().find(|j| j.id == number).unwrap().error.as_deref().unwrap().contains("not text"));
        assert_eq!(reg.shutdown(Duration::from_millis(100)), Vec::<String>::new(), "nothing is left going");
    }

    #[test]
    fn cancelling_stops_a_cooperative_job_and_a_sleep_wakes_at_once() {
        let reg = JobRegistry::new();
        let (slept_tx, slept_rx) = mpsc::channel::<(bool, Instant)>();
        let id = reg.spawn(JobKind::Scheduler, "aqua-test-sleeper", Some(Duration::from_secs(60)), move |ctx| {
            let went_on = ctx.sleep(Duration::from_secs(3600));
            let _ = slept_tx.send((went_on, Instant::now()));
            Ok(())
        })
        .unwrap();
        // Give the job time to be inside its sleep.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(state_of(&reg, id), "running");
        let told = Instant::now();
        let view = reg.cancel(id).unwrap();
        assert_eq!(view.state, "stopping", "the answer is at once, with the job as it is then");
        let (went_on, woke) = slept_rx.recv_timeout(Duration::from_secs(5)).expect("the sleep returned");
        assert!(!went_on, "a cancelled sleep says the job should stop");
        assert!(woke.duration_since(told) < Duration::from_millis(500), "an hour-long sleep woke {} ms after the cancel (it is a condition variable, not a poll)", woke.duration_since(told).as_millis());
        wait_for("the job to be over", || state_of(&reg, id) == "cancelled");
        assert_eq!(reg.cancel(id), Err(CancelError::AlreadyOver(Box::new(reg.list()[0].clone()))), "a job that is over cannot be cancelled again");
        assert_eq!(reg.cancel(9999), Err(CancelError::Unknown));
        // A sleep that is cancelled before it starts returns false at once, and one that is not cancelled returns true after the time.
        let (tx, rx) = mpsc::channel();
        let id = reg.spawn(JobKind::Scheduler, "aqua-test-sleep-twice", None, move |ctx| {
            let t = Instant::now();
            let first = ctx.sleep(Duration::from_millis(60));
            let spent = t.elapsed();
            while !ctx.cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = tx.send((first, spent, ctx.sleep(Duration::from_secs(3600))));
            Ok(())
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        reg.cancel(id).unwrap();
        let (first, spent, after) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first && spent >= Duration::from_millis(60), "an undisturbed sleep takes its time: {spent:?}");
        assert!(!after);
    }

    #[test]
    fn shutdown_stops_what_it_can_and_names_what_it_cannot() {
        let reg = JobRegistry::new();
        let a = reg.spawn(JobKind::Scheduler, "aqua-test-good-a", Some(Duration::from_secs(1)), waits_for_cancel).unwrap();
        let b = reg.spawn(JobKind::Watcher, "aqua-test-good-b", None, waits_for_cancel).unwrap();
        // This one is inside a call that does not look at the flag, as a job in a long database or network call is.
        let (release, held) = mpsc::channel::<()>();
        let stuck_in_a_call = reg.spawn(JobKind::Indexer, "aqua-test-in-a-call", None, move |_| {
            held.recv().ok();
            Ok(())
        })
        .unwrap();
        let t = Instant::now();
        let left = reg.shutdown(Duration::from_millis(300));
        assert_eq!(left, vec!["aqua-test-in-a-call".to_string()], "the straggler is named");
        assert!(t.elapsed() < Duration::from_secs(3), "all the jobs are waited for together, not one grace each: {:?}", t.elapsed());
        assert_eq!((state_of(&reg, a), state_of(&reg, b)), ("cancelled", "cancelled"));
        assert_eq!(state_of(&reg, stuck_in_a_call), "stopping", "it was told, and has not ended");
        // After a shutdown nothing new is started.
        let refused = reg.spawn(JobKind::Tool, "aqua-test-too-late", None, |_| Ok(()));
        assert!(refused.is_err() && reg.list().len() == 3, "{refused:?}");
        // When the call returns, the job ends as cancelled.
        release.send(()).unwrap();
        wait_for("the straggler to end", || state_of(&reg, stuck_in_a_call) == "cancelled");
        assert!(reg.shutdown(Duration::from_millis(50)).is_empty(), "a second call finds nothing going");
    }

    #[test]
    fn a_job_that_calls_shutdown_does_not_wait_for_itself() {
        let reg = JobRegistry::new();
        let other = reg.spawn(JobKind::Watcher, "aqua-test-other", None, waits_for_cancel).unwrap();
        let (tx, rx) = mpsc::channel();
        let r = reg.clone();
        reg.spawn(JobKind::Watcher, "aqua-test-parent-watch", None, move |_| {
            let t = Instant::now();
            let left = r.shutdown(Duration::from_secs(5));
            let _ = tx.send((left, t.elapsed()));
            Ok(())
        })
        .unwrap();
        let (left, took) = rx.recv_timeout(Duration::from_secs(10)).expect("shutdown from inside a job returned");
        assert!(left.is_empty(), "{left:?}");
        assert!(took < Duration::from_secs(4), "it did not wait out its own grace: {took:?}");
        assert_eq!(state_of(&reg, other), "cancelled");
    }

    #[test]
    fn a_job_is_stuck_after_five_times_what_it_promised_and_never_without_a_promise_or_after_it_ended() {
        let reg = JobRegistry::new();
        // Both sit in a call that never beats, until the test lets them go.
        let (release, held) = mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        let (h1, h2) = (held.clone(), held.clone());
        let promised = reg.spawn(JobKind::Scheduler, "aqua-test-promises", Some(Duration::from_secs(2)), move |_| {
            h1.lock().unwrap().recv().ok();
            Ok(())
        })
        .unwrap();
        let silent = reg.spawn(JobKind::Maintenance, "aqua-test-no-promise", None, move |_| {
            h2.lock().unwrap().recv().ok();
            Ok(())
        })
        .unwrap();
        let start = reg.list()[0].last_beat_ms;
        let stuck_of = |at: i64, id: JobId| reg.list_at(at).into_iter().find(|j| j.id == id).unwrap().stuck;
        assert!(!stuck_of(start + 10_000, promised), "five times two seconds is not yet over");
        assert!(stuck_of(start + 10_100, promised), "just past five times what it promised");
        assert_eq!(reg.list_at(start + 10_100).iter().filter(|j| j.stuck).count(), 1);
        assert!(!stuck_of(start + 3_600_000, silent), "no promise, never stuck");
        release.send(()).unwrap();
        release.send(()).unwrap();
        wait_for("both to end", || state_of(&reg, promised) == "finished" && state_of(&reg, silent) == "finished");
        assert!(!stuck_of(start + 3_600_000, promised), "a job that ended is not stuck");
        assert!(reg.stuck().is_empty());
        // A beat moves the deadline.
        let (release, held) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let beating = reg.spawn(JobKind::Watcher, "aqua-test-beats", Some(Duration::from_secs(1)), move |ctx| {
            ctx.beat();
            let _ = ready_tx.send(());
            held.recv().ok();
            Ok(())
        })
        .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let beat = reg.list().into_iter().find(|j| j.id == beating).unwrap().last_beat_ms;
        assert!(!stuck_of(beat + 5_000, beating) && stuck_of(beat + 5_100, beating));
        release.send(()).unwrap();
    }

    #[test]
    fn a_supervised_loop_is_started_again_after_a_panic_counted_and_cancelled_even_while_it_waits_to_start() {
        let reg = JobRegistry::new();
        let entries = Arc::new(AtomicU32::new(0));
        let e = entries.clone();
        let quick = Backoff { first: Duration::from_millis(5), max: Duration::from_millis(20) };
        let id = reg.spawn_supervised(JobKind::Scheduler, "aqua-test-supervised-loop", Some(Duration::from_secs(1)), quick, move |ctx| {
            if e.fetch_add(1, Ordering::SeqCst) < 2 {
                panic!("the first two rounds break");
            }
            while ctx.sleep(Duration::from_secs(3600)) {}
        })
        .unwrap();
        wait_for("the third entry", || entries.load(Ordering::SeqCst) == 3);
        let job = reg.list().into_iter().find(|j| j.id == id).unwrap();
        assert_eq!((job.state, job.restarts), ("running", 2), "{job:?}");
        reg.cancel(id).unwrap();
        wait_for("the loop to end", || state_of(&reg, id) == "cancelled");
        // A body that never works is given up after five panics, and the job is failed (and named in the supervisor's list).
        let never = reg.spawn_supervised(JobKind::Scheduler, "aqua-test-given-up", None, Backoff { first: Duration::from_millis(1), max: Duration::from_millis(2) }, |_| panic!("always")).unwrap();
        wait_for("the job to be given up", || state_of(&reg, never) == "failed");
        let job = reg.list().into_iter().find(|j| j.id == never).unwrap();
        assert!(job.error.as_deref().unwrap().contains("stopped") && job.restarts == 4, "{job:?}");
        assert!(crate::supervise::background_failed().iter().any(|n| n == "aqua-test-given-up"));
        // Told to stop while the supervisor waits out its backoff: it does not wait the backoff out.
        let slow = Backoff { first: Duration::from_secs(30), max: Duration::from_secs(60) };
        let waiting = reg.spawn_supervised(JobKind::Scheduler, "aqua-test-backoff-cancel", None, slow, |_| panic!("once")).unwrap();
        wait_for("the first panic", || reg.list().iter().any(|j| j.id == waiting && j.state == "running"));
        std::thread::sleep(Duration::from_millis(150));
        let t = Instant::now();
        assert!(reg.shutdown(Duration::from_secs(5)).is_empty());
        assert!(t.elapsed() < Duration::from_secs(3), "shutdown did not wait for a 30 second backoff: {:?}", t.elapsed());
        assert_eq!(state_of(&reg, waiting), "cancelled");
    }

    #[test]
    fn the_registry_keeps_a_hundred_jobs_that_are_over_and_forgets_the_oldest_first() {
        let reg = JobRegistry::new();
        let mut ids = Vec::new();
        for _ in 0..(KEEP_OVER + 15) {
            ids.push(reg.spawn(JobKind::Tool, "aqua-test-many", None, |_| Ok(())).unwrap());
            // Ended times are in milliseconds: the order must be told apart.
            std::thread::sleep(Duration::from_millis(2));
        }
        let still = reg.spawn(JobKind::Watcher, "aqua-test-still-going", None, waits_for_cancel).unwrap();
        wait_for("every short job to end", || reg.list().iter().filter(|j| j.state == "finished").count() >= KEEP_OVER);
        let list = reg.list();
        assert!(list.iter().filter(|j| j.state == "finished").count() <= KEEP_OVER, "{}", list.len());
        assert!(list.iter().any(|j| j.id == still), "a job that is going is never forgotten");
        assert!(!list.iter().any(|j| j.id == ids[0]) && list.iter().any(|j| j.id == *ids.last().unwrap()), "the oldest is forgotten, the newest stays");
        reg.shutdown(Duration::from_secs(2));
    }

    #[test]
    fn two_jobs_may_share_a_name_and_have_numbers_of_their_own() {
        let reg = JobRegistry::new();
        let first = reg.spawn(JobKind::Maintenance, "aqua-test-same-name", None, |_| Ok(())).unwrap();
        wait_for("the first", || state_of(&reg, first) == "finished");
        let again = reg.spawn(JobKind::Maintenance, "aqua-test-same-name", None, |_| Err("the second one failed".into())).unwrap();
        wait_for("the second", || state_of(&reg, again) == "failed");
        assert_ne!(first, again);
        assert_eq!(reg.list().iter().map(|j| (j.id, j.state)).collect::<Vec<_>>(), vec![(first, "finished"), (again, "failed")]);
    }

    #[test]
    fn the_gauge_counts_the_jobs_of_the_process_by_kind_and_state_and_falls_back_to_nothing() {
        // The registry of the process is shared by every test of the binary that uses it: this one is the only user of the kind `tool`.
        let before = crate::metrics::render();
        assert!(!before.contains("aqua_jobs{kind=\"tool\",state=\"running\"} 1"), "{before}");
        let (release, held) = mpsc::channel::<()>();
        let id = spawn(JobKind::Tool, "aqua-test-gauge", None, move |_| {
            held.recv().ok();
            Ok(())
        })
        .unwrap();
        let text = crate::metrics::render();
        assert!(text.contains("# TYPE aqua_jobs gauge") && text.contains("aqua_jobs{kind=\"tool\",state=\"running\"} 1\n"), "{text}");
        release.send(()).unwrap();
        wait_for("the job to finish", || list().iter().any(|j| j.id == id && j.state == "finished"));
        let text = crate::metrics::render();
        assert!(text.contains("aqua_jobs{kind=\"tool\",state=\"running\"} 0\n") && text.contains("aqua_jobs{kind=\"tool\",state=\"finished\"} 1\n"), "{text}");
        // The name and the kind are all a label can carry.
        assert!(!text.contains("aqua-test-gauge"));
    }

    #[test]
    fn the_record_for_the_doctor_holds_the_stuck_and_the_failed_and_is_written_read_and_removed() {
        let data = temp("record");
        let now = 1_800_000_000_000i64;
        let view = |name: &str, state: &'static str, silent: i64, stuck: bool, error: Option<&str>| JobView {
            id: 1,
            kind: JobKind::Scheduler,
            name: name.into(),
            state,
            started_ms: now - 1_000_000,
            last_beat_ms: now - silent,
            ended_ms: None,
            expect_beat_ms: Some(10_000),
            stuck,
            restarts: 0,
            error: error.map(str::to_string),
        };
        let jobs = vec![view("aqua-fine", "running", 5_000, false, None), view("aqua-stuck", "running", 120_000, true, None), view("aqua-failed", "failed", 1, false, Some("the folder is gone")), view("aqua-done", "finished", 1, false, None)];
        let record = record_for(&jobs, now);
        assert_eq!(record.jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(), vec!["aqua-stuck", "aqua-failed"]);
        assert_eq!(record.jobs[0].silent_ms, 120_000);
        assert!(read_record(&data).is_none());
        write_record(&data, &record);
        assert_eq!(read_record(&data), Some(record.clone()));
        // Nothing to say: the file goes.
        write_record(&data, &record_for(&jobs[..1], now));
        assert!(read_record(&data).is_none() && !record_path(&data).exists());
        write_record(&data, &record);
        forget_record(&data);
        assert!(read_record(&data).is_none());
        // A file that is not JSON reads as nothing.
        std::fs::create_dir_all(data.join("crash")).unwrap();
        std::fs::write(record_path(&data), "{\"at_unix").unwrap();
        assert!(read_record(&data).is_none());
        let _ = std::fs::remove_dir_all(&data);
    }
}
