//! The scheduler: every few seconds, look at which schedules have come due and deal with each one.
//!
//! What "dealing with it" means, in order:
//! 1. If the previous run of the same schedule is still going (or waiting for you), do not start another
//!    one on top of it: record that and move on.
//! 2. If it is only a little late, start it. If it is very late, the schedule's *misfire policy* decides:
//!    run it now, skip it, or ask.
//! 3. Start the run, record what happened (an occurrence, with the reason in words), work out the next
//!    time, and tell the person as their notification preferences say.
//!
//! Nothing here is kept in memory between ticks: the schedules, occurrences and the heartbeat are in the
//! database, so a restart loses nothing and a missed time can be explained afterwards.

use super::model::*;
use super::store::SchedStore;
use crate::flows::notify::{summarize, NewNotification, Notifier};
use crate::flows::run::manager::{FlowRunner, RunObserver};
use crate::flows::run::model::*;
use crate::flows::store::FlowStore;
use crate::flows::supervisor::VerdictKind;
use crate::flows::validate::validate;
use crate::flows::zone::Zone;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub type R<T> = Result<T, String>;

/// Test hook (compiled in tests only): the next `tick` of the scheduler at this address panics, once. The scenario test of the supervised thread uses it.
#[cfg(test)]
pub(crate) static PANIC_NEXT_TICK_OF: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

/// A run that finishes at once must not announce "completed" before the scheduler has announced "started".
/// The scheduler holds this while it starts a run and posts that notice; the run's own notice waits for it.
static ORDER: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn in_order() -> std::sync::MutexGuard<'static, ()> {
    ORDER.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Scheduler {
    pub store: Arc<SchedStore>,
    pub runner: Arc<FlowRunner>,
    pub flows: Arc<FlowStore>,
    pub notifier: Arc<Notifier>,
}

/// A run that has been quiet this long while still being driven is reported as possibly stuck.
pub const STALL_SECS: i64 = 30 * 60;
/// This many failures in a row, all the same kind, switch a schedule off.
pub const REPEAT_LIMIT: usize = 3;

#[derive(Debug, Default, PartialEq)]
pub struct WatchReport {
    pub orphaned: usize,
    pub stalled: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct TickReport {
    pub started: usize,
    pub skipped: usize,
    pub asked: usize,
    pub failed: usize,
}

/// How a campaign is going, from the record.
#[derive(Debug, Clone, Serialize)]
pub struct CampaignView {
    pub name: String,
    pub runs_started: u32,
    /// How many runs it will have made in all, when that is known.
    pub total_planned: Option<u32>,
    pub remaining: u32,
    pub worked: u32,
    pub recovered: u32,
    pub failed: u32,
    pub unverified: u32,
    /// Started and not finished, or waiting for you.
    pub in_progress: u32,
    pub skipped: u32,
    pub awaiting_decision: u32,
    pub next_run_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub over: bool,
    pub text: String,
}

/// A schedule as a person sees it.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleView {
    #[serde(flatten)]
    pub schedule: Schedule,
    pub state: State,
    pub flow_name: String,
    pub zone_label: String,
    /// The next time, written in the schedule's own zone.
    pub next_run_text: String,
    pub active_run: Option<i64>,
    /// A missed time waiting for a decision.
    pub pending_missed: Option<i64>,
}

fn zone_of(s: &Schedule) -> Zone {
    s.zone().unwrap_or(Zone::Local)
}

impl Scheduler {
    fn payload(s: &Schedule, due: i64) -> Value {
        json!({ "schedule_id": s.id, "due_at": due })
    }

    fn emit(&self, s: &Schedule, kind: &'static str, level: &'static str, run_id: Option<i64>, title: String, body: String, at: i64) {
        let _ = self.notifier.emit(&s.owner_id, &s.notify.channels, NewNotification { kind, level, flow_id: s.flow_id, run_id, schedule_id: Some(s.id), title, body, data: Value::Null }, at);
    }

    /// How a run stands right now, as the word stored on the schedule. A quick run may have finished, and been
    /// reported, before the scheduler gets to write anything; that outcome must not be overwritten with "running".
    fn status_word(&self, owner: &str, run_id: i64, running: &str) -> String {
        match self.runner.store().get_run(owner, run_id) {
            Ok(Some(r)) if r.status == RunStatus::Succeeded => "succeeded".into(),
            Ok(Some(r)) if r.status == RunStatus::Failed => "failed".into(),
            Ok(Some(r)) if r.status.is_finished() => r.status.as_str().into(),
            _ => running.into(),
        }
    }

