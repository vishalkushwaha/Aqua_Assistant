//! Tests of `scope/run_scope.rs` and of `agent_tools/run_scope.rs`: the options a command gives a run, the id a run was told, the scope of a run and the runs that start as having read outside text (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test was `agent_tools::run_scope_tests::<name>` and is now `agent_runs::scope::run_scope_tests::<name>`: card OA13-03 moved them with the run scope).

use super::*;
use crate::agent_tools::tests::host;
use crate::agent_tools::{call, tools_for_goal};
use aqua_tool_host::Caller;
use serde_json::json;

/// Card OA11-02: a command starts a run with the list of tools it may call, the packs it is shown and the mark of text a program printed. The list narrows the host (the same refusal a loaded
/// skill's list gives), it is the only thing the model is shown, and it goes to the first run begun while the options wait and to no other.
#[test]
fn a_run_started_by_a_command_may_call_only_what_the_command_allows_and_is_shown_only_that() {
    let h = Arc::new(host());
    let options = RunOptions { allowed_tools: vec!["fs.read_file".into(), "git.status".into()], forced_packs: vec!["files".into(), "git".into()], tainted: true };
    let (id, shown, write, read) = with_run_options(options.clone(), || {
        let scope = RunScope::begin(Some(h.clone()));
        let id = scope.id().to_string();
        assert_eq!(options_of(&id), Some(options.clone()));
        // A run begun inside it (a sub-run) has none: the options were taken by the first.
        {
            let inner = RunScope::begin(Some(h.clone()));
            assert_eq!(options_of(inner.id()), None);
            assert!(!h.is_tainted(inner.id()));
        }
        let shown: Vec<String> = tools_for_goal(&h, "do the thing").into_iter().map(|t| t.name).collect();
        let write = call(&h, "fs.write_file", json!({"path": "a"}), Caller::Agent, &id);
        let read = call(&h, "fs.read_file", json!({"path": "a"}), Caller::Agent, &id);
        assert!(h.is_tainted(&id), "the goal holds what a program printed, so the run starts as one that has read text from outside");
        (id, shown, write, read)
    });
    assert_eq!(shown, vec!["fs.read_file", "git.status"], "only the allowed tools of the packs the command names");
    assert!(write.starts_with("ERROR [PERMISSION_DENIED] fs.write_file: ") && write.contains("allows only fs.read_file, git.status") && write.contains("fs.write_file is not one of them"), "{write}");
    assert!(read.contains("ran fs.read_file"), "{read}");
    // The run is over: its options are let go, and the next run is a plain one.
    assert_eq!(options_of(&id), None);
    let plain = RunScope::begin(Some(h.clone()));
    assert_eq!(options_of(plain.id()), None);
    assert!(!h.is_tainted(plain.id()) && h.restrictions_of(plain.id()).is_empty());
    assert!(tools_for_goal(&h, "do the thing").is_empty(), "the words of this goal pick no pack");
    // Options that say nothing change nothing (and the options do not leak out of the closure that gave them).
    with_run_options(RunOptions::default(), || {
        let scope = RunScope::begin(Some(h.clone()));
        assert_eq!(options_of(scope.id()), None);
    });
    assert!(PENDING_OPTIONS.with(|p| p.borrow().is_none()));
}

/// Card OA10-03 (UX-03): a run whose client was told its id before it began is recorded under that id, once; and a question may be given more time than the default only for the
/// runs that are inside `with_approval_wait`.
#[test]
fn a_planned_run_id_is_taken_by_the_first_run_that_begins_and_the_wait_for_an_answer_is_the_runs_own() {
    let planned = new_run_id();
    assert!(planned.starts_with("run_") && planned != new_run_id(), "{planned}");
    let (first, second) = with_planned_run_id(&planned, || (RunScope::begin(None).id().to_string(), RunScope::begin(None).id().to_string()));
    assert_eq!(first, planned, "the run that began took the id it was told");
    assert_ne!(second, planned, "only once");
    assert_ne!(RunScope::begin(None).id(), planned, "and not after the scope has ended");
    assert_eq!(approval_wait_now(), None);
    with_approval_wait(Some(300), || {
        assert_eq!(approval_wait_now(), Some(300));
        with_approval_wait(None, || assert_eq!(approval_wait_now(), None));
        assert_eq!(approval_wait_now(), Some(300), "the outer setting comes back");
    });
    assert_eq!(approval_wait_now(), None);
}

#[test]
fn runs_have_ids_end_cleanly_and_nest() {
    let h = Arc::new(host());
    assert_eq!(current_run(), "");
    let outer = RunScope::begin(Some(h.clone()));
    let outer_id = outer.id().to_string();
    assert_eq!(current_run(), outer_id);
    {
        let inner = RunScope::begin(Some(h.clone()));
        assert_ne!(inner.id(), outer_id);
        assert_eq!(current_run(), inner.id());
    }
    assert_eq!(current_run(), outer_id, "the outer run is restored");
    drop(outer);
    assert_eq!(current_run(), "");
}

/// Card OA8-04, SE-12: the agent step of a workflow or a flow is a run of its own, and it starts as a run that has read outside text when the steps before it made it one.
#[test]
fn runs_begun_while_the_guard_lives_start_as_having_read_outside_text_and_what_an_ended_run_held_can_be_asked() {
    let h = Arc::new(host());
    let clean = RunScope::begin(Some(h.clone()));
    let clean_id = clean.id().to_string();
    assert!(!h.is_tainted(&clean_id), "no guard, no taint");
    drop(clean);
    assert_eq!(ended_run_marks(&clean_id), Some((false, false)));
    let tainted_id = {
        let _guard = runs_start_tainted();
        let scope = RunScope::begin(Some(h.clone()));
        assert!(h.is_tainted(scope.id()), "begun under the guard");
        {
            let _nested = runs_start_tainted();
            assert!(h.is_tainted(RunScope::begin(Some(h.clone())).id()), "a second guard changes nothing");
        }
        assert!(h.is_tainted(RunScope::begin(Some(h.clone())).id()), "the first guard still holds after the second one is gone");
        scope.id().to_string()
    };
    assert_eq!(ended_run_marks(&tainted_id), Some((true, false)), "what the run held is what the step reads back");
    let after = RunScope::begin(Some(h.clone()));
    assert!(!h.is_tainted(after.id()), "the guard is gone, and the next run is clean again");
    // Another thread is not touched by this thread's guard.
    let _guard = runs_start_tainted();
    let other = {
        let h = h.clone();
        std::thread::spawn(move || {
            let scope = RunScope::begin(Some(h.clone()));
            h.is_tainted(scope.id())
        })
        .join()
        .unwrap()
    };
    assert!(!other, "the guard is of one thread");
    assert_eq!(ended_run_marks("run_nobody_remembers"), None);
    // No host, nothing to mark: a scope with none does not fail.
    let _ = RunScope::begin(None);
}
