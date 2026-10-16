//! Scheduled execution: schedules that live in the database, a scheduler that looks at them every few
//! seconds, and the rules for what to do about a time that was missed. See `tick`.

pub mod model;
pub mod preflight;
pub mod store;
pub mod tick;

use crate::flows::notify::Notifier;
use crate::flows::run::manager::FlowRunner;
use crate::flows::store::FlowStore;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use store::SchedStore;
use tick::{RunNotifier, Scheduler};

static SCHEDULER: OnceLock<Arc<Scheduler>> = OnceLock::new();

pub fn scheduler() -> Option<Arc<Scheduler>> {
    SCHEDULER.get().cloned()
}

/// How often the scheduler looks.
pub const TICK: Duration = Duration::from_secs(10);

/// Start the scheduler thread and register the notifier that reports how runs end.
pub fn spawn(store: Arc<SchedStore>, runner: Arc<FlowRunner>, flows: Arc<FlowStore>, notifier: Arc<Notifier>) -> Arc<Scheduler> {
    let s = Arc::new(Scheduler { store, runner: runner.clone(), flows, notifier });
    if SCHEDULER.set(s.clone()).is_err() {
        return scheduler().expect("set");
    }
    runner.observe(Arc::new(RunNotifier { scheduler: s.clone() }));
    let this = s.clone();
    // Supervised: a panic in one tick (it also fires the reminders) is logged and the next tick runs, instead of the thread ending for good. A job of the registry (`jobs`): it is
    // listed, and it promises a sign of life every tick.
    let spawned = crate::jobs::spawn_supervised(crate::jobs::JobKind::Scheduler, "aqua-flow-scheduler", Some(TICK), crate::supervise::Backoff::STANDARD, move |job| this.run_job(TICK, &|| chrono::Utc::now().timestamp(), job));
    if spawned.is_err() {
        tracing::warn!("could not start the flow scheduler thread");
    }
    s
}
