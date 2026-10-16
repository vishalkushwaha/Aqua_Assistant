//! Keeping the history of flows from growing without end: finished runs and their logs, read notifications, and the record of
//! scheduled times. Once an hour the oldest are forgotten. What is still going, waiting for a person, or not yet seen is never
//! touched.

use super::notify::NotifyStore;
use super::run::store::RunStore;
use super::sched::store::SchedStore;
use std::sync::Arc;
use std::time::Duration;

/// Finished runs kept for each flow, and how long a finished run is kept at most.
pub const RUNS_KEPT_PER_FLOW: u32 = 200;
pub const RUN_DAYS: i64 = 90;
/// Read notifications kept for each person, and how long a read one is kept at most.
pub const NOTIFICATIONS_KEPT: u32 = 500;
pub const NOTIFICATION_DAYS: i64 = 30;
/// Decided scheduled times kept for each schedule, and how long one is kept at most.
pub const OCCURRENCES_KEPT_PER_SCHEDULE: u32 = 200;
pub const OCCURRENCE_DAYS: i64 = 90;

/// How much one pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub runs: usize,
    pub notifications: usize,
    pub occurrences: usize,
}

/// One pass. A store that fails is logged and the others still go on.
pub fn prune_once(runs: &RunStore, notifications: &NotifyStore, schedules: &SchedStore, now: i64) -> Pruned {
    const DAY: i64 = 86_400;
    fn count(what: &str, r: Result<usize, String>) -> usize {
        r.unwrap_or_else(|e| {
            tracing::warn!("flow history ({what}) could not be pruned: {e}");
            0
        })
    }
    Pruned {
        runs: count("runs", runs.prune(RUNS_KEPT_PER_FLOW, now - RUN_DAYS * DAY)),
        notifications: count("notifications", notifications.prune(NOTIFICATIONS_KEPT, now - NOTIFICATION_DAYS * DAY)),
        occurrences: count("scheduled times", schedules.prune(OCCURRENCES_KEPT_PER_SCHEDULE, now - OCCURRENCE_DAYS * DAY)),
    }
}

/// Start the thread that does it every hour (the first time two minutes after start-up, so start-up is not slowed).
pub fn spawn(runs: Arc<RunStore>, notifications: Arc<NotifyStore>, schedules: Arc<SchedStore>) {
    // A job of the registry (`jobs`): it promises a sign of life every hour, and the shutdown cuts its waits short.
    let started = crate::jobs::spawn_supervised(crate::jobs::JobKind::Maintenance, "aqua-flow-prune", Some(Duration::from_secs(3600)), crate::supervise::Backoff::STANDARD, move |job| {
        if !job.sleep(Duration::from_secs(120)) {
            return;
        }
        loop {
            let p = prune_once(&runs, &notifications, &schedules, chrono::Utc::now().timestamp());
            if p != Pruned::default() {
                tracing::info!("flow history pruned: {} run(s), {} notification(s), {} scheduled time(s)", p.runs, p.notifications, p.occurrences);
            }
            if !job.sleep(Duration::from_secs(3600)) {
                return;
            }
        }
    });
    if started.is_err() {
        tracing::warn!("could not start the flow history thread");
    }
}
