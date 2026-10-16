use super::model::*;
use super::notify::*;
use super::run::exec::*;
use super::run::manager::FlowRunner;
use super::run::model::*;
use super::run::store::RunStore;
use super::sched::model::*;
use super::sched::store::SchedStore;
use super::sched::tick::{RunNotifier, Scheduler};
use super::store::FlowStore;
use super::zone::Zone;
use crate::workflows::schedule::CronExpr;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 2030-01-01 00:00:00 UTC (a Tuesday).
const T0: i64 = 1_893_456_000;

fn ts(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc).timestamp()
}

// ---- time zones -------------------------------------------------------------------------------------------

#[test]
fn a_named_zone_turns_a_wall_clock_time_into_the_right_moment_and_back() {
    let kolkata = Zone::parse("Asia/Kolkata").unwrap();
    assert_eq!(kolkata.label(), "Asia/Kolkata");
    let ten = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap().and_hms_opt(10, 0, 0).unwrap();
    assert_eq!(kolkata.to_timestamp(ten), Some(ts("2030-01-01T04:30:00Z")), "10:00 IST is 04:30 UTC");
    assert_eq!(kolkata.naive_at(ts("2030-01-01T04:30:00Z")), Some(ten));
    assert_eq!(kolkata.show(ts("2030-01-01T04:30:00Z")), "2030-01-01 10:00 IST");
    let daily = CronExpr::parse("0 10 * * *").unwrap();
    assert_eq!(kolkata.next_cron(&daily, T0), Some(ts("2030-01-01T04:30:00Z")), "at 05:30 IST the next 10:00 is today");
    assert_eq!(kolkata.next_cron(&daily, ts("2030-01-01T04:30:00Z")), Some(ts("2030-01-02T04:30:00Z")), "strictly after");
    // Offsets, UTC, this computer's zone, and what is refused.
    assert_eq!(Zone::parse("+05:30").unwrap().label(), "UTC+05:30");
    assert_eq!(Zone::parse("-08:00").unwrap().to_timestamp(ten), Some(ts("2030-01-01T18:00:00Z")));
    assert_eq!(Zone::parse("UTC").unwrap().label(), "UTC");
    assert_eq!(Zone::parse("").unwrap(), Zone::Local);
    for bad in ["Mars/Olympus", "+25:00", "+xx", "Kolkata"] {
        assert!(Zone::parse(bad).is_err(), "{bad}");
    }
    assert!(Zone::parse("Mars/Olympus").unwrap_err().contains("Asia/Kolkata"));
}

#[test]
fn daylight_saving_never_runs_a_time_twice_or_at_a_time_that_does_not_exist() {
    let ny = Zone::parse("America/New_York").unwrap();
    // 2027-03-14: clocks go forward at 02:00, so 02:30 does not exist that day.
    let d = |y, m, dd, h, mi| NaiveDate::from_ymd_opt(y, m, dd).unwrap().and_hms_opt(h, mi, 0).unwrap();
    assert_eq!(ny.to_timestamp(d(2027, 3, 14, 2, 30)), None);
    let cron = CronExpr::parse("30 2 * * *").unwrap();
    let next = ny.next_cron(&cron, ts("2027-03-14T00:00:00Z")).unwrap();
    assert_eq!(ny.naive_at(next).unwrap(), d(2027, 3, 15, 2, 30), "that day is skipped, not moved to 03:30");
    // 2026-11-01: clocks go back at 02:00, so 01:30 happens twice; the first is used, once.
    let first = ny.to_timestamp(d(2026, 11, 1, 1, 30)).unwrap();
    assert_eq!(first, ts("2026-11-01T05:30:00Z"), "01:30 EDT, not 01:30 EST");
    let cron = CronExpr::parse("30 1 * * *").unwrap();
    assert_eq!(ny.next_cron(&cron, ts("2026-11-01T00:00:00Z")), Some(first));
    assert_eq!(ny.next_cron(&cron, first), Some(ts("2026-11-02T06:30:00Z")), "the repeated 01:30 does not fire again");
    // A daily 10:00 stays 10:00 on the wall clock across the change (the moment shifts by an hour).
    let ten = CronExpr::parse("0 10 * * *").unwrap();
    let before = ny.next_cron(&ten, ts("2027-03-13T12:00:00Z")).unwrap();
    let after = ny.next_cron(&ten, before).unwrap();
    assert_eq!((ny.naive_at(before).unwrap(), ny.naive_at(after).unwrap()), (d(2027, 3, 13, 10, 0), d(2027, 3, 14, 10, 0)));
    assert_eq!(after - before, 23 * 3600);
}

// ---- schedule input -------------------------------------------------------------------------------------------

fn cron_input(name: &str, cron: &str, tz: &str) -> ScheduleInput {
    ScheduleInput { name: name.into(), when: When::Cron { cron: cron.into() }, timezone: tz.into(), inputs: Map::new(), misfire: Misfire::Ask, notify: NotifyPrefs::default(), enabled: true, campaign: Default::default() }
}

#[test]
fn a_schedule_is_checked_before_it_is_stored_and_says_when_it_will_first_run() {
    let ok = cron_input("Daily post", "0 10 * * *", "Asia/Kolkata");
    let (zone, next) = ok.validate(T0).unwrap();
    assert_eq!((zone.label(), next), ("Asia/Kolkata".to_string(), Some(ts("2030-01-01T04:30:00Z"))));
    let e = |i: ScheduleInput| i.validate(T0).unwrap_err();
    assert!(e(cron_input("  ", "0 10 * * *", "")).contains("needs a name"));
    assert!(e(cron_input("x", "99 * * * *", "")).contains("not valid"));
    assert!(e(cron_input("x", "0 10 * * *", "Mars/Olympus")).contains("not a time zone I know"));
    let once = |when: &str, tz: &str| ScheduleInput { when: When::Once { when: when.into() }, ..cron_input("once", "", tz) };
    assert_eq!(once("2030-06-01 10:00", "UTC").validate(T0).unwrap().1, Some(ts("2030-06-01T10:00:00Z")));
    assert!(e(once("2029-01-01 10:00", "UTC")).contains("already passed"));
    assert!(e(once("tomorrow", "UTC")).contains("not a date and time"));
    assert!(e(once("2027-03-14 02:30", "America/New_York")).contains("does not exist"), "the time that DST skips");
    let mut bad = cron_input("x", "0 10 * * *", "");
    bad.notify.channels = vec!["pigeon".into()];
    assert!(e(bad).contains("not a notification channel"));
    let json: ScheduleInput = serde_json::from_value(json!({ "name": "n", "when": { "type": "cron", "cron": "0 9 * * 1" }, "timezone": "UTC" })).unwrap();
    assert_eq!((json.misfire, json.enabled, json.notify.channels.clone(), json.notify.on_each_step), (Misfire::Ask, true, vec!["in_app".to_string()], false), "safe defaults: ask about misses, tell me in the app");
}

// ---- a rig with a real engine and a fake outside world ---------------------------------------------------------

struct Instant;
impl NodeExecutor for Instant {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::TextGeneration, NodeKind::SocialMedia]
    }
    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let mut o = Map::new();
        match ctx.node.kind {
            NodeKind::SocialMedia => {
                o.insert("status".into(), json!("published"));
                o.insert("post_url".into(), json!("https://example.com/p/1"));
            }
            _ => {
                o.insert("text".into(), json!("Generated"));
            }
        }
        Ok(NodeResult { outputs: o, logs: vec![] })
    }
}

struct Rig {
    flows: Arc<FlowStore>,
    runs: Arc<RunStore>,
    sched: Arc<SchedStore>,
    notes: Arc<NotifyStore>,
    runner: Arc<FlowRunner>,
    scheduler: Arc<Scheduler>,
    path: std::path::PathBuf,
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("aqua_sched_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let flows = Arc::new(FlowStore::new(&path));
    let runs = Arc::new(RunStore::new(&path));
    let sched = Arc::new(SchedStore::new(&path));
    let notes = Arc::new(NotifyStore::new(&path));
    let clock = Arc::new(AtomicI64::new(T0));
    let c = clock.clone();
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(Arc::new(Instant))), Arc::new(|_, cancel| !cancel.load(Ordering::SeqCst)), Arc::new(move || c.load(Ordering::SeqCst)));
    let scheduler = Arc::new(Scheduler { store: sched.clone(), runner: runner.clone(), flows: flows.clone(), notifier: Arc::new(Notifier::new(notes.clone())) });
    runner.observe(Arc::new(RunNotifier { scheduler: scheduler.clone() }));
    Rig { flows, runs, sched, notes, runner, scheduler, path }
}

fn flow_def(name: &str, with_approval: bool) -> FlowDef {
    let mut nodes = vec![
        json!({ "id": "start", "kind": "trigger", "name": "Start", "config": { "trigger_type": "manual" } }),
        json!({ "id": "write", "kind": "text_generation", "name": "Write", "config": { "prompt": "Write about {{vars.topic}}" } }),
    ];
    let mut edges = vec![json!({ "from": "start", "to": "write" })];
    if with_approval {
        nodes.push(json!({ "id": "ok", "kind": "approval", "name": "Approve", "config": { "title": "Publish?", "preview": "{{nodes.write.text}}" } }));
        nodes.push(json!({ "id": "pub", "kind": "social_media", "name": "Publish", "config": { "platform": "linkedin", "action": "publish", "text": "{{nodes.ok.text}}" } }));
        edges.push(json!({ "from": "write", "to": "ok" }));
        edges.push(json!({ "from": "ok", "to": "pub", "port": "approved" }));
    } else {
        nodes.push(json!({ "id": "pub", "kind": "social_media", "name": "Publish", "config": { "platform": "linkedin", "action": "publish", "text": "{{nodes.write.text}}" } }));
        edges.push(json!({ "from": "write", "to": "pub" }));
    }
    serde_json::from_value(json!({ "name": name, "settings": { "approval": "never" }, "variables": [{ "name": "topic", "label": "Topic", "kind": "text", "required": true, "default": "cats" }], "nodes": nodes, "edges": edges })).unwrap()
}

impl Rig {
    fn flow(&self, name: &str, with_approval: bool) -> i64 {
        self.flows.create("local", flow_def(name, with_approval), "user", "", T0).unwrap().id
    }
    fn schedule(&self, flow: i64, input: ScheduleInput) -> Schedule {
        self.sched.create("local", flow, &input, T0).unwrap()
    }
    fn settle(&self) {
        assert!(self.runner.wait_idle(Duration::from_secs(15)));
    }
    fn tick(&self, at: i64) -> super::sched::tick::TickReport {
        let r = self.scheduler.tick(at);
        self.settle();
        r
    }
    fn get(&self, id: i64) -> Schedule {
        self.sched.get("local", id).unwrap().unwrap()
    }
    fn occurrences(&self, id: i64) -> Vec<Occurrence> {
        self.sched.occurrences("local", id, 50).unwrap()
    }
    fn notes(&self) -> Vec<Notification> {
        let mut v = self.notes.list("local", false, 100).unwrap();
        v.reverse();
        v
    }
    fn kinds(&self) -> Vec<String> {
        self.notes().into_iter().map(|n| n.kind).collect()
    }
    fn run(&self, id: i64) -> Run {
        self.runs.get_run("local", id).unwrap().unwrap()
    }
}

