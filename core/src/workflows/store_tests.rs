use super::model::*;
use super::store::*;
use serde_json::json;

pub(super) fn db_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aqua_wf_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("workflows.db")
}

pub(super) fn input(name: &str, trigger: serde_json::Value, enabled: bool) -> WorkflowInput {
    serde_json::from_value(json!({
        "name": name,
        "enabled": enabled,
        "trigger": trigger,
        "steps": [
            { "id": "a", "type": "set", "value": "one" },
            { "id": "b", "type": "set", "value": "{{steps.a.output}} two" }
        ]
    }))
    .unwrap()
}

/// One tool step, so a run reaches the executor.
pub(super) fn tool_input(name: &str, trigger: serde_json::Value, enabled: bool) -> WorkflowInput {
    serde_json::from_value(json!({
        "name": name,
        "enabled": enabled,
        "trigger": trigger,
        "steps": [{ "id": "t", "type": "tool", "tool": "web.get", "arguments": {} }]
    }))
    .unwrap()
}

#[test]
fn create_read_update_enable_delete() {
    let s = WorkflowStore::new(db_path("crud"));
    let now = 1_800_000_000;
    let w = s.create("local", &input("Daily", json!({ "type": "schedule", "cron": "0 9 * * *" }), true), now).unwrap();
    assert_eq!((w.name.as_str(), w.enabled, w.owner_id.as_str()), ("Daily", true, "local"));
    assert!(w.next_run_at.is_some_and(|n| n > now), "an enabled schedule has a next run");
    assert_eq!(w.steps.len(), 2);
    assert_eq!(s.list("local").unwrap().len(), 1);

    let off = s.set_enabled("local", w.id, false, now + 1).unwrap().unwrap();
    assert!(!off.enabled && off.next_run_at.is_none(), "disabled means no next run");
    let on = s.set_enabled("local", w.id, true, now + 2).unwrap().unwrap();
    assert!(on.next_run_at.is_some());

    let mut edited = input("Daily v2", json!({ "type": "interval", "every_secs": 600 }), true);
    edited.description = "changed".into();
    let up = s.update("local", w.id, &edited, now + 10).unwrap().unwrap();
    assert_eq!((up.name.as_str(), up.description.as_str()), ("Daily v2", "changed"));
    assert_eq!(up.next_run_at, Some(now + 10 + 600));
    assert!(s.update("local", 999, &edited, now).unwrap().is_none());

    // Invalid input never reaches the database.
    let mut bad = edited.clone();
    bad.steps.clear();
    assert!(s.create("local", &bad, now).is_err() && s.update("local", w.id, &bad, now).is_err());
    assert_eq!(s.list("local").unwrap().len(), 1);

    assert!(s.delete("local", w.id).unwrap());
    assert!(!s.delete("local", w.id).unwrap());
    assert!(s.get("local", w.id).unwrap().is_none());
}

#[test]
fn one_owner_never_sees_or_changes_another_owners_workflows_or_runs() {
    let s = WorkflowStore::new(db_path("owners"));
    let now = 1_800_000_000;
    let mine = s.create("alice", &input("Mine", json!({ "type": "manual" }), true), now).unwrap();
    let theirs = s.create("bob", &input("Theirs", json!({ "type": "manual" }), true), now).unwrap();
    assert_eq!(s.list("alice").unwrap().iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["Mine"]);
    assert_eq!(s.list("bob").unwrap().len(), 1);
    assert!(s.get("alice", theirs.id).unwrap().is_none());
    assert!(s.update("alice", theirs.id, &input("Hijack", json!({ "type": "manual" }), true), now).unwrap().is_none());
    assert!(s.set_enabled("alice", theirs.id, false, now).unwrap().is_none());
    assert!(!s.delete("alice", theirs.id).unwrap());
    assert_eq!(s.get("bob", theirs.id).unwrap().unwrap().name, "Theirs", "untouched");
    // Runs too.
    let Claim::Started(run) = s.claim(&theirs, "manual", false, now).unwrap() else { panic!() };
    assert!(s.get_run("alice", run).unwrap().is_none());
    assert!(s.list_runs("alice", theirs.id, 10).unwrap().is_empty());
    assert!(s.get_run("bob", run).unwrap().is_some());
    let _ = mine;
}