    /// Start the run for a schedule. `Err` carries the reason in words.
    fn start(&self, s: &Schedule, due: i64) -> R<Run> {
        self.runner.start(&s.owner_id, s.flow_id, s.inputs_for_next_run(), Self::payload(s, due), false, "schedule")
    }

    /// Start the run for a time that has come, by itself: first what can be known without asking anyone is checked (`preflight`: an `Err` that starts with `preflight:` means the run
    /// was not tried), then the run is started under a key of this schedule and this time. Should the scheduler start a run and then fail to write down that it did (the database was
    /// busy), the next tick finds the same time due again: the key gives that run back (`true`) and no second one is made.
    fn start_once(&self, s: &Schedule, due: i64) -> R<(Run, bool)> {
        super::preflight::check(&super::preflight::facts_now()).map_err(|why| format!("{}{why}", super::preflight::PREFIX))?;
        let key = format!("schedule:{}:{}", s.id, due);
        self.runner.start_keyed(&s.owner_id, s.flow_id, s.inputs_for_next_run(), Self::payload(s, due), false, "schedule", Some((key.as_str(), "")))
    }

    /// A time that was started already (see `start_once`): nothing is started or announced again; the schedule only goes on to its next time, as it should have the first time.
    fn already_started(&self, s: &Schedule, run: &Run, next_if_started: Option<i64>, at: i64) {
        tracing::warn!("flow scheduler: the run of schedule {} for this time was started already (run {}); no second run was made", s.id, run.id);
        let word = self.status_word(&s.owner_id, run.id, "running");
        self.advance_retry(s.id, next_if_started, at, Some(run.id), &word);
    }

    fn why_missed(&self, due: i64, at: i64) -> String {
        let z = Zone::Local;
        let gap = self.store.gap_covering(due).ok().flatten();
        match gap {
            Some((a, b)) => format!("Aqua was not running from {} to {}", z.show(a), z.show(b)),
            None => format!("the scheduler was {} minutes late (the computer may have been asleep, or Aqua busy)", (at - due) / 60),
        }
    }

    /// The loop of the scheduler thread: look at what is due, wait `every`, and again, until `stop` says so (in the program it never does). It reads everything it needs from the
    /// database in each round, so the supervisor can enter it again after a panic (see `supervise`).
    pub fn run_until(&self, every: Duration, now: &dyn Fn() -> i64, stop: &dyn Fn() -> bool) {
        while !stop() {
            let at = now();
            self.tick(at);
            self.watchdog(at);
            std::thread::sleep(every);
        }
    }

    /// The same loop as a background job (`jobs`): the wait between two rounds is the job's sleep, which a cancel or the shutdown cuts short, and it counts as a sign of life.
    /// Like [`run_until`](Scheduler::run_until) it reads everything it needs in each round, so the supervisor can enter it again after a panic.
    pub fn run_job(&self, every: Duration, now: &dyn Fn() -> i64, job: &crate::jobs::JobCtx) {
        while !job.cancelled() {
            let at = now();
            self.tick(at);
            self.watchdog(at);
            if !job.sleep(every) {
                break;
            }
        }
    }