// ---- firing on time ----------------------------------------------------------------------------------------------------

/// The scenario of QA-15: the scheduler thread (which also fires reminders) panics in a tick, the supervisor logs it and enters the loop again, and the schedule still fires.
#[test]
fn a_schedule_still_fires_after_the_scheduler_panicked_in_a_tick() {
    use crate::supervise::{supervise, Backoff, Env};
    let r = rig("panic_in_tick");
    let f = r.flow("Remind me", false);
    let s = r.schedule(f, cron_input("Every day at 10", "0 10 * * *", "Asia/Kolkata"));
    let due = ts("2030-01-01T04:30:00Z");
    assert_eq!(s.next_run_at, Some(due));
    // The next tick of this scheduler panics.
    *super::sched::tick::PANIC_NEXT_TICK_OF.lock().unwrap() = Some(Arc::as_ptr(&r.scheduler) as usize);
    let env = Env::new(|| Duration::from_secs(1), |_| {}, None);
    let before = crate::metrics::counter_value("aqua_thread_panics_total", &[("thread", "test-flow-scheduler")]);
    supervise("test-flow-scheduler", Backoff::STANDARD, &env, || {
        // The loop of the thread, with the time of the schedule and a stop once the schedule has fired.
        r.scheduler.run_until(Duration::from_millis(1), &|| due + 2, &|| !r.occurrences(s.id).is_empty());
    });
    r.settle();
    assert_eq!(crate::metrics::counter_value("aqua_thread_panics_total", &[("thread", "test-flow-scheduler")]) - before, 1.0, "the panic was caught and counted once");
    assert!(!crate::supervise::background_failed().iter().any(|n| n == "test-flow-scheduler"), "one panic does not give the thread up");
    let occurrences = r.occurrences(s.id);
    assert_eq!(occurrences.len(), 1, "it fired once, not twice");
    assert_eq!((occurrences[0].outcome.as_str(), occurrences[0].due_at), ("started", due), "the reminder fired after the panic");
    assert_eq!(r.get(s.id).last_status, "succeeded");
}

#[test]
fn a_schedule_fires_at_its_time_starts_a_run_by_itself_records_it_and_tells_me_when_it_finishes() {
    let r = rig("fires");
    let f = r.flow("Daily post", false);
    let s = r.schedule(f, ScheduleInput { inputs: [("topic".to_string(), json!("AI agents"))].into_iter().collect(), ..cron_input("Every day at 10", "0 10 * * *", "Asia/Kolkata") });
    let due = ts("2030-01-01T04:30:00Z");
    assert_eq!(s.next_run_at, Some(due));
    assert_eq!(r.tick(due - 10), Default::default(), "not yet: nothing happens, nothing is recorded");
    assert!(r.occurrences(s.id).is_empty());
    let report = r.tick(due + 2);
    assert_eq!((report.started, report.failed), (1, 0));
    let occ = &r.occurrences(s.id)[0];
    assert_eq!((occ.outcome.as_str(), occ.due_at), ("started", due));
    let run = r.run(occ.run_id.unwrap());
    assert_eq!((run.trigger_kind.as_str(), run.status, run.vars["topic"].clone()), ("schedule", RunStatus::Succeeded, json!("AI agents")), "it ran by itself with the schedule's inputs");
    assert_eq!(run.payload["schedule_id"], s.id);
    let after = r.get(s.id);
    assert_eq!((after.next_run_at, after.last_status.as_str(), after.last_run_id), (Some(ts("2030-01-02T04:30:00Z")), "succeeded", Some(run.id)), "the next run is tomorrow at 10:00 IST");
    assert_eq!(r.kinds(), ["started", "succeeded"], "I was told when it started and when it finished");
    let done = r.notes().pop().unwrap();
    assert!(done.title.starts_with("✅ Daily post completed successfully") && done.level == "success", "{}", done.title);
    for line in ["Workflow: Daily post", "Steps: 3 total, 3 succeeded, 0 failed", "Result: https://example.com/p/1", "Next run: 2030-01-02 10:00 IST"] {
        assert!(done.body.contains(line), "{line} in\n{}", done.body);
    }
    assert_eq!(done.data["steps_succeeded"], 3, "the summary is stored as data as well as text");
    assert_eq!(r.tick(due + 30), Default::default(), "once is enough: it is not due again until tomorrow");
    assert_eq!(r.tick(ts("2030-01-02T04:30:05Z")).started, 1, "and tomorrow it fires again");
}

#[test]
fn notification_preferences_decide_what_is_said_and_a_hand_started_run_only_speaks_up_when_it_needs_me() {
    let r = rig("prefs");
    let f = r.flow("Quiet", false);
    let mut quiet = cron_input("Quiet", "0 10 * * *", "UTC");
    quiet.notify = NotifyPrefs { on_start: false, on_success: false, ..NotifyPrefs::default() };
    let s = r.schedule(f, quiet);
    r.tick(s.next_run_at.unwrap() + 1);
    assert_eq!(r.run(r.occurrences(s.id)[0].run_id.unwrap()).status, RunStatus::Succeeded);
    assert!(r.notes().is_empty(), "start and success were switched off");
    // A run someone started by hand: silent unless it needs them.
    let run = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded);
    assert!(r.notes().is_empty());
    let g = r.flow("Needs me", true);
    let mut d = r.flows.get("local", g).unwrap().unwrap().def;
    d.settings.approval = ApprovalPolicy::Always;
    r.flows.update("local", g, d, "user", "", None, T0 + 1).unwrap();
    let waiting = r.runner.start("local", g, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    assert_eq!(r.run(waiting.id).status, RunStatus::Waiting);
    let n = r.notes().pop().unwrap();
    assert_eq!((n.kind.as_str(), n.level.as_str()), ("needs_you", "warning"));
    assert!(n.title.contains("Needs me is waiting for you") && n.body.contains("Approve \"Approve\": Publish?") && n.body.contains("Generated"), "{}", n.body);
}

#[test]
fn a_run_that_fails_is_reported_with_the_step_and_the_reason() {
    let r = rig("failure");
    let f = r.flow("Breaks", false);
    let mut d = r.flows.get("local", f).unwrap().unwrap().def;
    d.node_mut("pub").unwrap().config["action"] = json!("get_post"); // still valid
    d.node_mut("write").unwrap().config["prompt"] = json!("{{vars.topic}}");
    r.flows.update("local", f, d, "user", "", None, T0 + 1).unwrap();
    struct Broken;
    impl NodeExecutor for Broken {
        fn kinds(&self) -> &[NodeKind] {
            &[NodeKind::TextGeneration, NodeKind::SocialMedia]
        }
        fn execute(&self, _c: &NodeCtx) -> Result<NodeResult, NodeError> {
            Err(NodeError::Fatal("the site refused the login (token=abc999)".into()))
        }
    }
    let runner = FlowRunner::with_clock(r.flows.clone(), r.runs.clone(), Arc::new(Executors::new().with(Arc::new(Broken))), Arc::new(|_, c| !c.load(Ordering::SeqCst)), Arc::new(|| T0));
    let scheduler = Arc::new(Scheduler { store: r.sched.clone(), runner: runner.clone(), flows: r.flows.clone(), notifier: Arc::new(Notifier::new(r.notes.clone())) });
    runner.observe(Arc::new(RunNotifier { scheduler: scheduler.clone() }));
    let s = r.schedule(f, cron_input("Breaks", "0 10 * * *", "UTC"));
    scheduler.tick(s.next_run_at.unwrap() + 1);
    assert!(runner.wait_idle(Duration::from_secs(10)));
    let fail = r.notes().into_iter().find(|n| n.kind == "failed").expect("a failure notification");
    assert!(fail.title.starts_with("⚠️ Breaks could not be completed") && fail.level == "error");
    assert!(fail.body.contains("Stopped at \"Write\"") && fail.body.contains("the site refused the login"), "{}", fail.body);
    assert!(!fail.body.contains("abc999"), "no secret in a notification: {}", fail.body);
    assert_eq!(r.get(s.id).last_status, "failed");
    let views = scheduler.views("local", None).unwrap();
    assert_eq!(views[0].state, State::NeedsAttention, "a schedule whose last run failed says so");
}

// ---- missing a time ------------------------------------------------------------------------------------------------------

/// A schedule that has been due for `late` seconds, with Aqua either running the whole time or not.
fn missed(r: &Rig, misfire: Misfire, downtime: bool) -> (Schedule, i64, i64) {
    let f = r.flow(&format!("Missed {misfire:?} {downtime}"), false);
    let mut input = cron_input("Daily", "0 10 * * *", "UTC");
    input.misfire = misfire;
    let s = r.schedule(f, input);
    let due = s.next_run_at.unwrap();
    // Before it was due Aqua was alive; then, if it was down, nothing until 40 minutes after the due time.
    r.scheduler.tick(due - 600);
    let back = due + 40 * 60;
    if !downtime {
        for t in (due - 590..back).step_by(10) {
            r.sched.heartbeat(t).unwrap(); // Aqua was running, just not looking at this schedule
        }
    }
    (s, due, back)
}

#[test]
fn a_time_missed_while_aqua_was_off_follows_the_schedules_own_rule_and_says_why() {
    // Skip: nothing runs, it is recorded with the reason, and tomorrow's run stays on.
    let r = rig("miss_skip");
    let (s, due, back) = missed(&r, Misfire::Skip, true);
    let report = r.tick(back);
    assert_eq!((report.skipped, report.started), (1, 0));
    let o = &r.occurrences(s.id)[0];
    assert_eq!(o.outcome, "skipped");
    assert!(o.reason.contains("Aqua was not running from") && o.reason.contains("Skipped"), "{}", o.reason);
    assert_eq!(r.get(s.id).next_run_at, Some(due + 86_400), "it does not replay every missed time: the next is tomorrow");
    assert!(r.notes().iter().any(|n| n.kind == "missed" && n.body.contains("Aqua was not running")));
    // Run now: as soon as Aqua is back.
    let r = rig("miss_run");
    let (s, _, back) = missed(&r, Misfire::RunNow, true);
    assert_eq!(r.tick(back).started, 1);
    let o = &r.occurrences(s.id)[0];
    assert_eq!(o.outcome, "started_late");
    assert!(o.reason.contains("Aqua was not running"));
    assert_eq!(r.run(o.run_id.unwrap()).status, RunStatus::Succeeded);
    // Next run: same as skip, read differently.
    let r = rig("miss_next");
    let (s, _, back) = missed(&r, Misfire::NextRun, true);
    assert_eq!(r.tick(back).skipped, 1);
    assert_eq!(r.occurrences(s.id)[0].outcome, "skipped");
}

