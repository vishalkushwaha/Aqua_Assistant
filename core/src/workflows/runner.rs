//! Starts workflows: on their schedule, at start-up, from a webhook, or when a person presses Run.
//!
//! One background thread wakes every few seconds and asks the store which workflows are due. Each
//! run gets its own thread, so a slow workflow never delays another one. Rules that keep it reliable:
//! - **One run at a time per workflow.** If a scheduled run comes due while the previous one is still
//!   going, it is recorded as skipped and the schedule moves on (no pile-up).
//! - **A cap on runs at once** (2). Extra due workflows wait for the next check.
//! - **Missed runs.** Aqua only runs workflows while it is running. A run that came due while it was
//!   closed (more than 5 minutes ago) is skipped and recorded, unless the workflow says `catch_up`,
//!   in which case it runs once.
//! - **Crashes.** A run left "running" by a crash is marked `interrupted` when Aqua starts again.
//! - **Unattended.** Runs are on threads where every approval prompt is refused on the spot; nothing
//!   is shown and nothing is granted. Anything that needs a person's OK fails with a clear message.

use super::engine::{self, real_sleep, Env, Executor, Sleeper};
use super::model::*;
use super::store::{now, Claim, WorkflowStore, R};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// How late a scheduled run may start and still count as on time.
pub const GRACE_SECS: i64 = 300;
const CHECK_EVERY: Duration = Duration::from_secs(10);
const STARTUP_DELAY: Duration = Duration::from_secs(20);
pub const MAX_CONCURRENT: usize = 2;

static STORE: OnceLock<Arc<WorkflowStore>> = OnceLock::new();
static SCHEDULER: OnceLock<Arc<Scheduler>> = OnceLock::new();

/// The process-wide store. `AQUA_WORKFLOWS_DB` moves it (tests use this so they never touch real data).
pub fn store() -> Arc<WorkflowStore> {
    STORE
        .get_or_init(|| {
            let path = std::env::var_os("AQUA_WORKFLOWS_DB").map(PathBuf::from).unwrap_or_else(|| crate::runtime::workspace_root().join("data").join("workflows.db"));
            Arc::new(WorkflowStore::new(path))
        })
        .clone()
}