    /// Look at everything due at `at`.
    pub fn tick(&self, at: i64) -> TickReport {
        #[cfg(test)]
        {
            let mut target = PANIC_NEXT_TICK_OF.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if *target == Some(self as *const Scheduler as usize) {
                *target = None;
                drop(target);
                panic!("injected panic in tick");
            }
        }
        let mut report = TickReport::default();
        if let Err(e) = self.store.heartbeat(at) {
            tracing::warn!("flow scheduler: {e}");
        }
        let due = match self.store.due(at) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("flow scheduler: {e}");
                return report;
            }
        };
        for s in due {
            let Some(due_at) = s.next_run_at else { continue };
            let zone = zone_of(&s);
            let next = s.next_within_campaign(at, false);
            let next_if_started = s.next_within_campaign(at, true);
            let owner = s.owner_id.clone();
            // 1. Never on top of a run that is still going.
            if let Ok(Some((rid, status))) = self.store.active_run_of(&owner, s.id) {
                let reason = format!("the previous run (#{rid}) is still {status}");
                self.occurrence_logged(&owner, s.id, due_at, at, "skipped", None, &reason);
                self.advance_retry(s.id, next, 0, None, &format!("skipped: {reason}"));
                self.emit(&s, "missed", "warning", Some(rid), format!("Skipped a scheduled run of {}", s.name), format!("It was due {}, but {reason}.", zone.show(due_at)), at);
                report.skipped += 1;
                continue;
            }
            let late = at - due_at;
            if late > GRACE_SECS {
                let _order = in_order();
                let why = self.why_missed(due_at, at);
                match s.misfire {
                    Misfire::Skip | Misfire::NextRun => {
                        let reason = format!("missed: {why}. Skipped as this schedule is set to wait for its next time");
                        self.occurrence_logged(&owner, s.id, due_at, at, "skipped", None, &reason);
                        self.advance_retry(s.id, next, 0, None, "missed (skipped)");
                        self.emit(&s, "missed", "warning", None, format!("Missed a scheduled run of {}", s.name), format!("It was due {}: {why}. It was skipped, and the next run is {}.", zone.show(due_at), next.map(|n| zone.show(n)).unwrap_or_else(|| "none".into())), at);
                        report.skipped += 1;
                    }
                    Misfire::Ask => {
                        let reason = format!("missed: {why}. Waiting for your decision");
                        self.occurrence_logged(&owner, s.id, due_at, at, "missed_pending", None, &reason);
                        self.advance_retry(s.id, next, 0, None, "missed (asking you)");
                        self.emit(&s, "needs_you", "warning", None, format!("Missed run of {}: run it now?", s.name), format!("It was due {}: {why}. Choose to run it now or skip it.", zone.show(due_at)), at);
                        report.asked += 1;
                    }
                    Misfire::RunNow => match self.start_once(&s, due_at) {
                        Ok((run, true)) => self.already_started(&s, &run, next_if_started, at),
                        Ok((run, false)) => {
                            self.occurrence_logged(&owner, s.id, due_at, at, "started_late", Some(run.id), &format!("started late: {why}"));
                            let word = self.status_word(&owner, run.id, "running (late)");
                            self.note_started_logged(s.id);
                            self.advance_retry(s.id, next_if_started, at, Some(run.id), &word);
                            self.emit(&s, "started", "info", Some(run.id), format!("Started {} (late)", s.name), format!("It was due {}: {why}. Started now, as this schedule is set to.", zone.show(due_at)), at);
                            report.started += 1;
                        }
                        Err(e) => self.could_not_start(&s, due_at, at, next, &e, &mut report),
                    },
                }
                continue;
            }
            let _order = in_order();
            match self.start_once(&s, due_at) {
                Ok((run, true)) => self.already_started(&s, &run, next_if_started, at),
                Ok((run, false)) => {
                    self.occurrence_logged(&owner, s.id, due_at, at, "started", Some(run.id), "");
                    let word = self.status_word(&owner, run.id, "running");
                    self.note_started_logged(s.id);
                    self.advance_retry(s.id, next_if_started, at, Some(run.id), &word);
                    if s.notify.on_start {
                        self.emit(&s, "started", "info", Some(run.id), format!("Started {}", s.name), format!("Scheduled for {}; started {}.", zone.show(due_at), zone.show(at)), at);
                    }
                    report.started += 1;
                }
                Err(e) => self.could_not_start(&s, due_at, at, next, &e, &mut report),
            }
        }
        report
    }

    /// Look for runs that are marked running but are not. One with nothing driving it (the driver died) is marked
    /// interrupted, so it does not look busy forever and block its schedule; one still driven but silent for
    /// `STALL_SECS` is reported once as possibly stuck. Neither is guessed at or killed.
    pub fn watchdog(&self, at: i64) -> WatchReport {
        let mut report = WatchReport::default();
        let Ok(running) = self.runner.store().running_runs() else { return report };
        for (id, owner, flow_id, name, last) in running {
            if !self.runner.is_active(id) {
                // Give a driver that is just being started a moment.
                if at - last < 30 {
                    continue;
                }
                let store = self.runner.store();
                if let Ok(nodes) = store.node_runs(id) {
                    for mut n in nodes.into_iter().filter(|n| n.status == NodeStatus::Running) {
                        n.status = NodeStatus::Interrupted;
                        n.error = "nothing was running this step any more".into();
                        let _ = store.save_node(&n);
                    }
                }
                let _ = store.set_status(id, RunStatus::Interrupted, "the run stopped without finishing: nothing was driving it", at);
                let _ = store.add_event(id, None, crate::flows::run::model::Level::Error, "watchdog", "Nothing was driving this run, so it was marked interrupted. You can resume it.", at);
                if !self.notifier.store.exists_for_run(&owner, id, "failed").unwrap_or(true) {
                    let _ = self.notifier.emit(&owner, &["in_app".to_string()], NewNotification { kind: "failed", level: "warning", flow_id, run_id: Some(id), schedule_id: None, title: format!("⚠️ {name} stopped without finishing"), body: "Nothing was driving the run, so it was marked interrupted. Open it to resume from where it stopped.".into(), data: Value::Null }, at);
                }
                report.orphaned += 1;
            } else if at - last >= STALL_SECS && !self.notifier.store.exists_for_run(&owner, id, "stalled").unwrap_or(true) {
                let _ = self.notifier.emit(&owner, &["in_app".to_string()], NewNotification { kind: "stalled", level: "warning", flow_id, run_id: Some(id), schedule_id: None, title: format!("⏳ {name} has been quiet for {} minutes", (at - last) / 60), body: "A step has not reported anything for a long time. It may be stuck. Open the run to see which step, then stop or retry it.".into(), data: Value::Null }, at);
                report.stalled += 1;
            }
        }
        report
    }

    /// After a scheduled run fails, look at the last few. If they all failed the same way, another try tomorrow
    /// will not help and may do harm (a locked account, a flood of failed posts): switch the schedule off and say why.
    fn guard_repeated(&self, s: &Schedule, at: i64) {
        let Ok(recent) = self.runner.store().list_runs(&s.owner_id, Some(s.flow_id), 12) else { return };
        let mut classes = Vec::new();
        for r in recent.into_iter().filter(|r| r.trigger_kind == "schedule" && r.status.is_finished() && !r.dry_run) {
            let Ok(Some(full)) = self.runner.store().get_run(&s.owner_id, r.id) else { break };
            let nodes = self.runner.store().node_runs(r.id).unwrap_or_default();
            let v = crate::flows::supervisor::judge(&full, &nodes, &full.def);
            if v.kind != VerdictKind::Failed {
                break;
            }
            classes.push(v.class.unwrap_or(crate::flows::supervisor::ErrorClass::Unknown));
            if classes.len() == REPEAT_LIMIT {
                break;
            }
        }
        if classes.len() < REPEAT_LIMIT || !classes.iter().all(|c| *c == classes[0]) {
            return;
        }
        let _ = self.store.set_enabled(&s.owner_id, s.id, false, at);
        let _ = self.store.set_last_status(s.id, s.last_run_id.unwrap_or(0), "failed: switched off after repeated failures");
        let c = classes[0];
        self.emit(s, "failed", "error", s.last_run_id, format!("🛑 {} was switched off after {} failures in a row", s.name, REPEAT_LIMIT), format!("The last {REPEAT_LIMIT} runs all failed the same way: {} ({}). Running it again would not help, so the schedule was stopped to be safe. Nothing else was changed. Fix the cause, then switch the schedule back on.", c.label(), c.explain()), at);
    }

    /// Move a schedule to its next time. A failure here is not ignorable: if `next_run_at` stays in the past the same
    /// schedule is due again on the next tick and would start a second run, so the write is retried and a lasting
    /// failure is logged loudly.
    fn advance_retry(&self, id: i64, next: Option<i64>, ran_at: i64, run_id: Option<i64>, status: &str) {
        for attempt in 0..4 {
            match self.store.advance(id, next, ran_at, run_id, status) {
                Ok(()) => return,
                Err(e) if attempt == 3 => tracing::error!("flow scheduler: could not move schedule {id} to its next time ({e}); it may start again on the next tick"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100 << attempt)),
            }
        }
    }

    fn occurrence_logged(&self, owner: &str, schedule_id: i64, due_at: i64, at: i64, outcome: &str, run_id: Option<i64>, reason: &str) {
        if let Err(e) = self.store.add_occurrence(owner, schedule_id, due_at, at, outcome, run_id, reason) {
            tracing::warn!("flow scheduler: could not record the {outcome} occurrence of schedule {schedule_id}: {e}");
        }
    }

    fn note_started_logged(&self, id: i64) {
        if let Err(e) = self.store.note_started(id) {
            tracing::warn!("flow scheduler: could not count the start of schedule {id}: {e}");
        }
    }

    fn could_not_start(&self, s: &Schedule, due_at: i64, at: i64, next: Option<i64>, error: &str, report: &mut TickReport) {
        let zone = zone_of(s);
        // The preflight said not now: the run was not tried, so it did not fail. It is a skipped time, said as one, and the schedule goes on to its next time.
        if let Some(why) = error.strip_prefix(super::preflight::PREFIX) {
            let reason = format!("not started: {why}");
            self.occurrence_logged(&s.owner_id, s.id, due_at, at, "skipped", None, &reason);
            self.advance_retry(s.id, next, 0, None, &format!("skipped: {reason}"));
            self.emit(s, "missed", "warning", None, format!("Skipped a scheduled run of {}", s.name), format!("It was due {}, but it was not started: {why}.\nThe schedule stays on; it will try again at its next time.", zone.show(due_at)), at);
            report.skipped += 1;
            return;
        }
        self.occurrence_logged(&s.owner_id, s.id, due_at, at, "failed_to_start", None, error);
        let deleted = error == "no such flow";
        self.advance_retry(s.id, if deleted { None } else { next }, 0, None, &format!("could not start: {error}"));
        self.emit(s, "failed", "error", None, format!("Scheduled run of {} could not start", s.name), format!("It was due {}.\nReason: {}{}", zone.show(due_at), if deleted { "the flow was deleted, so this schedule has been switched off" } else { error }, if deleted { "" } else { "\nThe schedule stays on; it will try again at its next time." }), at);
        report.failed += 1;
    }

    /// Run a schedule's flow right now (a person pressed "Run now"). The schedule's own times are not changed.
    pub fn run_now(&self, owner: &str, schedule_id: i64, at: i64) -> R<Run> {
        let s = self.store.get(owner, schedule_id)?.ok_or("no such schedule")?;
        if let Some((rid, status)) = self.store.active_run_of(owner, s.id)? {
            return Err(format!("the previous run (#{rid}) is still {status}"));
        }
        let run = self.start(&s, at)?;
        self.occurrence_logged(owner, s.id, at, at, "started", Some(run.id), "you ran it now");
        let word = self.status_word(owner, run.id, "running");
        let _ = self.store.set_last_status(s.id, run.id, &word);
        Ok(run)
    }

    /// Decide a missed time that was waiting for you: `run` it now, or `skip` it.
    pub fn resolve_missed(&self, owner: &str, occurrence_id: i64, decision: &str, at: i64) -> R<Option<Run>> {
        let occ = self.store.occurrence(owner, occurrence_id)?.ok_or("no such missed run")?;
        if occ.outcome != "missed_pending" {
            return Err("that one has already been decided".into());
        }
        let s = self.store.get(owner, occ.schedule_id)?.ok_or("no such schedule")?;
        match decision {
            "run" => {
                if s.campaign.max_runs.is_some_and(|m| s.runs_started >= m) {
                    return Err(format!("the campaign has already made all its {} runs, so this one is not run", s.runs_started));
                }
                let run = self.start(&s, occ.due_at)?;
                self.note_started_logged(s.id);
                self.store.resolve_occurrence(occ.id, "resolved_run", Some(run.id), "you chose to run it", at)?;
                let word = self.status_word(owner, run.id, "running");
                let _ = self.store.set_last_status(s.id, run.id, &word);
                Ok(Some(run))
            }
            "skip" => {
                self.store.resolve_occurrence(occ.id, "resolved_skip", None, "you chose to skip it", at)?;
                Ok(None)
            }
            other => Err(format!("choose \"run\" or \"skip\", not \"{other}\"")),
        }
    }

    /// In words: what happened to the time a schedule should have run at. For "why didn't my 10 AM workflow run?".
    pub fn why_not_run(&self, owner: &str, schedule_id: i64, expected: i64) -> R<String> {
        let s = self.store.get(owner, schedule_id)?.ok_or("no such schedule")?;
        let zone = zone_of(&s);
        let near = self.store.occurrences(owner, schedule_id, 100)?.into_iter().find(|o| (o.due_at - expected).abs() <= GRACE_SECS);
        if let Some(o) = near {
            return Ok(match o.outcome.as_str() {
                "started" | "started_late" | "resolved_run" => format!("It did run{}: run #{}.{}", if o.outcome == "started_late" { " (late)" } else { "" }, o.run_id.unwrap_or(0), if o.reason.is_empty() { String::new() } else { format!(" {}", o.reason) }),
                "skipped" | "resolved_skip" => format!("It was skipped: {}.", o.reason),
                "missed_pending" => format!("It was missed and is waiting for your decision: {}.", o.reason),
                _ => format!("It could not start: {}.", o.reason),
            });
        }
        if let Some((a, b)) = self.store.gap_covering(expected)? {
            return Ok(format!("Aqua was not running from {} to {}, so the {} run could not start.", zone.show(a), zone.show(b), zone.show(expected)));
        }
        if !s.enabled {
            return Ok("The schedule is switched off.".into());
        }
        if s.next_run_at.is_some_and(|n| n > expected) && s.created_at > expected {
            return Ok(format!("The schedule did not exist yet at {}; it was created {}.", zone.show(expected), zone.show(s.created_at)));
        }
        Ok(format!("I have no record of a run being due at {}. If the schedule was changed since, it may have been set for another time: it is now {}.", zone.show(expected), s.describe()))
    }

    /// How a schedule's runs have gone so far, and how many are still to come. Judged from what happened, not counted from
    /// what was started: a run that failed is not a run that worked.
    pub fn campaign(&self, owner: &str, schedule_id: i64, now: i64) -> R<CampaignView> {
        use crate::flows::supervisor::{judge, VerdictKind};
        let s = self.store.get(owner, schedule_id)?.ok_or("no such schedule")?;
        let zone = zone_of(&s);
        let mut v = CampaignView { name: s.name.clone(), runs_started: s.runs_started, total_planned: None, remaining: 0, worked: 0, recovered: 0, failed: 0, unverified: 0, in_progress: 0, skipped: 0, awaiting_decision: 0, next_run_at: s.next_run_at.filter(|_| s.enabled), ends_at: s.ends_at, over: false, text: String::new() };
        for o in self.store.occurrences(owner, schedule_id, 1000)? {
            match (o.outcome.as_str(), o.run_id) {
                ("started" | "started_late" | "resolved_run", Some(rid)) => match self.runner.store().get_run(owner, rid)? {
                    Some(run) if run.status.is_finished() => {
                        let nodes = self.runner.store().node_runs(rid).unwrap_or_default();
                        match judge(&run, &nodes, &run.def).kind {
                            VerdictKind::Success => v.worked += 1,
                            VerdictKind::Recovered => v.recovered += 1,
                            VerdictKind::Unknown => v.unverified += 1,
                            VerdictKind::Failed => v.failed += 1,
                            VerdictKind::Waiting => v.in_progress += 1,
                        }
                    }
                    Some(_) => v.in_progress += 1,
                    None => {}
                },
                ("skipped" | "resolved_skip", _) => v.skipped += 1,
                ("missed_pending", _) => v.awaiting_decision += 1,
                ("failed_to_start", _) => v.failed += 1,
                _ => {}
            }
        }
        // How many more are to come, worked out the way the scheduler will.
        let mut sim = s.clone();
        let mut t = if s.enabled { s.next_run_at } else { None };
        while let Some(at) = t {
            if v.remaining >= 1000 {
                break;
            }
            v.remaining += 1;
            sim.runs_started += 1;
            t = sim.next_within_campaign(at, false);
        }
        if s.campaign.is_set() {
            v.total_planned = Some(s.runs_started + v.remaining);
        }
        v.over = s.campaign.is_set() && s.runs_started > 0 && s.next_within_campaign(now, false).is_none();
        let mut parts = vec![match v.total_planned {
            Some(total) => format!("{} of {} runs started", v.runs_started, total),
            None => format!("{} runs started", v.runs_started),
        }];
        for (n, w) in [(v.worked, "worked"), (v.recovered, "worked after recovering"), (v.unverified, "could not be confirmed"), (v.failed, "failed"), (v.in_progress, "still going"), (v.skipped, "skipped"), (v.awaiting_decision, "waiting for your decision")] {
            if n > 0 {
                parts.push(format!("{n} {w}"));
            }
        }
        if let Some(n) = v.next_run_at {
            parts.push(format!("next {}", zone.show(n)));
        }
        if let Some(e) = s.campaign.end_on.as_ref() {
            parts.push(format!("last day {e}"));
        }
        v.text = parts.join("; ");
        Ok(v)
    }

    pub fn views(&self, owner: &str, flow_id: Option<i64>) -> R<Vec<ScheduleView>> {
        let mut out = Vec::new();
        let pending = self.store.pending_missed(owner)?;
        for s in self.store.list(owner, flow_id)? {
            let flow = self.flows.get(owner, s.flow_id)?;
            let zone = zone_of(&s);
            let active = self.store.active_run_of(owner, s.id)?.map(|(id, _)| id);
            let missed = pending.iter().find(|(o, _)| o.schedule_id == s.id).map(|(o, _)| o.id);
            let invalid = flow.as_ref().is_none_or(|f| !validate(&f.def).ok());
            let state = if missed.is_some() {
                State::NeedsAttention
            } else if !s.enabled {
                if (matches!(s.when, When::Once { .. }) && s.last_run_at > 0) || (s.campaign.is_set() && s.campaign_over(chrono::Utc::now().timestamp())) { State::Completed } else { State::Paused }
            } else if invalid {
                State::Invalid
            } else if missed.is_some() || s.last_status.starts_with("failed") || s.last_status == "unverified" || s.last_status.starts_with("could not start") {
                State::NeedsAttention
            } else if active.is_some() {
                State::Running
            } else {
                State::Scheduled
            };
            out.push(ScheduleView { next_run_text: s.next_run_at.map(|n| zone.show(n)).unwrap_or_default(), zone_label: zone.label(), flow_name: flow.map(|f| f.def.name).unwrap_or_else(|| "(deleted flow)".into()), active_run: active, pending_missed: missed, state, schedule: s });
        }
        Ok(out)
    }
}