#[test]
fn ask_me_leaves_the_decision_in_the_inbox_and_running_or_skipping_it_is_recorded() {
    let r = rig("miss_ask");
    let (s, due, back) = missed(&r, Misfire::Ask, true);
    let report = r.tick(back);
    assert_eq!((report.asked, report.started), (1, 0));
    let o = r.occurrences(s.id)[0].clone();
    assert_eq!(o.outcome, "missed_pending");
    assert!(r.notes().iter().any(|n| n.kind == "needs_you" && n.title.contains("run it now?")));
    let views = r.scheduler.views("local", None).unwrap();
    assert_eq!((views[0].state, views[0].pending_missed), (State::NeedsAttention, Some(o.id)));
    assert_eq!(r.get(s.id).next_run_at, Some(due + 86_400), "the schedule itself carries on");
    assert!(r.scheduler.resolve_missed("local", o.id, "maybe", back).unwrap_err().contains("\"run\" or \"skip\""));
    let run = r.scheduler.resolve_missed("local", o.id, "run", back + 5).unwrap().unwrap();
    r.settle();
    assert_eq!(r.run(run.id).status, RunStatus::Succeeded);
    assert_eq!((r.sched.occurrence("local", o.id).unwrap().unwrap().outcome.as_str(), r.sched.occurrence("local", o.id).unwrap().unwrap().run_id), ("resolved_run", Some(run.id)));
    assert!(r.scheduler.resolve_missed("local", o.id, "skip", back).unwrap_err().contains("already been decided"));
    assert!(r.sched.pending_missed("local").unwrap().is_empty());
    // And skipping.
    let r = rig("miss_ask_skip");
    let (s, _, back) = missed(&r, Misfire::Ask, true);
    r.tick(back);
    let o = r.occurrences(s.id)[0].clone();
    assert!(r.scheduler.resolve_missed("local", o.id, "skip", back).unwrap().is_none());
    assert_eq!(r.sched.occurrence("local", o.id).unwrap().unwrap().outcome, "resolved_skip");
    assert!(r.scheduler.resolve_missed("mallory", o.id, "run", back).is_err(), "another owner cannot decide it");
}

#[test]
fn a_late_scheduler_is_told_apart_from_a_computer_that_was_off_and_a_little_lateness_is_not_a_miss() {
    let r = rig("late");
    // Aqua was running the whole time, only the scheduler was behind.
    let (s, _, back) = missed(&r, Misfire::Skip, false);
    r.tick(back);
    let reason = r.occurrences(s.id)[0].reason.clone();
    assert!(reason.contains("the scheduler was 40 minutes late") && !reason.contains("not running"), "{reason}");
    // Four minutes late is inside the grace period: it just runs.
    let f = r.flow("Almost on time", false);
    let mut input = cron_input("A", "0 10 * * *", "UTC");
    input.misfire = Misfire::Skip;
    let s2 = r.schedule(f, input);
    r.tick(s2.next_run_at.unwrap() + 240);
    assert_eq!(r.occurrences(s2.id)[0].outcome, "started", "not 'started_late', and the skip rule did not apply");
}

#[test]
fn a_schedule_never_starts_on_top_of_its_own_unfinished_run() {
    let r = rig("overlap");
    let f = r.flow("Slow", true);
    let mut d = r.flows.get("local", f).unwrap().unwrap().def;
    d.settings.approval = ApprovalPolicy::Always;
    r.flows.update("local", f, d, "user", "", None, T0 + 1).unwrap();
    let s = r.schedule(f, cron_input("Hourly", "0 * * * *", "UTC"));
    let first = s.next_run_at.unwrap();
    r.tick(first);
    let run_id = r.occurrences(s.id)[0].run_id.unwrap();
    assert_eq!(r.run(run_id).status, RunStatus::Waiting, "it is waiting for my approval");
    let report = r.tick(first + 3600);
    assert_eq!((report.skipped, report.started), (1, 0));
    let o = &r.occurrences(s.id)[0];
    assert!(o.outcome == "skipped" && o.reason.contains(&format!("the previous run (#{run_id}) is still waiting")), "{o:?}");
    assert!(r.notes().iter().any(|n| n.kind == "missed" && n.body.contains("still waiting")));
    assert_eq!(r.get(s.id).next_run_at, Some(first + 7200));
    // Approve it; the schedule then carries on as normal.
    r.runner.respond("local", run_id, "ok", Response { decision: Some("approved".into()), ..Default::default() }).unwrap();
    r.settle();
    assert_eq!(r.tick(first + 7200).started, 1);
}

#[test]
fn a_run_that_cannot_start_is_recorded_with_its_reason_and_the_schedule_stays_on() {
    let r = rig("cannot");
    // A flow that has become invalid.
    let f = r.flow("Broken later", false);
    let s = r.schedule(f, cron_input("A", "0 10 * * *", "UTC"));
    let mut d = r.flows.get("local", f).unwrap().unwrap().def;
    d.edges.clear();
    r.flows.update("local", f, d, "user", "", None, T0 + 5).unwrap();
    let due = s.next_run_at.unwrap();
    assert_eq!(r.tick(due).failed, 1);
    let o = &r.occurrences(s.id)[0];
    assert!(o.outcome == "failed_to_start" && o.reason.contains("has problems"), "{o:?}");
    let after = r.get(s.id);
    assert!(after.enabled && after.next_run_at == Some(due + 86_400) && after.last_status.starts_with("could not start"), "it will try again tomorrow: {after:?}");
    let n = r.notes().pop().unwrap();
    assert!(n.title.contains("could not start") && n.body.contains("will try again at its next time"), "{}", n.body);
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::Invalid, "and the dashboard says why it cannot run");
    // A required input nobody gave.
    let g = r.flow("Needs input", false);
    let mut d = r.flows.get("local", g).unwrap().unwrap().def;
    d.variables[0].default = None;
    r.flows.update("local", g, d, "user", "", None, T0 + 5).unwrap();
    let s2 = r.schedule(g, cron_input("B", "0 10 * * *", "UTC"));
    r.tick(s2.next_run_at.unwrap());
    assert!(r.occurrences(s2.id)[0].reason.contains("missing: Topic"), "{:?}", r.occurrences(s2.id));
    // A deleted flow switches its schedule off instead of failing forever.
    let h = r.flow("Doomed", false);
    let s3 = r.schedule(h, cron_input("C", "0 10 * * *", "UTC"));
    r.flows.delete("local", h, "user", T0 + 9).unwrap();
    assert!(r.sched.list("local", Some(h)).unwrap().is_empty(), "deleting a flow deletes its schedules");
    let orphan = r.sched.create("local", 424242, &cron_input("Orphan", "0 10 * * *", "UTC"), T0).unwrap();
    r.tick(orphan.next_run_at.unwrap());
    assert!(!r.get(orphan.id).enabled && r.occurrences(orphan.id)[0].reason == "no such flow");
    assert!(r.notes().iter().any(|n| n.body.contains("the flow was deleted, so this schedule has been switched off")));
    let _ = s3;
}

// ---- one-time schedules, the dashboard, run now -----------------------------------------------------------------------------

#[test]
fn a_one_time_schedule_runs_once_and_is_then_complete() {
    let r = rig("once");
    let f = r.flow("Launch", false);
    let s = r.schedule(f, ScheduleInput { when: When::Once { when: "2030-01-05 09:00".into() }, ..cron_input("Launch day", "", "Asia/Kolkata") });
    let due = ts("2030-01-05T03:30:00Z");
    assert_eq!(s.next_run_at, Some(due));
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::Scheduled);
    assert_eq!(r.tick(due + 1).started, 1);
    let after = r.get(s.id);
    assert_eq!((after.next_run_at, after.enabled, after.last_status.as_str()), (None, false, "succeeded"));
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::Completed);
    assert_eq!(r.tick(due + 86_400), Default::default(), "never again");
    assert!(r.sched.set_enabled("local", s.id, true, due + 100).unwrap_err().contains("has passed"), "a spent one-time schedule cannot be switched back on");
}

#[test]
fn the_dashboard_shows_where_each_schedule_is_and_run_now_does_not_disturb_the_timetable() {
    let r = rig("views");
    let f = r.flow("Post", false);
    let s = r.schedule(f, cron_input("Daily", "0 10 * * *", "Asia/Kolkata"));
    let v = &r.scheduler.views("local", None).unwrap()[0];
    assert_eq!((v.state, v.flow_name.as_str(), v.zone_label.as_str(), v.next_run_text.as_str()), (State::Scheduled, "Post", "Asia/Kolkata", "2030-01-01 10:00 IST"));
    // Paused, and coming back computes a fresh next time rather than firing a stale one.
    let paused = r.sched.set_enabled("local", s.id, false, T0).unwrap().unwrap();
    assert_eq!((paused.enabled, paused.next_run_at), (false, None));
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::Paused);
    assert_eq!(r.tick(ts("2030-01-03T00:00:00Z")), Default::default(), "a paused schedule does not fire");
    let back = r.sched.set_enabled("local", s.id, true, ts("2030-01-03T00:00:00Z")).unwrap().unwrap();
    assert_eq!(back.next_run_at, Some(ts("2030-01-03T04:30:00Z")), "the next 10:00 after now; nothing from the days it was off");
    // Run now.
    let run = r.scheduler.run_now("local", s.id, ts("2030-01-03T01:00:00Z")).unwrap();
    r.settle();
    assert_eq!((r.run(run.id).trigger_kind.as_str(), r.get(s.id).next_run_at), ("schedule", Some(ts("2030-01-03T04:30:00Z"))));
    assert_eq!(r.occurrences(s.id)[0].reason, "you ran it now");
    // Several schedules on one flow, and other owners cannot see them.
    let second = r.schedule(f, cron_input("Weekly", "0 9 * * 1", "UTC"));
    assert_eq!(r.sched.list("local", Some(f)).unwrap().len(), 2);
    assert!(r.sched.get("mallory", second.id).unwrap().is_none() && r.sched.list("mallory", None).unwrap().is_empty());
    assert!(r.sched.set_enabled("mallory", second.id, false, T0).unwrap().is_none() && !r.sched.delete("mallory", second.id).unwrap());
    assert!(r.scheduler.run_now("mallory", second.id, T0).is_err());
    assert!(r.sched.update("local", second.id, &cron_input("Weekly v2", "0 9 * * 2", "UTC"), T0).unwrap().is_some());
    assert!(r.sched.delete("local", second.id).unwrap());
}