#[test]
fn a_workflow_runs_one_at_a_time_and_a_run_leaves_a_step_log() {
    let s = WorkflowStore::new(db_path("claim"));
    let now = 1_800_000_000;
    let w = s.create("local", &input("W", json!({ "type": "interval", "every_secs": 60 }), true), now).unwrap();
    let Claim::Started(run) = s.claim(&w, "manual", false, now).unwrap() else { panic!("should start") };
    let r = s.get_run("local", run).unwrap().unwrap();
    assert_eq!((r.status, r.steps.len()), (RunStatus::Running, 2));
    assert!(r.steps.iter().all(|x| x.status == "pending"));

    // A manual run while one is going is refused; a scheduled one is recorded as skipped and moves on.
    assert!(s.claim(&w, "manual", false, now + 1).unwrap_err().contains("already running"));
    let due = s.get("local", w.id).unwrap().unwrap();
    let Claim::Skipped { reason, .. } = s.claim(&due, "interval", true, now + 60).unwrap() else { panic!("should skip") };
    assert!(reason.contains("still going"));
    assert_eq!(s.get("local", w.id).unwrap().unwrap().next_run_at, Some(now + 60 + 60), "the schedule moved on");

    s.step_started(run, 0, now).unwrap();
    s.step_attempt(run, 0, 2).unwrap();
    s.step_finished(run, 0, "succeeded", "one", "", now + 1).unwrap();
    s.finish_run(run, RunStatus::Succeeded, "", now + 2).unwrap();
    let r = s.get_run("local", run).unwrap().unwrap();
    assert_eq!((r.status, r.finished_at), (RunStatus::Succeeded, now + 2));
    assert_eq!((r.steps[0].status.as_str(), r.steps[0].attempts, r.steps[0].output.as_str()), ("succeeded", 2, "one"));
    assert_eq!(s.get("local", w.id).unwrap().unwrap().last_status, "succeeded");
    // Now it can run again.
    assert!(matches!(s.claim(&w, "manual", false, now + 100).unwrap(), Claim::Started(_)));
    let runs = s.list_runs("local", w.id, 10).unwrap();
    assert_eq!(runs.len(), 3);
    assert!(runs[0].id > runs[1].id, "newest first");
}

#[test]
fn due_workflows_missed_runs_and_crash_recovery() {
    let s = WorkflowStore::new(db_path("due"));
    let now = 1_800_000_000;
    let w = s.create("local", &input("W", json!({ "type": "interval", "every_secs": 60 }), true), now).unwrap();
    assert!(s.due(now).unwrap().is_empty());
    assert_eq!(s.due(now + 60).unwrap().len(), 1);
    let disabled = s.set_enabled("local", w.id, false, now).unwrap().unwrap();
    assert!(s.due(now + 1000).unwrap().is_empty(), "disabled workflows never come due: {disabled:?}");

    let w = s.set_enabled("local", w.id, true, now).unwrap().unwrap();
    let run = s.record_missed(&w, now + 3600, now + 60).unwrap();
    let r = s.get_run("local", run).unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Skipped);
    assert!(r.error.contains("missed") && r.error.contains("not running"), "{}", r.error);
    assert_eq!(s.get("local", w.id).unwrap().unwrap().next_run_at, Some(now + 3600 + 60));

    // A crash mid-run: the run and its running step are marked, pending steps skipped.
    let Claim::Started(run) = s.claim(&w, "manual", false, now + 4000).unwrap() else { panic!() };
    s.step_started(run, 0, now + 4000).unwrap();
    assert_eq!(s.recover_interrupted(now + 5000).unwrap(), 1);
    let r = s.get_run("local", run).unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Interrupted);
    assert_eq!((r.steps[0].status.as_str(), r.steps[1].status.as_str()), ("failed", "skipped"));
    assert_eq!(s.get("local", w.id).unwrap().unwrap().last_status, "interrupted");
    assert!(matches!(s.claim(&w, "manual", false, now + 6000).unwrap(), Claim::Started(_)), "it can run again");
}