// ---- turning what happens to a run into messages ---------------------------------------------------------------

/// Watches every run end (or stop to wait) and tells the person, as the schedule's preferences say.
pub struct RunNotifier {
    pub scheduler: Arc<Scheduler>,
}

impl RunObserver for RunNotifier {
    fn on_run_status(&self, run: &Run, nodes: &[NodeRun], status: RunStatus) {
        let _order = in_order();
        let sch = &self.scheduler;
        let schedule = run.payload.get("schedule_id").and_then(|v| v.as_i64()).filter(|_| run.trigger_kind == "schedule").and_then(|id| sch.store.get(&run.owner_id, id).ok().flatten());
        if let Some(s) = &schedule {
            if status.is_finished() {
                let _ = sch.store.set_last_status(s.id, run.id, match (status, crate::flows::supervisor::judge(run, nodes, &run.def).kind) { (RunStatus::Succeeded, VerdictKind::Success) => "succeeded", (RunStatus::Succeeded, VerdictKind::Recovered) => "recovered", (RunStatus::Succeeded, VerdictKind::Unknown) => "unverified", (RunStatus::Succeeded, _) | (RunStatus::Failed, _) => "failed", _ => status.as_str() });
            }
        }
        // A campaign's last run has ended: say how the whole campaign went.
        if let (Some(s), true) = (&schedule, status.is_finished()) {
            let fresh = sch.store.get(&s.owner_id, s.id).ok().flatten();
            let now = chrono::Utc::now().timestamp();
            if let Some(f) = fresh.filter(|f| f.campaign.is_set() && f.runs_started > 0 && f.next_within_campaign(f.last_run_at, false).is_none() && sch.store.active_run_of(&f.owner_id, f.id).ok().flatten().is_none()) {
                if !sch.notifier.store.exists_for_run(&f.owner_id, run.id, "campaign").unwrap_or(true) {
                    if let Ok(view) = sch.campaign(&f.owner_id, f.id, now) {
                        let _ = sch.notifier.emit(&f.owner_id, &f.notify.channels, NewNotification { kind: "campaign", level: if view.failed > 0 || view.unverified > 0 { "warning" } else { "success" }, flow_id: f.flow_id, run_id: Some(run.id), schedule_id: Some(f.id), title: format!("🏁 The campaign {} has finished", f.name), body: view.text.clone(), data: serde_json::to_value(&view).unwrap_or(Value::Null) }, now);
                    }
                }
            }
        }
        // A run someone started by hand only interrupts for what needs them; a schedule follows its preferences.
        let prefs = schedule.as_ref().map(|s| s.notify.clone()).unwrap_or(NotifyPrefs { on_start: false, on_success: false, on_failure: true, on_recovered: false, on_needs_you: true, on_each_step: false, channels: vec!["in_app".into()] });
        let zone = schedule.as_ref().map(zone_of).unwrap_or(Zone::Local);
        let next = schedule.as_ref().and_then(|s| s.next_run_at);
        let summary = summarize(run, nodes, &run.def, next);
        // A reminder that falls due says what it is for, not how a workflow ran.
        if run.def.description == crate::reminders::MARKER && status == RunStatus::Succeeded && !run.dry_run {
            let message = run.vars.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let _ = sch.notifier.emit(
                &run.owner_id,
                &prefs.channels,
                NewNotification { kind: "reminder", level: "info", flow_id: run.flow_id, run_id: Some(run.id), schedule_id: schedule.as_ref().map(|s| s.id), title: format!("⏰ {message}"), body: message, data: serde_json::json!({ "reminder": true }) },
                chrono::Utc::now().timestamp(),
            );
            return;
        }
        let (kind, level, title, wanted): (&'static str, &'static str, String, bool) = match status {
            RunStatus::Succeeded => match summary.verdict.kind {
                VerdictKind::Recovered => ("recovered", "success", format!("🔧 {} completed after recovering from a problem", run.name), prefs.on_recovered && !run.dry_run),
                // It ended, but nothing proves the work was done: say so instead of claiming success.
                VerdictKind::Unknown => ("unverified", "warning", format!("❔ {} finished, but I could not confirm it worked", run.name), prefs.on_failure && !run.dry_run),
                VerdictKind::Failed => ("failed", "error", format!("⚠️ {} could not be completed", run.name), prefs.on_failure && !run.dry_run),
                _ => ("succeeded", "success", format!("✅ {} completed successfully", run.name), prefs.on_success && !run.dry_run),
            },
            RunStatus::Failed => ("failed", "error", format!("⚠️ {} could not be completed", run.name), prefs.on_failure),
            RunStatus::Interrupted => ("failed", "warning", format!("⚠️ {} was interrupted", run.name), prefs.on_failure),
            RunStatus::Waiting => ("needs_you", "warning", format!("⏸ {} is waiting for you", run.name), prefs.on_needs_you && nodes.iter().any(|n| n.wait.as_ref().is_some_and(|w| !matches!(w, WaitRequest::Until { .. })))),
            _ => return,
        };
        let failed_for_good = matches!(status, RunStatus::Failed) || (status == RunStatus::Succeeded && summary.verdict.kind == VerdictKind::Failed);
        if failed_for_good && !run.dry_run {
            if let Some(s) = &schedule {
                sch.guard_repeated(s, chrono::Utc::now().timestamp());
            }
        }
        if !wanted {
            return;
        }
        let mut body = summary.text(&zone);
        if status == RunStatus::Waiting {
            body = String::new();
            for n in nodes.iter().filter(|n| matches!(n.status, NodeStatus::Waiting | NodeStatus::NeedsAction)) {
                let name = run.def.node(&n.node_id).map(|x| x.name.clone()).unwrap_or_default();
                match &n.wait {
                    Some(WaitRequest::Approval { title, preview, .. }) => body.push_str(&format!("Approve \"{name}\": {title}\n{}\n", preview.chars().take(300).collect::<String>())),
                    Some(WaitRequest::Input { question, .. }) => body.push_str(&format!("Question at \"{name}\": {question}\n")),
                    Some(WaitRequest::Failure { message, .. }) => body.push_str(&format!("{message}\n")),
                    _ => {}
                }
            }
        }
        let now = chrono::Utc::now().timestamp();
        let _ = sch.notifier.emit(&run.owner_id, &prefs.channels, NewNotification { kind, level, flow_id: run.flow_id, run_id: Some(run.id), schedule_id: schedule.as_ref().map(|s| s.id), title, body, data: serde_json::to_value(&summary).unwrap_or(Value::Null) }, now);
    }
}