#[test]
fn asked_why_a_run_did_not_happen_it_answers_from_the_record() {
    let r = rig("why");
    let f = r.flow("Post", false);
    let mut input = cron_input("Daily", "0 10 * * *", "Asia/Kolkata");
    input.misfire = Misfire::Skip;
    let s = r.schedule(f, input);
    let due = s.next_run_at.unwrap();
    // It ran.
    r.tick(due);
    assert!(r.scheduler.why_not_run("local", s.id, due).unwrap().starts_with("It did run: run #"));
    // The next day Aqua was off.
    let d2 = due + 86_400;
    r.scheduler.tick(d2 - 600);
    r.tick(d2 + 3000);
    let why = r.scheduler.why_not_run("local", s.id, d2).unwrap();
    assert!(why.starts_with("It was skipped: missed: Aqua was not running from"), "{why}");
    // A time before any record, while Aqua was off, is explained by the gap alone.
    let gap = r.sched.gap_covering(d2 - 100).unwrap();
    assert!(gap.is_some());
    // Switched off.
    r.sched.set_enabled("local", s.id, false, d2 + 4000).unwrap();
    let why = r.scheduler.why_not_run("local", s.id, d2 + 3 * 86_400).unwrap();
    assert_eq!(why, "The schedule is switched off.");
}

// ---- notifications -----------------------------------------------------------------------------------------------------------

#[test]
fn notifications_are_stored_read_and_counted_and_a_channel_that_is_not_connected_says_so() {
    let r = rig("notes");
    let notifier = Notifier::new(r.notes.clone());
    let say = |title: &str, channels: &[&str]| {
        notifier
            .emit("local", &channels.iter().map(|s| s.to_string()).collect::<Vec<_>>(), NewNotification { kind: "succeeded", level: "success", flow_id: 1, run_id: None, schedule_id: None, title: title.into(), body: "b".into(), data: Value::Null }, T0)
            .unwrap()
    };
    let a = say("one", &["in_app"]);
    let b = say("two", &["in_app", "email", "slack"]);
    assert_eq!(a.delivery.len(), 1);
    assert_eq!(b.delivery.iter().map(|d| (d.channel.as_str(), d.ok)).collect::<Vec<_>>(), [("in_app", true), ("email", false), ("slack", false)]);
    assert!(b.delivery[1].note.contains("Email is not connected"));
    let stored = r.notes.list("local", false, 10).unwrap();
    assert_eq!(stored[0].title, "two");
    assert_eq!(stored[0].delivery.len(), 3, "what could not be delivered is remembered, not lost");
    assert_eq!(r.notes.unread_count("local").unwrap(), 2);
    assert_eq!(r.notes.mark_read("local", &[a.id]).unwrap(), 1);
    assert_eq!(r.notes.list("local", true, 10).unwrap().len(), 1);
    assert_eq!(r.notes.mark_read("local", &[]).unwrap(), 1, "no ids means all");
    assert_eq!(r.notes.unread_count("local").unwrap(), 0);
    assert!(r.notes.list("mallory", false, 10).unwrap().is_empty());
    assert_eq!(r.notes.mark_read("mallory", &[a.id]).unwrap(), 0);
    let status = notifier.channel_status();
    assert_eq!(status.iter().map(|c| (c.id.as_str(), c.connected)).collect::<Vec<_>>(), [("in_app", true), ("email", false), ("slack", false), ("push", false)]);
    // A connected channel delivers.
    struct Log(std::sync::Mutex<Vec<String>>);
    impl Channel for Log {
        fn id(&self) -> &'static str {
            "slack"
        }
        fn label(&self) -> &'static str {
            "Slack"
        }
        fn connected(&self) -> bool {
            true
        }
        fn deliver(&self, n: &Notification) -> Result<(), String> {
            self.0.lock().unwrap().push(n.title.clone());
            Ok(())
        }
    }
    let log = Arc::new(Log(Default::default()));
    let with = Notifier::new(r.notes.clone()).with_channel(log.clone());
    let n = with.emit("local", &["slack".into()], NewNotification { kind: "failed", level: "error", flow_id: 1, run_id: None, schedule_id: None, title: "boom".into(), body: String::new(), data: Value::Null }, T0).unwrap();
    assert!(n.delivery.iter().any(|d| d.channel == "slack" && d.ok));
    assert_eq!(*log.0.lock().unwrap(), ["boom"]);
    assert!(with.channel_status().iter().any(|c| c.id == "slack" && c.connected));
}

#[test]
fn the_summary_of_a_run_is_built_from_what_actually_happened() {
    let r = rig("summary");
    let f = r.flow("Post", false);
    let run = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    let run = r.run(run.id);
    let nodes = r.runs.node_runs(run.id).unwrap();
    let s = summarize(&run, &nodes, &run.def, Some(T0 + 86_400));
    assert_eq!((s.steps_total, s.steps_succeeded, s.steps_failed, s.steps_skipped), (3, 3, 0, 0));
    assert_eq!(s.result, "Result: https://example.com/p/1");
    assert!(s.artifacts.iter().any(|(step, key, v)| step == "Publish" && key == "post_url" && v.starts_with("https://")));
    assert!(s.artifacts.iter().any(|(_, key, _)| key == "text"));
    let text = s.text(&Zone::parse("Asia/Kolkata").unwrap());
    assert!(text.contains("Next run: 2030-01-02 05:30 IST") && text.contains("Produced:"), "{text}");
    // A failed run names the failed step and the reason.
    let mut failed_nodes = nodes.clone();
    failed_nodes[1].status = NodeStatus::Failed;
    failed_nodes[1].error = "the model returned nothing".into();
    failed_nodes[2].status = NodeStatus::Skipped;
    let mut failed_run = run.clone();
    failed_run.status = RunStatus::Failed;
    let f = summarize(&failed_run, &failed_nodes, &run.def, None);
    assert_eq!((f.failed_step.as_deref(), f.steps_failed, f.steps_skipped), (Some("Write"), 1, 1));
    assert_eq!(f.result, "Stopped at \"Write\": the model returned nothing");
    let _ = &r.path;
}

// ---- the supervisor at work on scheduled runs ---------------------------------------------------------------------------------

/// A rig whose outside world is `exec`, everything else real.
fn rig_with(name: &str, exec: Arc<dyn NodeExecutor>) -> Rig {
    let mut r = rig(name);
    let runner = FlowRunner::with_clock(r.flows.clone(), r.runs.clone(), Arc::new(Executors::new().with(exec)), Arc::new(|_, c| !c.load(Ordering::SeqCst)), Arc::new(|| T0));
    let scheduler = Arc::new(Scheduler { store: r.sched.clone(), runner: runner.clone(), flows: r.flows.clone(), notifier: Arc::new(Notifier::new(r.notes.clone())) });
    runner.observe(Arc::new(RunNotifier { scheduler: scheduler.clone() }));
    r.runner = runner;
    r.scheduler = scheduler;
    r
}

/// The text step fails with `errors` in turn (then works); publishing gives proof unless `proof` is false.
struct Scripted {
    errors: std::sync::Mutex<Vec<NodeError>>,
    proof: bool,
    calls: std::sync::atomic::AtomicUsize,
}
impl Scripted {
    fn new(errors: Vec<NodeError>, proof: bool) -> Arc<Self> {
        Arc::new(Self { errors: std::sync::Mutex::new(errors), proof, calls: Default::default() })
    }
}
impl NodeExecutor for Scripted {
    fn kinds(&self) -> &[NodeKind] {
        &[NodeKind::TextGeneration, NodeKind::SocialMedia]
    }
    fn execute(&self, ctx: &NodeCtx) -> Result<NodeResult, NodeError> {
        let mut o = Map::new();
        if ctx.node.kind == NodeKind::SocialMedia {
            if self.proof {
                o.insert("post_url".into(), json!("https://example.com/p/1"));
                o.insert("status".into(), json!("published"));
            }
        } else {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut e = self.errors.lock().unwrap();
            if !e.is_empty() {
                return Err(e.remove(0));
            }
            o.insert("text".into(), json!("Generated"));
        }
        Ok(NodeResult { outputs: o, logs: vec![] })
    }
}

fn due_run(r: &Rig, s: &Schedule) {
    let at = r.get(s.id).next_run_at.unwrap() + 1;
    r.tick(at);
}

#[test]
fn an_unattended_run_retries_what_is_transient_and_says_it_recovered_but_a_run_by_hand_does_not() {
    let exec = Scripted::new(vec![NodeError::Retryable("error sending request: connection reset".into()), NodeError::Retryable("request timed out".into())], true);
    let r = rig_with("recovered", exec.clone());
    let f = r.flow("Flaky", false);
    let s = r.schedule(f, cron_input("Flaky", "0 10 * * *", "UTC"));
    due_run(&r, &s);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 3, "two transient failures, then it worked");
    let run = r.runs.list_runs("local", Some(f), 5).unwrap().remove(0);
    assert_eq!(run.status, RunStatus::Succeeded);
    let note = r.notes().into_iter().find(|n| n.kind == "recovered").expect("a recovered notice, not a plain success");
    assert!(note.title.contains("recovering") && note.body.contains("attempt 3") && note.level == "success", "{note:?}");
    assert!(r.notes().iter().all(|n| n.kind != "succeeded"), "it must not also claim a clean success");
    assert_eq!(r.get(s.id).last_status, "recovered");
    let log: Vec<String> = r.runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(log.iter().any(|m| m.contains("Network") && m.contains("supervisor")), "{log:?}");
    assert!(log.iter().any(|m| m.contains("Timeout")), "{log:?}");

    // The same failure in a run someone started by hand is not retried on their behalf.
    let exec = Scripted::new(vec![NodeError::Retryable("request timed out".into())], true);
    let r = rig_with("by_hand", exec.clone());
    let f = r.flow("Flaky", false);
    let run = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    assert_eq!(r.runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Failed);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_failure_that_will_not_fix_itself_is_not_retried_even_when_unattended() {
    let exec = Scripted::new(vec![NodeError::Retryable("401 unauthorized: session expired".into()); 5], true);
    let r = rig_with("noretry", exec.clone());
    let f = r.flow("Login", false);
    let s = r.schedule(f, cron_input("Login", "0 10 * * *", "UTC"));
    due_run(&r, &s);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 1, "a lapsed login is for a person, not for retrying");
    let fail = r.notes().into_iter().find(|n| n.kind == "failed").unwrap();
    assert!(fail.body.contains("Authentication"), "the notice names the kind of problem: {}", fail.body);
}

#[test]
fn a_run_that_finishes_without_proof_of_its_outside_action_is_not_called_a_success() {
    let r = rig_with("unverified", Scripted::new(vec![], false));
    let f = r.flow("Post", false);
    let s = r.schedule(f, cron_input("Post", "0 10 * * *", "UTC"));
    due_run(&r, &s);
    let notes = r.notes();
    let n = notes.iter().find(|n| n.kind == "unverified").expect("an honest notice");
    assert!(n.title.contains("could not confirm") && n.body.contains("no proof") && n.level == "warning", "{n:?}");
    assert!(notes.iter().all(|n| n.kind != "succeeded"));
    assert_eq!(r.get(s.id).last_status, "unverified");
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::NeedsAttention);
}