#[test]
fn history_is_pruned_and_deleting_a_workflow_deletes_its_history() {
    let s = WorkflowStore::new(db_path("prune"));
    let now = 1_800_000_000;
    let w = s.create("local", &input("W", json!({ "type": "manual" }), true), now).unwrap();
    for i in 0..6 {
        let Claim::Started(run) = s.claim(&w, "manual", false, now + i).unwrap() else { panic!() };
        s.finish_run(run, RunStatus::Succeeded, "", now + i).unwrap();
    }
    assert_eq!(s.prune(3, 10 * 86_400, now + 10).unwrap(), 3);
    let kept = s.list_runs("local", w.id, 50).unwrap();
    assert_eq!(kept.len(), 3);
    assert!(s.get_run("local", kept[0].id).unwrap().unwrap().steps.len() == 2, "kept runs keep their steps");
    // By age.
    assert_eq!(s.prune(100, 5, now + 1000).unwrap(), 3);
    assert!(s.list_runs("local", w.id, 50).unwrap().is_empty());
    let Claim::Started(run) = s.claim(&w, "manual", false, now + 2000).unwrap() else { panic!() };
    // A running run is never pruned.
    assert_eq!(s.prune(0, 0, now + 9999).unwrap(), 0);
    assert!(s.delete("local", w.id).unwrap());
    assert!(s.get_run("local", run).unwrap().is_none());
}

// ---- the file (OA3-02: the shared SQLite helper) ------------------------------------------------------------

fn file_version(path: &std::path::Path) -> i64 {
    rusqlite::Connection::open(path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

#[test]
fn the_workflow_file_has_a_version_and_one_made_before_versions_keeps_its_workflows_when_stamped() {
    let path = db_path("version");
    let s = WorkflowStore::new(&path);
    let w = s.create("local", &input("Daily", json!({ "type": "manual" }), false), 1_800_000_000).unwrap();
    assert_eq!(file_version(&path), 1);
    rusqlite::Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 0;").unwrap();
    let again = WorkflowStore::new(&path);
    assert_eq!(file_version(&path), 1);
    assert_eq!(again.list("local").unwrap().iter().map(|x| x.id).collect::<Vec<_>>(), vec![w.id]);
}

#[test]
fn a_damaged_workflow_file_is_moved_aside_and_the_store_works_in_a_new_one() {
    let path = db_path("damaged");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "this is not a database, only text. ".repeat(300)).unwrap();
    let s = WorkflowStore::new(&path);
    assert!(s.list("local").unwrap().is_empty());
    s.create("local", &input("After", json!({ "type": "manual" }), false), 1_800_000_000).unwrap();
    assert_eq!(s.list("local").unwrap().len(), 1);
    let aside = crate::storage::sqlite_store::leftovers(path.parent().unwrap());
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("this is not a database"));
}

#[test]
fn a_workflow_file_from_a_newer_build_is_refused_call_by_call_and_left_alone() {
    let path = db_path("newer");
    let s = WorkflowStore::new(&path);
    s.create("local", &input("Kept", json!({ "type": "manual" }), false), 1_800_000_000).unwrap();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
        c.execute_batch("PRAGMA user_version = 3;").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let newer = WorkflowStore::new(&path);
    let err = newer.list("local").unwrap_err();
    assert!(err.contains("newer than this build") && err.contains("version 3"), "{err}");
    assert!(newer.create("local", &input("Must not be written", json!({ "type": "manual" }), false), 1_800_000_001).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
