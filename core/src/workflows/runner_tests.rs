use super::engine::*;
use super::model::*;
use super::runner::*;
use super::store::*;
use super::store_tests::{db_path, input, tool_input};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Succeeds at once and counts how many times it was asked.
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
}
impl Executor for Counting {
    fn run_tool(&self, _t: &str, _a: &Value, _d: Duration, _c: &AtomicBool) -> Outcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Outcome::success("done", Value::Null)
    }
    fn run_agent(&self, _g: &str, _m: u32, _d: Duration, _c: &AtomicBool) -> Outcome {
        Outcome::success("done", Value::Null)
    }
}

/// Holds the run open until told to go (or cancelled), so overlap can be tested.
#[derive(Default)]
struct Blocking {
    go: AtomicBool,
    started: AtomicUsize,
}
impl Executor for Blocking {
    fn run_tool(&self, _t: &str, _a: &Value, _d: Duration, cancel: &AtomicBool) -> Outcome {
        self.started.fetch_add(1, Ordering::SeqCst);
        while !self.go.load(Ordering::SeqCst) {
            if cancel.load(Ordering::SeqCst) {
                return Outcome::failure("cancelled", false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Outcome::success("released", Value::Null)
    }
    fn run_agent(&self, g: &str, _m: u32, d: Duration, c: &AtomicBool) -> Outcome {
        self.run_tool(g, &Value::Null, d, c)
    }
}

fn no_sleep(_: u64, c: &AtomicBool) -> bool {
    !c.load(Ordering::SeqCst)
}

fn scheduler(name: &str, exec: Arc<dyn Executor>, max: usize) -> (Arc<Scheduler>, Arc<WorkflowStore>, std::path::PathBuf) {
    let path = db_path(name);
    let store = Arc::new(WorkflowStore::new(&path));
    (Scheduler::new(store.clone(), exec, Arc::new(no_sleep), max), store, path)
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    crate::test_support::wait_until(Duration::from_secs(10), what, f);
}

/// Pretend the schedule came due `secs_ago` seconds ago.
fn make_due(path: &std::path::Path, id: i64, when: i64) {
    Connection::open(path).unwrap().execute("UPDATE workflows SET next_run_at=?1 WHERE id=?2", rusqlite::params![when, id]).unwrap();
}

fn interval() -> Value {
    json!({ "type": "interval", "every_secs": 3600 })
}

#[test]
fn a_due_workflow_runs_by_itself_and_is_scheduled_again() {
    let exec = Arc::new(Counting::default());
    let (sched, store, path) = scheduler("due_runs", exec.clone(), 2);
    let now = now();
    let w = store.create("local", &input("Every hour", interval(), true), now).unwrap();
    let wrong = store.create("local", &input("Off", interval(), false), now).unwrap();
    make_due(&path, w.id, now - 5);
    make_due(&path, wrong.id, now - 5);
    assert_eq!(sched.tick(now), 1, "only the enabled one runs");
    wait_until("the run to finish", || sched.is_idle() && store.list_runs("local", w.id, 5).unwrap().iter().all(|r| r.status != RunStatus::Running));
    let runs = store.list_runs("local", w.id, 5).unwrap();
    assert_eq!((runs.len(), runs[0].status, runs[0].trigger_kind.as_str()), (1, RunStatus::Succeeded, "interval"));
    assert!(store.list_runs("local", wrong.id, 5).unwrap().is_empty());
    assert_eq!(store.get("local", w.id).unwrap().unwrap().next_run_at, Some(now + 3600), "next hour");
    assert_eq!(sched.tick(now + 1), 0, "not due again yet");
}

#[test]
fn a_run_missed_while_aqua_was_closed_is_skipped_unless_the_workflow_asks_to_catch_up() {
    let exec = Arc::new(Counting::default());
    let (sched, store, path) = scheduler("missed", exec, 2);
    let now = now();
    let skip = store.create("local", &input("Skip", interval(), true), now).unwrap();
    let mut catch = input("Catch up", interval(), true);
    catch.catch_up = true;
    let catch = store.create("local", &catch, now).unwrap();
    for w in [&skip, &catch] {
        make_due(&path, w.id, now - 3 * 3600);
    }
    assert_eq!(sched.tick(now), 1);
    wait_until("idle", || sched.is_idle());
    let s = store.list_runs("local", skip.id, 5).unwrap();
    assert_eq!((s.len(), s[0].status), (1, RunStatus::Skipped));
    assert!(s[0].error.contains("missed"), "{}", s[0].error);
    assert_eq!(store.list_runs("local", catch.id, 5).unwrap()[0].status, RunStatus::Succeeded, "catch-up runs once");
    for w in [&skip, &catch] {
        assert_eq!(store.get("local", w.id).unwrap().unwrap().next_run_at, Some(now + 3600), "it does not replay every missed hour");
    }
}

#[test]
fn a_workflow_never_overlaps_itself_and_a_manual_run_is_refused_while_one_is_going() {
    let exec = Arc::new(Blocking::default());
    let (sched, store, path) = scheduler("overlap", exec.clone(), 2);
    let now = now();
    let w = store.create("local", &tool_input("Slow", interval(), true), now).unwrap();
    let run = sched.start(&w, "manual", Value::Null).unwrap();
    wait_until("the first step to start", || exec.started.load(Ordering::SeqCst) == 1);
    assert!(sched.start(&w, "manual", Value::Null).unwrap_err().contains("already running"));
    make_due(&path, w.id, now - 1);
    assert_eq!(sched.tick(now), 0, "the scheduled one is skipped, not stacked");
    let runs = store.list_runs("local", w.id, 5).unwrap();
    assert_eq!(runs.iter().filter(|r| r.status == RunStatus::Skipped).count(), 1);
    exec.go.store(true, Ordering::SeqCst);
    wait_until("idle", || sched.is_idle());
    assert_eq!(store.get_run("local", run).unwrap().unwrap().status, RunStatus::Succeeded);
}

#[test]
fn only_so_many_workflows_run_at_once_and_the_rest_wait_their_turn() {
    let exec = Arc::new(Blocking::default());
    let (sched, store, path) = scheduler("cap", exec.clone(), 1);
    let now = now();
    let a = store.create("local", &tool_input("A", interval(), true), now).unwrap();
    let b = store.create("local", &tool_input("B", interval(), true), now).unwrap();
    make_due(&path, a.id, now - 2);
    make_due(&path, b.id, now - 1);
    assert_eq!(sched.tick(now), 1);
    wait_until("first to start", || exec.started.load(Ordering::SeqCst) == 1);
    assert_eq!(sched.tick(now), 0, "at the cap");
    assert!(sched.start(&b, "manual", Value::Null).unwrap_err().contains("too many"));
    exec.go.store(true, Ordering::SeqCst);
    wait_until("idle", || sched.is_idle());
    assert_eq!(sched.tick(now + 1), 1, "the waiting one goes next");
    wait_until("idle again", || sched.is_idle());
    assert_eq!(store.list_runs("local", b.id, 5).unwrap()[0].status, RunStatus::Succeeded);
}

#[test]
fn a_running_workflow_can_be_cancelled() {
    let exec = Arc::new(Blocking::default());
    let (sched, store, _) = scheduler("cancel_run", exec.clone(), 2);
    let w = store.create("local", &tool_input("Slow", json!({ "type": "manual" }), true), now()).unwrap();
    let run = sched.start(&w, "manual", Value::Null).unwrap();
    wait_until("start", || exec.started.load(Ordering::SeqCst) == 1);
    assert!(sched.cancel(run));
    wait_until("idle", || sched.is_idle());
    assert_eq!(store.get_run("local", run).unwrap().unwrap().status, RunStatus::Cancelled);
    assert!(!sched.cancel(run), "nothing left to cancel");
}

#[test]
fn startup_workflows_run_once_when_asked_and_webhook_payloads_reach_the_steps() {
    let exec = Arc::new(Counting::default());
    let (sched, store, _) = scheduler("startup", exec.clone(), 2);
    let now = now();
    let on_start = store.create("local", &input("At start", json!({ "type": "startup" }), true), now).unwrap();
    let _off = store.create("local", &input("Disabled", json!({ "type": "startup" }), false), now).unwrap();
    assert_eq!(sched.run_startup_workflows(), 1);
    wait_until("idle", || sched.is_idle());
    assert_eq!(store.list_runs("local", on_start.id, 5).unwrap()[0].trigger_kind, "startup");

    let hook_input: WorkflowInput = serde_json::from_value(json!({
        "name": "Hook", "enabled": true, "trigger": { "type": "webhook" },
        "steps": [{ "id": "a", "type": "set", "value": "hello {{trigger.payload.name}}" }]
    }))
    .unwrap();
    let hook = store.create("local", &hook_input, now).unwrap();
    let run = sched.start(&hook, "webhook", json!({ "name": "Asha" })).unwrap();
    wait_until("idle", || sched.is_idle());
    let r = store.get_run("local", run).unwrap().unwrap();
    assert_eq!((r.status, r.steps[0].output.as_str(), r.trigger_kind.as_str()), (RunStatus::Succeeded, "hello Asha", "webhook"));
}