#[test]
fn three_failures_in_a_row_of_the_same_kind_switch_the_schedule_off_and_say_why() {
    let r = rig_with("repeat", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into()); 10], true));
    let f = r.flow("Locked", false);
    let s = r.schedule(f, cron_input("Locked", "0 10 * * *", "UTC"));
    for i in 1..=2 {
        due_run(&r, &s);
        assert!(r.get(s.id).enabled, "still on after failure {i}");
    }
    due_run(&r, &s);
    let after = r.get(s.id);
    assert!(!after.enabled, "switched off after the third");
    assert!(after.last_status.contains("repeated failures"));
    let stop = r.notes().into_iter().find(|n| n.title.contains("switched off")).expect("it says so");
    assert!(stop.body.contains("Authentication") && stop.body.contains("Nothing else was changed"), "{}", stop.body);
    let runs_before = r.runs.list_runs("local", Some(f), 20).unwrap().len();
    r.tick(T0 + 86_400 * 30);
    assert_eq!(r.runs.list_runs("local", Some(f), 20).unwrap().len(), runs_before, "and it no longer runs");
}

#[test]
fn different_failures_or_a_success_in_between_do_not_trip_the_stop() {
    let r = rig_with("mixed", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into()), NodeError::Fatal("missing: Topic".into()), NodeError::Fatal("401 unauthorized".into())], true));
    let f = r.flow("Mixed", false);
    let s = r.schedule(f, cron_input("Mixed", "0 10 * * *", "UTC"));
    for _ in 0..3 {
        due_run(&r, &s);
    }
    assert!(r.get(s.id).enabled, "three failures, but not the same kind");
    due_run(&r, &s); // the script is spent: this one works
    assert_eq!(r.get(s.id).last_status, "succeeded");
}

#[test]
fn a_run_marked_running_that_nothing_is_driving_is_marked_interrupted_and_reported_once() {
    let r = rig("watchdog");
    let f = r.flow("Orphan", false);
    let flow = r.flows.get("local", f).unwrap().unwrap();
    let run = r.runs.create_run("local", &flow, Map::new(), Value::Null, "schedule", false, None, T0).unwrap();
    r.runs.set_status(run.id, RunStatus::Running, "", T0).unwrap();
    let mut n = NodeRun::pending(run.id, "write");
    n.status = NodeStatus::Running;
    r.runs.save_node(&n).unwrap();
    assert_eq!(r.scheduler.watchdog(T0 + 5), super::sched::tick::WatchReport::default(), "a run that has only just started is left alone");
    assert_eq!(r.scheduler.watchdog(T0 + 120).orphaned, 1);
    let after = r.runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(after.status, RunStatus::Interrupted);
    assert!(after.error.contains("nothing was driving it"));
    assert_eq!(r.runs.node_run(run.id, "write").unwrap().unwrap().status, NodeStatus::Interrupted);
    assert_eq!(r.notes().iter().filter(|n| n.run_id == Some(run.id)).count(), 1);
    assert_eq!(r.scheduler.watchdog(T0 + 200).orphaned, 0, "it is not running any more, so it is not found again");
}

#[test]
fn an_interrupted_run_waits_to_be_resumed_but_does_not_block_every_later_time_of_its_schedule() {
    let r = rig("interrupted_blocks");
    let f = r.flow("Daily", false);
    let s = r.schedule(f, cron_input("Daily", "0 10 * * *", "UTC"));
    let flow = r.flows.get("local", f).unwrap().unwrap();
    // A run this schedule started, whose driver died.
    let dead = r.runs.create_run("local", &flow, Map::new(), serde_json::json!({ "schedule_id": s.id }), "schedule", false, None, T0).unwrap();
    r.runs.set_status(dead.id, RunStatus::Interrupted, "the run stopped without finishing", T0).unwrap();
    assert!(r.scheduler.store.active_run_of("local", s.id).unwrap().is_none(), "interrupted is not 'still going'");
    let before = r.runs.list_runs("local", Some(f), 20).unwrap().len();
    due_run(&r, &s);
    let after = r.runs.list_runs("local", Some(f), 20).unwrap();
    assert_eq!(after.len(), before + 1, "the next time ran instead of being skipped behind the dead run");
    assert_ne!(r.get(s.id).last_status, "skipped: the previous run (#1) is still interrupted");
    // A run that really is going still holds the schedule back.
    let live = r.runs.create_run("local", &flow, Map::new(), serde_json::json!({ "schedule_id": s.id }), "schedule", false, None, T0).unwrap();
    r.runs.set_status(live.id, RunStatus::Running, "", T0).unwrap();
    assert_eq!(r.scheduler.store.active_run_of("local", s.id).unwrap().map(|(id, _)| id), Some(live.id));
}

// ---- recovery and what is learned from it ---------------------------------------------------------------------------------------

use super::supervisor::recovery::{self, CARRY_ON, EXTRA_PATIENCE, MORE_RETRIES};
use super::supervisor::ErrorClass;

/// `rig_with` plus the learner, as the server wires it.
fn learning_rig(name: &str, exec: Arc<dyn NodeExecutor>) -> Rig {
    let r = rig_with(name, exec);
    r.runner.observe(Arc::new(recovery::RecoveryLearner { runs: r.runs.clone() }));
    r
}

fn failed_run(r: &Rig, flow: i64) -> Run {
    let run = r.runner.start("local", flow, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    let run = r.runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Failed, "{}", run.error);
    run
}

fn net() -> NodeError {
    NodeError::Retryable("error sending request: connection reset".into())
}

#[test]
fn what_is_learned_from_outcomes_is_smoothed_trusted_slowly_and_retired_when_it_stops_working() {
    use crate::flows::run::recovery_store::Knowledge;
    let one = Knowledge::new("k", "network", 1, 0);
    assert_eq!((one.state, (one.confidence * 100.0).round()), ("candidate", 67.0), "one lucky run is not certainty");
    assert_eq!(Knowledge::new("k", "network", 2, 0).state, "candidate");
    assert_eq!(Knowledge::new("k", "network", 3, 0).state, "trusted");
    assert_eq!(Knowledge::new("k", "network", 3, 3).state, "candidate", "a coin flip is not trusted");
    assert_eq!(Knowledge::new("k", "network", 5, 1).state, "trusted");
    assert_eq!(Knowledge::new("k", "network", 0, 2).state, "retired");
    assert_eq!(Knowledge::new("k", "network", 1, 3).state, "retired");
}

#[test]
fn fixes_are_offered_only_for_failures_they_can_help_and_permanent_ones_always_need_approval() {
    let r = rig("suggest");
    let f = r.flow("Flow", false);
    let flow = r.flows.get("local", f).unwrap().unwrap();
    let write = flow.def.node("write").unwrap();
    let keys = |c| recovery::suggest(&r.runs, c, write).into_iter().map(|s| (s.key, s.kind, s.needs_approval)).collect::<Vec<_>>();
    assert_eq!(keys(ErrorClass::Network), vec![(EXTRA_PATIENCE, "temporary", false), (MORE_RETRIES, "permanent", true), (CARRY_ON, "permanent", true)]);
    assert_eq!(keys(ErrorClass::Authentication), vec![(CARRY_ON, "permanent", true)], "retrying a wrong login is not offered");
    assert!(recovery::guidance(ErrorClass::Authentication).unwrap().contains("never stores"));
    assert!(recovery::guidance(ErrorClass::Network).is_none());
    // A fix that has stopped working is no longer offered.
    for _ in 0..2 {
        r.runs.kb_record(MORE_RETRIES, "network", false, T0).unwrap();
    }
    assert_eq!(keys(ErrorClass::Network).iter().map(|k| k.0).collect::<Vec<_>>(), vec![EXTRA_PATIENCE, CARRY_ON]);
    let logic = recovery::suggest(&r.runs, ErrorClass::Network, write).into_iter().find(|s| s.key == CARRY_ON).unwrap();
    assert!(logic.changes_logic && logic.why.contains("does not depend"), "a change of logic says so");
}