/// The running scheduler, once `spawn` has been called.
pub fn scheduler() -> Option<Arc<Scheduler>> {
    SCHEDULER.get().cloned()
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStatus {
    pub running: bool,
    pub active_runs: usize,
    pub max_concurrent: usize,
    pub checked_at: i64,
}

pub struct Scheduler {
    store: Arc<WorkflowStore>,
    exec: Arc<dyn Executor>,
    sleep: Arc<Sleeper>,
    cancels: Mutex<HashMap<i64, Arc<AtomicBool>>>,
    active: AtomicUsize,
    max_concurrent: usize,
    last_check: Mutex<i64>,
}

impl Scheduler {
    pub fn new(store: Arc<WorkflowStore>, exec: Arc<dyn Executor>, sleep: Arc<Sleeper>, max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self { store, exec, sleep, cancels: Mutex::new(HashMap::new()), active: AtomicUsize::new(0), max_concurrent, last_check: Mutex::new(0) })
    }

    pub fn status(&self) -> SchedulerStatus {
        SchedulerStatus { running: true, active_runs: self.active.load(Ordering::SeqCst), max_concurrent: self.max_concurrent, checked_at: *self.last_check.lock().unwrap_or_else(PoisonError::into_inner) }
    }

    pub fn is_idle(&self) -> bool {
        self.active.load(Ordering::SeqCst) == 0
    }

    /// Stop a run at its next step (an agent step stops at its next planning round).
    pub fn cancel(&self, run_id: i64) -> bool {
        match self.cancels.lock().unwrap_or_else(PoisonError::into_inner).get(&run_id) {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    fn spawn_run(self: &Arc<Self>, w: Workflow, run_id: i64, kind: String, payload: Value) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancels.lock().unwrap_or_else(PoisonError::into_inner).insert(run_id, cancel.clone());
        self.active.fetch_add(1, Ordering::SeqCst);
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new().name(format!("aqua-workflow-{}", w.id)).spawn({
            let this = Arc::clone(&this);
            let w = w.clone();
            move || {
                // Per thread: every approval prompt is refused at once. Nothing is shown or granted.
                let _unattended = aqua_mcp_adapters::browser::unattended();
                let env = Env { store: &this.store, exec: this.exec.as_ref(), cancel: &cancel, sleep: this.sleep.as_ref() };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine::execute(&env, &w, run_id, &kind, &payload)));
                if outcome.is_err() {
                    tracing::warn!("workflow {} crashed (contained)", w.id);
                    let _ = this.store.finish_run(run_id, RunStatus::Failed, "the run crashed (contained)", now());
                }
                this.cancels.lock().unwrap_or_else(PoisonError::into_inner).remove(&run_id);
                this.active.fetch_sub(1, Ordering::SeqCst);
            }
        });
        if spawned.is_err() {
            this.cancels.lock().unwrap_or_else(PoisonError::into_inner).remove(&run_id);
            this.active.fetch_sub(1, Ordering::SeqCst);
            let _ = this.store.finish_run(run_id, RunStatus::Failed, "could not start a thread for the run", now());
        }
    }

    /// Run a workflow now (a person pressed Run, or a webhook arrived). Returns the run id at once;
    /// the run goes on in the background.
    pub fn start(self: &Arc<Self>, w: &Workflow, kind: &str, payload: Value) -> R<i64> {
        if self.active.load(Ordering::SeqCst) >= self.max_concurrent {
            return Err("too many workflows are running right now; try again in a moment".into());
        }
        match self.store.claim(w, kind, false, now())? {
            Claim::Started(run_id) => {
                self.spawn_run(w.clone(), run_id, kind.to_string(), payload);
                Ok(run_id)
            }
            Claim::Skipped { reason, .. } => Err(reason),
        }
    }

    /// One look at what is due. Returns how many runs were started.
    pub fn tick(self: &Arc<Self>, at: i64) -> usize {
        *self.last_check.lock().unwrap_or_else(PoisonError::into_inner) = at;
        let due = match self.store.due(at) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("workflow scheduler: {e}");
                return 0;
            }
        };
        let mut started = 0;
        for w in due {
            if self.active.load(Ordering::SeqCst) >= self.max_concurrent {
                break; // still due; the next check picks it up
            }
            let due_at = w.next_run_at.unwrap_or(at);
            if at - due_at > GRACE_SECS && !w.catch_up {
                let _ = self.store.record_missed(&w, at, due_at);
                continue;
            }
            match self.store.claim(&w, w.trigger.kind(), true, at) {
                Ok(Claim::Started(run_id)) => {
                    let kind = w.trigger.kind().to_string();
                    self.spawn_run(w, run_id, kind, Value::Null);
                    started += 1;
                }
                Ok(Claim::Skipped { .. }) => {}
                Err(e) => tracing::warn!("workflow scheduler: {e}"),
            }
        }
        started
    }

    /// Run the workflows that are set to run when Aqua starts.
    pub fn run_startup_workflows(self: &Arc<Self>) -> usize {
        let mut started = 0;
        for w in self.store.startup_workflows().unwrap_or_default() {
            if self.active.load(Ordering::SeqCst) >= self.max_concurrent {
                break;
            }
            if let Ok(Claim::Started(run_id)) = self.store.claim(&w, "startup", false, now()) {
                self.spawn_run(w, run_id, "startup".into(), Value::Null);
                started += 1;
            }
        }
        started
    }
}

/// Start the scheduler once. It does nothing until a workflow is enabled.
pub fn spawn(exec: Arc<dyn Executor>) {
    let store = store();
    let scheduler = Scheduler::new(store.clone(), exec, Arc::new(real_sleep), MAX_CONCURRENT);
    if SCHEDULER.set(scheduler.clone()).is_err() {
        return;
    }
    match store.recover_interrupted(now()) {
        Ok(n) if n > 0 => tracing::info!("{n} workflow run(s) were interrupted by the last shutdown"),
        Ok(_) => {}
        Err(e) => tracing::warn!("{e}"),
    }
    // A job of the registry (`jobs`): listed, stopped by the shutdown, and it promises a sign of life every round. Not supervised, as before: a panic ends it, and the registry says so.
    let spawned = crate::jobs::spawn(crate::jobs::JobKind::Scheduler, "aqua-workflows", Some(CHECK_EVERY), move |job| {
        if !job.sleep(STARTUP_DELAY) {
            return Ok(());
        }
        scheduler.run_startup_workflows();
        let mut rounds = 0u64;
        loop {
            scheduler.tick(now());
            rounds += 1;
            if rounds % 360 == 0 {
                // About hourly: keep the history to 200 runs per workflow and 90 days.
                let _ = scheduler.store.prune(200, 90 * 86_400, now());
            }
            if !job.sleep(CHECK_EVERY) {
                return Ok(());
            }
        }
    });
    if spawned.is_err() {
        tracing::warn!("could not start the workflow scheduler thread");
    }
}