#[test]
fn an_approved_fix_becomes_a_new_version_is_checked_by_the_next_run_and_teaches_the_knowledge_base() {
    let exec = Scripted::new(vec![net()], true);
    let r = learning_rig("apply", exec.clone());
    let f = r.flow("Flaky", false);
    let run = failed_run(&r, f);
    assert_eq!(r.flows.get("local", f).unwrap().unwrap().def.node("write").unwrap().retry.retries, 0);

    // Not offered, not applied.
    assert!(recovery::apply(&r.flows, &r.runs, "local", &run, "write", "made_up", T0).unwrap_err().contains("not offered"));
    assert!(recovery::apply(&r.flows, &r.runs, "local", &run, "start", MORE_RETRIES, T0).unwrap_err().contains("did not fail"));
    assert!(recovery::apply(&r.flows, &r.runs, "local", &run, "write", EXTRA_PATIENCE, T0).unwrap_err().contains("not applied to the flow"));

    let before = r.flows.get("local", f).unwrap().unwrap().version;
    let (flow, rec) = recovery::apply(&r.flows, &r.runs, "local", &run, "write", MORE_RETRIES, T0 + 1).unwrap();
    assert_eq!(flow.version, before + 1, "a new version, so it can be undone");
    assert_eq!(flow.def.node("write").unwrap().retry.retries, 3);
    assert_eq!((rec.status.as_str(), rec.outcome.as_str(), rec.class.as_str(), rec.version_before, rec.version_after), ("applied", "pending", "network", before, before + 1));
    let audit = r.flows.audit_log("local", Some(f), 20).unwrap();
    assert!(audit.iter().any(|a| a.actor == "supervisor" && a.detail.contains("Recovery")), "the change is in the audit trail: {audit:?}");

    // The next run uses the new version and works: the fix is recorded as having worked.
    let again = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    assert_eq!(r.runs.get_run("local", again.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(r.runs.recovery_get("local", rec.id).unwrap().unwrap().outcome, "worked");
    let k = r.runs.kb_get(MORE_RETRIES, "network").unwrap();
    assert_eq!((k.successes, k.failures, k.state), (1, 0, "candidate"), "one success is not yet trust");
}

#[test]
fn a_fix_that_did_not_help_is_recorded_as_such_and_can_be_undone_but_not_over_someones_edit() {
    let r = learning_rig("undo", Scripted::new(vec![net(); 20], true));
    let f = r.flow("Hopeless", false);
    let run = failed_run(&r, f);
    let (_, rec) = recovery::apply(&r.flows, &r.runs, "local", &run, "write", MORE_RETRIES, T0 + 1).unwrap();
    // Same failure again, at the same step, after the fix.
    let again = r.runner.start("local", f, Map::new(), Value::Null, false, "manual").unwrap();
    r.settle();
    assert_eq!(r.runs.get_run("local", again.id).unwrap().unwrap().status, RunStatus::Failed);
    assert_eq!(r.runs.recovery_get("local", rec.id).unwrap().unwrap().outcome, "did_not_help");
    assert_eq!(r.runs.kb_get(MORE_RETRIES, "network").unwrap().failures, 1);

    let restored = recovery::rollback(&r.flows, &r.runs, "local", rec.id, T0 + 2).unwrap();
    assert_eq!(restored.def.node("write").unwrap().retry.retries, 0, "back as it was");
    assert_eq!(restored.version, rec.version_after + 1, "the undo is itself a version");
    assert!(recovery::rollback(&r.flows, &r.runs, "local", rec.id, T0 + 3).unwrap_err().contains("already been undone"));
    assert_eq!(r.runs.recovery_get("local", rec.id).unwrap().unwrap().status, "rolled_back");

    // A person edited the flow after a fix: it is not undone over their work.
    let run2 = failed_run(&r, f);
    let (flow, rec2) = recovery::apply(&r.flows, &r.runs, "local", &run2, "write", CARRY_ON, T0 + 4).unwrap();
    let mut d = flow.def.clone();
    d.description = "edited by a person".into();
    r.flows.update("local", f, d, "user", "", None, T0 + 5).unwrap();
    assert!(recovery::rollback(&r.flows, &r.runs, "local", rec2.id, T0 + 6).unwrap_err().contains("edited since"));
}

#[test]
fn extra_patience_is_used_by_an_unattended_run_only_after_it_has_proven_itself() {
    // Not trusted: the ladder's three retries are all there is.
    let exec = Scripted::new(vec![net(); 4], true);
    let r = learning_rig("patience_no", exec.clone());
    let f = r.flow("Flaky", false);
    let s = r.schedule(f, cron_input("Flaky", "0 10 * * *", "UTC"));
    due_run(&r, &s);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 4, "the first try and three retries, then it stops");
    assert_eq!(r.get(s.id).last_status, "failed");

    // Trusted (worked three times before): one more, longer wait, said in the log.
    let exec = Scripted::new(vec![net(); 4], true);
    let r = learning_rig("patience_yes", exec.clone());
    for _ in 0..3 {
        r.runs.kb_record(EXTRA_PATIENCE, "network", true, T0).unwrap();
    }
    let f = r.flow("Flaky", false);
    let s = r.schedule(f, cron_input("Flaky", "0 10 * * *", "UTC"));
    due_run(&r, &s);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 5);
    assert_eq!(r.get(s.id).last_status, "recovered");
    let run = r.runs.list_runs("local", Some(f), 1).unwrap().remove(0);
    let log: Vec<String> = r.runs.events("local", run.id, 0, 100).unwrap().into_iter().filter(|e| e.kind == "temp_fix").map(|e| e.message).collect();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("longer wait") && log[0].contains("has worked before"));
    let k = r.runs.kb_get(EXTRA_PATIENCE, "network").unwrap();
    assert_eq!((k.successes, k.failures), (4, 0), "the run that used it taught it");

    // A different kind of failure never borrows that trust.
    let exec = Scripted::new(vec![NodeError::Retryable("request timed out".into()); 4], true);
    let r2 = learning_rig("patience_other", exec.clone());
    for _ in 0..3 {
        r2.runs.kb_record(EXTRA_PATIENCE, "network", true, T0).unwrap();
    }
    let f2 = r2.flow("Flaky", false);
    let s2 = r2.schedule(f2, cron_input("Flaky", "0 10 * * *", "UTC"));
    due_run(&r2, &s2);
    assert_eq!(exec.calls.load(Ordering::SeqCst), 4);
}

// ---- health, history, errors and asking ------------------------------------------------------------------------------------------

use super::supervisor::assistant::{self, Ctx};
use super::supervisor::health::{error_groups, health_of, history, judged_runs, HealthState, WINDOW};

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

fn hand_runs(r: &Rig, flow: i64, n: usize) {
    for _ in 0..n {
        r.runner.start("local", flow, Map::new(), Value::Null, false, "manual").unwrap();
        r.settle();
    }
}

fn ctx(r: &Rig) -> Ctx<'_> {
    Ctx { flows: &r.flows, runs: &r.runs, sched: &r.sched, scheduler: Some(&r.scheduler), owner: "local", now: now_ts() }
}

fn ask(r: &Rig, text: &str) -> assistant::Reply {
    assistant::answer(&ctx(r), text).unwrap_or_else(|| panic!("not understood: {text}"))
}

#[test]
fn health_is_judged_from_the_recent_runs_and_says_why() {
    // Nothing yet.
    let r = rig_with("h_idle", Scripted::new(vec![], true));
    let f = r.flow("Quiet", false);
    let h = health_of("Quiet", f, &judged_runs(&r.runs, "local", f, WINDOW).unwrap());
    assert_eq!((h.state, h.score), (HealthState::Idle, None));
    assert!(h.reasons[0].contains("nothing to judge"));

    // All good.
    hand_runs(&r, f, 5);
    let h = health_of("Quiet", f, &judged_runs(&r.runs, "local", f, WINDOW).unwrap());
    assert_eq!((h.state, h.score, h.succeeded, h.failure_streak), (HealthState::Healthy, Some(100), 5, 0));

    // One old failure, since fixed: still a little shaky, and it says what the trouble was.
    let r = rig_with("h_degraded", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into())], true));
    let f = r.flow("Shaky", false);
    hand_runs(&r, f, 5);
    let h = health_of("Shaky", f, &judged_runs(&r.runs, "local", f, WINDOW).unwrap());
    assert_eq!(h.state, HealthState::Degraded, "{h:?}");
    assert!(h.score.unwrap() > 60 && h.score.unwrap() < 90, "{h:?}");
    assert_eq!((h.failed, h.failure_streak, h.top_problem.clone()), (1, 0, Some(("Authentication".to_string(), 1))));
    assert!(h.last_success_at >= h.last_run_at - 5);

    // Failing right now.
    let r = rig_with("h_failing", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into()); 3], true));
    let f = r.flow("Broken", false);
    hand_runs(&r, f, 3);
    let h = health_of("Broken", f, &judged_runs(&r.runs, "local", f, WINDOW).unwrap());
    assert_eq!((h.state, h.score, h.failure_streak), (HealthState::Failing, Some(0), 3));
    assert!(h.reasons.iter().any(|x| x.contains("3 runs in a row")) && h.reasons.iter().any(|x| x.contains("Authentication")), "{:?}", h.reasons);

    // A publish with no proof is not counted as clean.
    let r = rig_with("h_unproven", Scripted::new(vec![], false));
    let f = r.flow("Unproven", false);
    hand_runs(&r, f, 2);
    let h = health_of("Unproven", f, &judged_runs(&r.runs, "local", f, WINDOW).unwrap());
    assert_eq!((h.unverified, h.score), (2, Some(50)));
    assert!(h.reasons.iter().any(|x| x.contains("without proof")));
}

#[test]
fn the_history_counts_each_day_and_the_error_dashboard_groups_by_kind() {
    let r = rig_with("hist", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into()), NodeError::Fatal("No such file or directory (os error 2)".into()), NodeError::Fatal("401 unauthorized".into())], true));
    let a = r.flow("Alpha", false);
    let b = r.flow("Beta", false);
    hand_runs(&r, a, 2);
    hand_runs(&r, b, 3);
    let ja = judged_runs(&r.runs, "local", a, WINDOW).unwrap();
    let jb = judged_runs(&r.runs, "local", b, WINDOW).unwrap();
    let days = history(&ja, now_ts(), 14);
    assert_eq!(days.len(), 14);
    assert!(days.windows(2).all(|w| w[0].starts_at < w[1].starts_at), "oldest first");
    let today = days.last().unwrap();
    assert_eq!((today.failed, today.success), (2, 0), "{today:?}");
    assert!(days[..13].iter().all(|d| d.failed + d.success + d.recovered + d.unverified == 0));

    let groups = error_groups(&[("Alpha".into(), ja), ("Beta".into(), jb)], now_ts() - 86_400);
    assert_eq!((groups[0].label, groups[0].count), ("Authentication", 2), "most frequent first");
    assert_eq!((groups[1].label, groups[1].count), ("Missing file", 1));
    assert_eq!(groups[0].flows.len(), 2, "which flows it touched: {:?}", groups[0].flows);
    assert!(groups[0].last_run_id > 0);
    assert!(error_groups(&[], 0).is_empty());
    assert!(error_groups(&[("x".into(), vec![])], 0).is_empty());
}

#[test]
fn asked_whether_a_scheduled_workflow_ran_it_answers_from_the_record_including_how_it_ended() {
    let r = rig_with("q_ran", Scripted::new(vec![], true));
    let f = r.flow("Daily post", false);
    let other = r.flow("Weekly report", false);
    let s = r.schedule(f, cron_input("Daily post", "0 10 * * *", "UTC"));
    let before = ask(&r, "Did my scheduled workflow run?").text;
    assert!(before.contains("nothing has come due yet") && before.contains("Next:"), "{before}");
    due_run(&r, &s);
    let a = ask(&r, "Did my Daily post workflow run today?").text;
    assert!(a.contains("Yes") && a.contains("and it worked") && a.contains("run #"), "{a}");
    assert!(ask(&r, "did the weekly report run?").text.contains("has no schedule"), "a flow with no schedule says so");
    let _ = other;
    // Not run because Aqua was off: the answer says why.
    let r2 = rig_with("q_missed", Scripted::new(vec![], true));
    let f2 = r2.flow("Daily post", false);
    let s2 = r2.schedule(f2, cron_input("Daily post", "0 10 * * *", "UTC"));
    let mut sch = r2.get(s2.id);
    sch.misfire = Misfire::Skip;
    r2.sched.update("local", s2.id, &ScheduleInput { name: sch.name, when: sch.when, timezone: sch.timezone, inputs: sch.inputs, misfire: Misfire::Skip, notify: sch.notify, enabled: true, campaign: Default::default() }, T0).unwrap();
    let due = r2.get(s2.id).next_run_at.unwrap();
    r2.tick(due + 3 * 3600);
    let a = ask(&r2, "Why didn't my daily post run?").text;
    assert!(a.contains("No") && a.contains("skipped") && a.contains("missed"), "{a}");
}

#[test]
fn fix_the_last_error_explains_it_and_only_proposes_what_a_person_must_approve() {
    // A failure only a person can fix: guidance, no proposal to change the flow's retries.
    let r = rig_with("q_fix_auth", Scripted::new(vec![NodeError::Fatal("401 unauthorized: session expired".into())], true));
    let f = r.flow("Login flow", false);
    hand_runs(&r, f, 1);
    let a = ask(&r, "Fix the last error");
    assert!(a.text.contains("Authentication") && a.text.contains("Sign in") && a.text.contains("Login flow"), "{}", a.text);
    assert!(a.proposals.iter().all(|p| p.key == "carry_on"), "no retry is offered for a lapsed login: {:?}", a.proposals);
    assert!(a.did.is_empty(), "nothing was changed");
    assert_eq!(r.flows.get("local", f).unwrap().unwrap().version, 1, "and the flow is untouched");

    // A transient failure: proposals are named, and nothing is applied.
    let r = rig_with("q_fix_net", Scripted::new(vec![net()], true));
    let f = r.flow("Flaky", false);
    hand_runs(&r, f, 1);
    let a = ask(&r, "fix the last failure");
    let keys: Vec<&str> = a.proposals.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, vec!["more_retries", "carry_on"], "{}", a.text);
    assert!(a.text.contains("nothing is changed until then") && a.text.contains("changes what the flow does"), "{}", a.text);
    assert_eq!(r.flows.get("local", f).unwrap().unwrap().version, 1);

    // Once the flow has worked since, there is nothing to fix.
    hand_runs(&r, f, 1);
    assert!(ask(&r, "Fix the last error").text.contains("Nothing to fix"));
}

#[test]
fn check_my_workflows_reports_every_problem_does_only_the_safe_thing_and_leaves_the_rest_to_approval() {
    let r = rig_with("q_all", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into()); 3], true));
    let bad = r.flow("Broken poster", false);
    let good = r.flow("Fine one", false);
    hand_runs(&r, bad, 3);
    hand_runs(&r, good, 2);
    // A run that nothing is driving.
    let flow = r.flows.get("local", good).unwrap().unwrap();
    let orphan = r.runs.create_run("local", &flow, Map::new(), Value::Null, "manual", false, None, now_ts() - 600).unwrap();
    r.runs.set_status(orphan.id, RunStatus::Running, "", now_ts() - 600).unwrap();
    // A schedule switched off after repeated failures.
    let s = r.schedule(bad, cron_input("Broken poster", "0 10 * * *", "UTC"));
    r.sched.set_enabled("local", s.id, false, T0).unwrap();
    r.sched.set_last_status(s.id, 1, "failed: switched off after repeated failures").unwrap();

    let a = ask(&r, "Check my workflows and fix any problems you can");
    assert!(a.text.contains("Broken poster") && a.text.contains("failing"), "{}", a.text);
    assert!(!a.text.contains("Fine one:"), "a healthy workflow is not listed as a problem: {}", a.text);
    assert_eq!(a.did.len(), 1, "{:?}", a.did);
    assert!(a.did[0].contains("stuck"));
    assert_eq!(r.runs.get_run("local", orphan.id).unwrap().unwrap().status, RunStatus::Interrupted);
    assert!(a.text.contains("none are made until then") && a.text.contains("Only you can do these"), "{}", a.text);
    assert!(a.text.contains("switched off after repeated failures") && a.text.contains("will not switch it back on"), "{}", a.text);
    assert!(!r.get(s.id).enabled, "it does not re-enable a schedule on its own");
    assert_eq!(r.flows.get("local", bad).unwrap().unwrap().version, 1, "no flow was changed");

    // When everything is fine, it says so.
    let r = rig_with("q_all_ok", Scripted::new(vec![], true));
    let f = r.flow("Fine", false);
    hand_runs(&r, f, 2);
    assert!(ask(&r, "Check my workflows and fix any problems you can").text.contains("Nothing needs fixing"));
}

#[test]
fn status_and_error_questions_are_answered_and_other_talk_is_left_alone() {
    let r = rig_with("q_status", Scripted::new(vec![NodeError::Fatal("403 forbidden".into()); 2], true));
    let f = r.flow("Poster", false);
    hand_runs(&r, f, 2);
    let s = ask(&r, "How are my workflows doing? Any problems?").text;
    assert!(s.contains("Poster") && s.contains("failing") && s.contains("0/100"), "{s}");
    let e = ask(&r, "what errors happened lately?").text;
    assert!(e.contains("2 × Permission") && e.contains("Poster"), "{e}");
    for chat in ["hello there", "write a poem about cats", "what is the capital of France"] {
        assert!(assistant::answer(&ctx(&r), chat).is_none(), "{chat}");
    }
    let empty = rig_with("q_none", Scripted::new(vec![], true));
    assert!(ask(&empty, "how are my workflows").text.contains("no workflows"));
}

// ---- campaigns: schedules that stop by themselves --------------------------------------------------------------------------------------

fn campaign(max_runs: Option<u32>, end_on: Option<&str>, series: Vec<Value>) -> Campaign {
    Campaign { end_on: end_on.map(String::from), max_runs, series: series.into_iter().map(|v| v.as_object().cloned().unwrap()).collect() }
}

fn daily(name: &str, c: Campaign) -> ScheduleInput {
    ScheduleInput { campaign: c, ..cron_input(name, "0 10 * * *", "UTC") }
}

#[test]
fn a_campaign_stops_after_its_number_of_runs_and_says_how_it_went() {
    let r = rig_with("camp_max", Scripted::new(vec![NodeError::Fatal("401 unauthorized".into())], true));
    let f = r.flow("Weekly post", false);
    let s = r.schedule(f, daily("Three posts", campaign(Some(3), None, vec![])));
    assert_eq!(s.campaign.max_runs, Some(3));
    for i in 1..=3 {
        due_run(&r, &s);
        let now = r.get(s.id);
        assert_eq!(now.runs_started, i, "run {i} counted");
        assert_eq!(now.enabled, i < 3, "still on until the last run has been started");
    }
    let done = r.get(s.id);
    assert_eq!((done.next_run_at, done.enabled), (None, false), "no fourth run is scheduled");
    assert_eq!(r.scheduler.views("local", None).unwrap()[0].state, State::Completed, "it reads as finished, not paused");
    let end: Vec<Notification> = r.notes().into_iter().filter(|n| n.kind == "campaign").collect();
    assert_eq!(end.len(), 1, "one closing notice: {:?}", r.notes().iter().map(|n| n.kind.clone()).collect::<Vec<_>>());
    assert!(end[0].title.contains("Three posts") && end[0].title.contains("finished"), "{}", end[0].title);
    assert!(end[0].body.contains("3 of 3 runs started") && end[0].body.contains("2 worked") && end[0].body.contains("1 failed"), "{}", end[0].body);
    assert_eq!(end[0].level, "warning", "a failure in it makes the closing notice a warning");
    // Nothing more happens, however long it is left.
    let runs = r.runs.list_runs("local", Some(f), 20).unwrap().len();
    r.tick(T0 + 86_400 * 30);
    assert_eq!(r.runs.list_runs("local", Some(f), 20).unwrap().len(), runs);
    let e = r.sched.set_enabled("local", s.id, true, T0 + 86_400 * 31).unwrap_err();
    assert!(e.contains("made all its runs"), "{e}");
}

#[test]
fn a_campaign_stops_after_its_last_day_including_that_whole_day() {
    let r = rig_with("camp_end", Scripted::new(vec![], true));
    let f = r.flow("Daily", false);
    // T0 is 2030-01-01 00:00 UTC; the runs are at 10:00, so 2030-01-01, 02 and 03 are inside "until the 3rd".
    let s = r.schedule(f, daily("Three days", campaign(None, Some("2030-01-03"), vec![])));
    assert_eq!(s.ends_at, Some(ts("2030-01-03T23:59:59Z")), "the whole last day counts");
    for _ in 0..3 {
        due_run(&r, &s);
    }
    let done = r.get(s.id);
    assert_eq!((done.runs_started, done.next_run_at, done.enabled), (3, None, false), "{done:?}");
    assert_eq!(r.notes().iter().filter(|n| n.kind == "campaign").count(), 1);
    // The first run of a schedule that is already past its last day is refused when it is made.
    let e = r.sched.create("local", f, &daily("Too late", campaign(None, Some("2029-12-31"), vec![])), T0).unwrap_err();
    assert!(e.contains("already passed"), "{e}");
}

#[test]
fn different_inputs_take_turns_and_start_again_from_the_first() {
    let r = rig_with("camp_series", Scripted::new(vec![], true));
    let f = r.flow("Topics", false);
    let series = vec![json!({ "topic": "agents" }), json!({}), json!({ "topic": "browsers" })];
    let input = ScheduleInput { inputs: [("topic".to_string(), json!("the schedule's own"))].into_iter().collect(), ..daily("Rotating", campaign(Some(5), None, series)) };
    let s = r.schedule(f, input);
    for _ in 0..5 {
        due_run(&r, &s);
    }
    let mut runs = r.runs.list_runs("local", Some(f), 10).unwrap();
    runs.reverse();
    let topics: Vec<String> = runs.iter().map(|x| x.vars["topic"].as_str().unwrap().to_string()).collect();
    assert_eq!(topics, ["agents", "the schedule's own", "browsers", "agents", "the schedule's own"], "in turn, then round again; an empty entry leaves the schedule's own input");
}

#[test]
fn a_time_that_was_skipped_or_missed_does_not_use_up_a_run() {
    let r = rig_with("camp_missed", Scripted::new(vec![], true));
    let f = r.flow("Careful", false);
    let s = r.schedule(f, ScheduleInput { misfire: Misfire::Skip, ..daily("Two runs", campaign(Some(2), None, vec![])) });
    let due = r.get(s.id).next_run_at.unwrap();
    r.tick(due + 3 * 3600); // three hours late with a "skip" rule: skipped, not run
    let after = r.get(s.id);
    assert_eq!((after.runs_started, after.enabled), (0, true), "nothing was run, so nothing was counted");
    due_run(&r, &s);
    due_run(&r, &s);
    let done = r.get(s.id);
    assert_eq!((done.runs_started, done.enabled), (2, false), "both real runs happened after the skipped day");
    let view = r.scheduler.campaign("local", s.id, T0 + 86_400 * 10).unwrap();
    assert_eq!((view.skipped, view.worked, view.total_planned), (1, 2, Some(2)), "{}", view.text);
}

#[test]
fn the_campaign_view_counts_how_runs_ended_and_how_many_are_still_to_come() {
    let r = rig_with("camp_view", Scripted::new(vec![NodeError::Fatal("403 forbidden".into())], false));
    let f = r.flow("Post", false);
    let s = r.schedule(f, daily("Ten posts", campaign(Some(10), None, vec![])));
    due_run(&r, &s);
    due_run(&r, &s);
    let v = r.scheduler.campaign("local", s.id, T0 + 86_400 * 2).unwrap();
    assert_eq!((v.runs_started, v.total_planned, v.remaining), (2, Some(10), 8));
    assert_eq!((v.failed, v.unverified, v.worked), (1, 1, 0), "the first failed, the second finished with nothing to prove it: {}", v.text);
    assert!(v.text.starts_with("2 of 10 runs started") && v.text.contains("1 failed") && v.text.contains("could not be confirmed") && v.text.contains("next "), "{}", v.text);
    assert!(!v.over);
    // A schedule with no limits says so plainly.
    let open = r.schedule(f, cron_input("Forever", "0 11 * * *", "UTC"));
    let ov = r.scheduler.campaign("local", open.id, T0).unwrap();
    assert_eq!((ov.total_planned, ov.over), (None, false));
    assert!(ov.remaining >= 1, "there is always a next one");
}

#[test]
fn campaign_settings_are_checked_before_anything_is_stored() {
    let r = rig("camp_valid");
    let f = r.flow("Flow", false);
    let bad = |c: Campaign, needle: &str| {
        let e = r.sched.create("local", f, &daily("Bad", c), T0).unwrap_err();
        assert!(e.contains(needle), "{needle}: {e}");
    };
    bad(campaign(Some(0), None, vec![]), "from 1 to 1000");
    bad(campaign(Some(1001), None, vec![]), "from 1 to 1000");
    bad(campaign(None, Some("soon"), vec![]), "not a date");
    bad(campaign(None, Some("2029-06-01"), vec![]), "already passed");
    bad(campaign(None, None, (0..201).map(|i| json!({ "n": i })).collect()), "at most 200");
    let once = ScheduleInput { when: When::Once { when: "2030-02-01 09:00".into() }, campaign: campaign(Some(3), None, vec![]), ..cron_input("Once", "", "UTC") };
    assert!(r.sched.create("local", f, &once, T0).unwrap_err().contains("nothing to repeat"));
    // A last day before the first run would mean it never runs.
    let weekly = ScheduleInput { when: When::Cron { cron: "0 10 * * 5".into() }, campaign: campaign(None, Some("2030-01-02"), vec![]), ..cron_input("Fri", "", "UTC") };
    assert!(r.sched.create("local", f, &weekly, T0).unwrap_err().contains("never run"));
    assert!(r.sched.list("local", None).unwrap().is_empty(), "none of them was stored");
}

#[test]
fn a_sentence_can_say_how_long_it_goes_on() {
    let now = chrono::NaiveDate::from_ymd_opt(2030, 1, 1).unwrap().and_hms_opt(8, 0, 0).unwrap();
    let l = |t: &str| super::schedule_text::limits(t, now);
    assert_eq!(l("every Monday at 9 for 4 weeks").end_on.as_deref(), Some("2030-01-29"));
    assert_eq!(l("every day at 10 for a month").end_on.as_deref(), Some("2030-02-01"));
    assert_eq!(l("every day at 10 for 10 days").end_on.as_deref(), Some("2030-01-11"));
    assert_eq!(l("every weekday at 9, eight times").max_runs, Some(8));
    assert_eq!(l("every day at 9 for 3 months, 20 times"), super::schedule_text::Limits { max_runs: Some(20), end_on: Some("2030-04-01".into()), summary: "for 3 months (until 2030-04-01), 20 times".into() });
    assert_eq!(l("every Friday until 2030-03-15").end_on.as_deref(), Some("2030-03-15"));
    assert_eq!(l("every Friday until March 15").end_on.as_deref(), Some("2030-03-15"));
    for none in ["every day at 10 AM", "every Monday at 9", "tomorrow at 3 pm", "every day at 10 for the team"] {
        assert_eq!(l(none), super::schedule_text::Limits::default(), "{none}");
    }
    assert_eq!(l("every day for 0 days").end_on, None);
}

#[test]
fn schedules_made_before_campaigns_existed_are_still_there_and_still_run() {
    let dir = std::env::temp_dir().join(format!("aqua_sched_migrate_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flows.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE flow_schedules (id INTEGER PRIMARY KEY AUTOINCREMENT, owner_id TEXT NOT NULL, flow_id INTEGER NOT NULL, name TEXT NOT NULL, when_json TEXT NOT NULL, timezone TEXT NOT NULL DEFAULT '', inputs_json TEXT NOT NULL DEFAULT '{}', misfire TEXT NOT NULL DEFAULT 'ask', notify_json TEXT NOT NULL DEFAULT '{}', enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, next_run_at INTEGER, last_run_at INTEGER NOT NULL DEFAULT 0, last_run_id INTEGER, last_status TEXT NOT NULL DEFAULT '');
            INSERT INTO flow_schedules (owner_id, flow_id, name, when_json, timezone, created_at, updated_at, next_run_at) VALUES ('local', 1, 'Old one', '{\"type\":\"cron\",\"cron\":\"0 9 * * *\"}', 'UTC', 1, 1, 1893488400);").unwrap();
    }
    let store = SchedStore::new(&path);
    let s = store.get("local", 1).unwrap().expect("the old schedule is still there");
    assert_eq!((s.name.as_str(), s.runs_started, s.campaign.is_set(), s.ends_at), ("Old one", 0, false, None));
    assert_eq!(s.next_within_campaign(1_893_488_400, true), s.next_after(1_893_488_400), "no limits: it goes on as before");
    let again = SchedStore::new(&path);
    assert_eq!(again.list("local", None).unwrap().len(), 1, "opening it again does not add the columns twice");
}


// ---- keys for the start of a run, and what is checked before a scheduled one ----------------------------------------------

fn topic_inputs(t: &str) -> Map<String, Value> {
    [("topic".to_string(), json!(t))].into_iter().collect()
}

#[test]
fn a_start_with_a_key_that_started_a_run_gives_that_run_back_and_makes_no_second_one() {
    use super::run::store::KEY_MISMATCH;
    let r = rig("keyed_start");
    let f = r.flow("Keyed", false);
    let g = r.flow("Other flow", false);
    let start = |flow: i64, topic: &str, key: Option<&str>| r.runner.start_keyed("local", flow, topic_inputs(topic), Value::Null, false, "manual", key.map(|k| (k, topic)));
    let (a, dup) = start(f, "cats", Some("order-1")).unwrap();
    assert!(!dup);
    // A retry (the caller never heard the answer): the same run, nothing new.
    let (b, dup) = start(f, "cats", Some("order-1")).unwrap();
    assert_eq!((b.id, dup), (a.id, true));
    r.settle();
    assert_eq!(r.runs.list_runs("local", Some(f), 50).unwrap().len(), 1, "one run");
    // The same key for another request is refused, and makes nothing.
    assert_eq!(start(f, "dogs", Some("order-1")).unwrap_err(), KEY_MISMATCH);
    assert_eq!(r.runs.list_runs("local", Some(f), 50).unwrap().len(), 1);
    // Another key, no key, and the same key on another flow each make a run of their own.
    assert!(!start(f, "cats", Some("order-2")).unwrap().1);
    assert!(!start(f, "cats", None).unwrap().1);
    assert!(!start(g, "cats", Some("order-1")).unwrap().1, "a key belongs to a flow");
    r.settle();
    assert_eq!(r.runs.list_runs("local", Some(f), 50).unwrap().len(), 3);
    // The run was cleared away with the finished ones: the key is free again.
    r.runs.prune(0, i64::MAX).unwrap();
    assert!(r.runs.list_runs("local", Some(f), 50).unwrap().is_empty());
    let (c, dup) = start(f, "cats", Some("order-1")).unwrap();
    assert!(!dup && c.id != a.id, "a key whose run is gone starts a new one");
    r.settle();
}

#[test]
fn two_starts_with_one_key_at_the_same_moment_make_one_run() {
    let r = rig("keyed_race");
    let f = r.flow("Race", false);
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let runner = r.runner.clone();
            std::thread::spawn(move || runner.start_keyed("local", f, topic_inputs("cats"), Value::Null, false, "manual", Some(("race-1", "cats"))).unwrap())
        })
        .collect();
    let results: Vec<(Run, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    r.settle();
    assert_eq!(results.iter().filter(|(_, dup)| !dup).count(), 1, "one made it");
    assert!(results.iter().all(|(run, _)| run.id == results[0].0.id));
    assert_eq!(r.runs.list_runs("local", Some(f), 50).unwrap().len(), 1);
}

#[test]
fn a_time_that_was_started_but_not_written_down_does_not_start_a_second_run_when_it_comes_due_again() {
    let r = rig("lost_advance");
    let f = r.flow("Daily", false);
    let s = r.schedule(f, cron_input("Daily", "0 10 * * *", "UTC"));
    let due = s.next_run_at.unwrap();
    assert_eq!(r.tick(due + 1).started, 1);
    let first = r.occurrences(s.id)[0].run_id.unwrap();
    // The write that moved the schedule on was lost (the database was busy): the same time is due again.
    r.sched.advance(s.id, Some(due), 0, None, "write lost").unwrap();
    assert_eq!(r.get(s.id).next_run_at, Some(due));
    let report = r.tick(due + 15);
    assert_eq!((report.started, report.failed, report.skipped), (0, 0, 0), "nothing was started or said again");
    assert_eq!(r.runs.list_runs("local", Some(f), 50).unwrap().len(), 1, "one run for the time");
    let after = r.get(s.id);
    assert_eq!((after.next_run_at, after.last_run_id), (Some(due + 24 * 3600), Some(first)), "and the schedule went on to its next time");
    assert_eq!(r.occurrences(s.id).len(), 1, "one record of the time");
}

#[test]
fn a_scheduled_run_that_is_certain_to_fail_for_want_of_disk_is_skipped_and_said_not_tried_and_the_schedule_goes_on() {
    use super::sched::preflight::FREE_FOR_TEST;
    let r = rig("preflight");
    let f = r.flow("Daily", false);
    let s = r.schedule(f, cron_input("Daily", "0 10 * * *", "UTC"));
    let due = s.next_run_at.unwrap();
    FREE_FOR_TEST.with(|c| c.set(Some(50 * 1024 * 1024)));
    let report = r.tick(due + 1);
    FREE_FOR_TEST.with(|c| c.set(None));
    assert_eq!((report.started, report.failed, report.skipped), (0, 0, 1), "skipped, not failed");
    assert!(r.runs.list_runs("local", Some(f), 50).unwrap().is_empty(), "no run was made");
    let occ = &r.occurrences(s.id)[0];
    assert_eq!(occ.outcome, "skipped");
    assert!(occ.reason.starts_with("not started: ") && occ.reason.contains("only 50 MB free"), "{}", occ.reason);
    assert_eq!(r.kinds(), ["missed"], "said as a skipped time, not as a failure");
    assert!(r.notes()[0].body.contains("it will try again at its next time"), "{}", r.notes()[0].body);
    let after = r.get(s.id);
    assert_eq!(after.next_run_at, Some(due + 24 * 3600));
    assert!(after.last_status.starts_with("skipped: "), "{}", after.last_status);
    // With room again, the next time runs.
    assert_eq!(r.tick(due + 24 * 3600 + 1).started, 1);
}
