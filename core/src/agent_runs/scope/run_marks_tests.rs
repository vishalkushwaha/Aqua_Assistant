//! Tests of `scope/run_marks.rs` and of `agent_tools/run_marks.rs`: the list of a skill or of an agent, the limits, and the marks and lists that go on to the run that continues one (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test was `agent_tools::run_marks_tests::<name>` and is now `agent_runs::scope::run_marks_tests::<name>`: card OA13-03 moved them with the run marks).

use super::*;
use crate::agent_tools::tests::host;
use crate::agent_tools::{restrictions_from_record, with_host};
use serde_json::json;

/// Card R6-16: a skill's list was applied to the calls that go through the host only, so a skill that allowed `fs.read` still let the run click in the browser.
#[test]
fn a_skills_list_narrows_the_browser_the_desktop_and_the_local_tools_too() {
    let h = host();
    h.restrict_run("run_s", vec!["fs.read_file".into()]);
    for tool in ["browser_click", "browser_navigate", "Click", "Snapshot", "local_find", "local_open_url", "term.run"] {
        let why = skill_refusal_on(&h, "run_s", tool).unwrap_or_else(|| panic!("{tool} was not refused"));
        assert!(why.starts_with(&format!("ERROR [PERMISSION_DENIED] {tool}: ")) && why.contains("allows only fs.read_file"), "{why}");
    }
    assert_eq!(skill_refusal_on(&h, "run_s", "fs.read_file"), None);
    assert_eq!(skill_refusal_on(&h, "run_s", "skill.load"), None, "another skill can still be loaded");
    // A list may name a tool as the model writes it or as the host does.
    h.restrict_run("run_b", vec!["browser.*".into()]);
    assert_eq!(skill_refusal_on(&h, "run_b", "browser_click"), None, "browser_click is browser.click");
    assert!(skill_refusal_on(&h, "run_b", "Click").is_some() && skill_refusal_on(&h, "run_b", "local_find").is_some());
    h.restrict_run("run_l", vec!["local_find".into(), "Snapshot".into()]);
    assert_eq!((skill_refusal_on(&h, "run_l", "local_find"), skill_refusal_on(&h, "run_l", "Snapshot")), (None, None));
    assert!(skill_refusal_on(&h, "run_l", "local_open_url").is_some());
    // A run no skill narrowed is not narrowed.
    assert_eq!(skill_refusal_on(&h, "run_free", "browser_click"), None);
    assert_eq!(skill_refusal_on(&h, "", "browser_click"), None);
    // The agent's loop asks first, in one place (card OA3-01, AQ-04: this was a search of the loop's source for the call). `call_named_tool` is where the loop dispatches every call;
    // it is run here with a host and a run of its own, no browser, no desktop and no list of tools, and a call the list does not name is refused there whichever route it would have
    // taken. (Nothing is reached if one were not refused: `local_open_url` has no address here, `local_find` no query, and there is no browser or desktop to go to.)
    let h = Arc::new(host());
    let scope = RunScope::begin(Some(h.clone()));
    h.restrict_run(scope.id(), vec!["fs.read_file".into()]);
    with_host(h.clone(), || {
        for tool in ["browser_click", "Click", "local_find", "local_open_url"] {
            let (label, why) = crate::modules::browser_agent::call_named_tool(tool, json!({}), &[], &None, &None).unwrap_or_else(|e| panic!("{tool} was not refused by the skill's list: {e}"));
            assert_eq!(label, "guard", "{tool}");
            assert!(why.starts_with(&format!("ERROR [PERMISSION_DENIED] {tool}: ")) && why.contains("allows only fs.read_file"), "{why}");
        }
    });
}

/// Card R6-16: the list was kept under the run's id, and a run that goes on after a question has a new one, so answering a question took the restriction off.
#[test]
fn a_skills_list_goes_with_the_run_that_goes_on_from_it() {
    let h = host();
    // The ids are this test's own: the list of ended runs is shared by the tests that run beside it, so it is not emptied here.
    h.restrict_run("run_p", vec!["fs.read_file".into()]);
    h.restrict_run("run_p", vec!["fs.*".into(), "git.status".into()]);
    note_ended(&h, "run_p");
    h.end_run("run_p");
    assert_eq!(skill_refusal_on(&h, "run_p", "term.run"), None, "the ended run holds nothing any more");
    inherit_marks_on(&h, "run_q", "run_p", false, None);
    assert!(skill_refusal_on(&h, "run_q", "term.run").is_some(), "the run that goes on is narrowed as its parent was");
    assert!(skill_refusal_on(&h, "run_q", "git.status").is_some(), "by both lists: a call has to be in every one");
    assert_eq!(skill_refusal_on(&h, "run_q", "fs.read_file"), None);
    // And again for the next one after that.
    note_ended(&h, "run_q");
    h.end_run("run_q");
    inherit_marks_on(&h, "run_r", "run_q", false, None);
    assert!(skill_refusal_on(&h, "run_r", "web.fetch").is_some());
    assert_eq!(skill_refusal_on(&h, "run_r", "fs.read_file"), None);
    // A parent that was not narrowed does not narrow.
    note_ended(&h, "run_free");
    inherit_marks_on(&h, "run_t", "run_free", false, None);
    assert_eq!(skill_refusal_on(&h, "run_t", "term.run"), None);
}

/// Card R6-16: after a restart this process remembers no run, and the list has to come from the record: the `skill.load` calls of the run and of the runs it went on from.
#[test]
fn a_run_from_before_a_restart_gets_its_list_back_from_the_calls_that_loaded_the_skills() {
    use crate::agent_runs::{AgentRunStore, NewRun, NewStep, StepOutcome};
    use crate::skills::{Skill, Skills};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_record_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
    let skills = Skills::from_skills(
        vec![
            Skill { name: "reader".into(), description: "d".into(), allowed_tools: vec!["fs.read_file".into()], body: "b".into() },
            Skill { name: "open".into(), description: "d".into(), allowed_tools: vec![], body: "b".into() },
        ],
        vec![],
    );
    let load = |run: &str, step: u32, name: &str, status: &str| {
        let seq = store.add_step(run, &NewStep { step, kind: "tool".into(), tool: "skill.load".into(), args_redacted: format!(r#"{{"name":"{name}"}}"#), ts: 10, ..Default::default() }).unwrap();
        store.finish_step(run, seq, &StepOutcome { ok: status == "completed", status: status.into(), ..Default::default() }).unwrap();
    };
    store.create_run(&NewRun { id: "grand".into(), started_at: 1, goal_full: "g".into(), caller: "agent".into(), ..Default::default() }).unwrap();
    load("grand", 1, "reader", "completed");
    load("grand", 2, "open", "completed");
    load("grand", 3, "ghost", "completed");
    store.create_run(&NewRun { id: "parent".into(), started_at: 2, goal_full: "g".into(), caller: "agent".into(), parent_run_id: Some("grand".into()), ..Default::default() }).unwrap();
    load("parent", 1, "reader", "failed");
    let lists = restrictions_from_record(&store, &skills, "parent");
    assert_eq!(lists, vec![vec!["fs.read_file".to_string()]], "the skill the grandparent loaded, once; one with no list, one that is not there and one that did not load count for nothing");
    assert!(restrictions_from_record(&store, &skills, "nobody").is_empty());
}

/// Card OA12-01: a skill that ships with Aqua was loaded, then `AQUA_SHIPPED_SKILLS` was turned off and Aqua restarted. The run that goes on is still held to that skill's list (a list
/// only narrows). Without the fallback to the shipped text the set of skills the run may use did not have the skill, and the run went on with every tool.
#[test]
fn a_run_that_loaded_a_shipped_skill_is_held_to_its_list_after_a_restart_even_when_they_are_off() {
    use crate::agent_runs::{AgentRunStore, NewRun, NewStep, StepOutcome};
    use crate::skills::{Skill, Skills};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_record_shipped_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = AgentRunStore::open(dir.join("agent_runs.db")).unwrap();
    let load = |run: &str, step: u32, name: &str| {
        let seq = store.add_step(run, &NewStep { step, kind: "tool".into(), tool: "skill.load".into(), args_redacted: format!(r#"{{"name":"{name}"}}"#), ts: 10, ..Default::default() }).unwrap();
        store.finish_step(run, seq, &StepOutcome { ok: true, status: "completed".into(), ..Default::default() }).unwrap();
    };
    store.create_run(&NewRun { id: "parent".into(), started_at: 1, goal_full: "g".into(), caller: "agent".into(), ..Default::default() }).unwrap();
    load("parent", 1, "review-diff");
    // The skills of the run with the switch off: none of the shipped ones is in the set.
    let none = Skills::none();
    let want = crate::skills::shipped::lookup("review-diff").unwrap().allowed_tools;
    assert!(!want.is_empty());
    assert_eq!(restrictions_from_record(&store, &none, "parent"), vec![want.clone()]);
    // The person's own skill of that name is found first.
    let own = Skills::from_skills(vec![Skill { name: "review-diff".into(), description: "d".into(), allowed_tools: vec!["fs.read".into()], body: "b".into() }], vec![]);
    assert_eq!(restrictions_from_record(&store, &own, "parent"), vec![vec!["fs.read".to_string()]]);
    // A name that is neither the person's nor shipped counts for nothing, as before.
    load("parent", 2, "ghost");
    assert_eq!(restrictions_from_record(&store, &none, "parent"), vec![want]);
}

/// Review fix (narrowing lost after the 64-run ring): a run that a command started with `allowed-tools`, and that ended with a question, was remembered by this process for 64 runs. After
/// that the run that went on from it found nothing in the record about the command's list (only the `skill.load` calls were there), so it ran with every tool. The list is now kept
/// with the run's record when the run is recorded, and the walk back over the parents reads it. Before the fix `restrictions_from_record` returned no list for the command's run, so
/// the first assertions below on `fs.write_file` being refused fail on the old code.
#[test]
fn the_list_a_command_allowed_goes_with_the_run_that_goes_on_after_the_run_is_out_of_the_remembered_ones() {
    use crate::agent_runs::{AgentRunStore, Recording};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_command_list_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(AgentRunStore::open(dir.join("agent_runs.db")).unwrap());
    let h = Arc::new(host());
    // A command starts a run with a list; the loop records the run and the run ends with a question.
    let options = RunOptions { allowed_tools: vec!["fs.read_file".into()], forced_packs: vec![], tainted: false };
    let parent = with_run_options(options, || {
        let scope = RunScope::begin(Some(h.clone()));
        let id = scope.id().to_string();
        Recording::begin_in(store.clone(), &id, "goal", "agent", "agent", None).expect("recorded").finish("question", 1, "which file?", &[]);
        id
    });
    assert_eq!(store.allowed_tools_of(&parent).unwrap(), vec!["fs.read_file".to_string()], "kept with the record of the run");
    // A run no command narrowed has nothing kept, and a run that is not recorded has nothing.
    let plain = RunScope::begin(Some(h.clone()));
    Recording::begin_in(store.clone(), plain.id(), "goal", "agent", "agent", None).expect("recorded").finish("final", 1, "done", &[]);
    assert!(store.allowed_tools_of(plain.id()).unwrap().is_empty() && store.allowed_tools_of("nobody").unwrap().is_empty());
    // Sixty-four runs end after it: the process no longer remembers the run (the list of the runs it remembers is shared with the other tests, which only add to it).
    for i in 0..(ENDED_KEEP + 6) {
        note_ended(&h, &format!("run_flood_{i}_{}", std::process::id()));
    }
    assert_eq!(ended_run_marks(&parent), None, "the parent is out of the remembered runs");
    // The run that goes on from it is narrowed all the same, from the record.
    inherit_marks_on(&h, "run_cmd_child", &parent, false, Some(&store));
    let refused = skill_refusal_on(&h, "run_cmd_child", "fs.write_file").expect("the command's list reaches the run that goes on");
    assert!(refused.starts_with("ERROR [PERMISSION_DENIED] fs.write_file: ") && refused.contains("allows only fs.read_file"), "{refused}");
    assert_eq!(skill_refusal_on(&h, "run_cmd_child", "fs.read_file"), None);
    // And the one after that, when the child is out of the remembered runs too: the walk goes back through the parents of the record.
    Recording::begin_in(store.clone(), "run_cmd_child", "goal", "agent", "agent", Some(&parent)).expect("recorded").finish("question", 1, "which?", &[]);
    for i in 0..(ENDED_KEEP + 6) {
        note_ended(&h, &format!("run_flood_b{i}_{}", std::process::id()));
    }
    inherit_marks_on(&h, "run_cmd_grandchild", "run_cmd_child", true, Some(&store));
    assert!(skill_refusal_on(&h, "run_cmd_grandchild", "fs.write_file").is_some(), "the list is not lost along the chain");
    assert_eq!(skill_refusal_on(&h, "run_cmd_grandchild", "fs.read_file"), None);
    // A run whose parent was never narrowed is not narrowed.
    inherit_marks_on(&h, "run_plain_child", plain.id(), false, Some(&store));
    assert_eq!(skill_refusal_on(&h, "run_plain_child", "fs.write_file"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Card OA11-04 (security review of agent_runs/mod.rs): a run that both a command (`allowed-tools`) and an agent narrowed keeps **both** lists with its record, and the run that goes on after
/// the process forgot it (and after a restart) is narrowed by both: a call has to be in every list. The first version of the change kept the command's list only, so the run that went on had
/// the tools of the command and not the agent's narrowing: the `web.fetch` assertion below fails on that.
#[test]
fn a_run_that_a_command_and_an_agent_both_narrowed_goes_on_narrowed_by_both_after_the_process_forgot_it() {
    use crate::agent_runs::{AgentRunStore, Recording};
    use crate::agents::{parse_agent, AgentUse, Role, Source};
    let dir = std::env::temp_dir().join(format!("aqua_agent_tools_two_lists_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(AgentRunStore::open(dir.join("agent_runs.db")).unwrap());
    let h = Arc::new(host());
    let agent = AgentUse::of(&parse_agent("---\ndescription: d\nallowed-tools: fs.read_file, web.fetch\n---\nwords", "twolists", Source::Data).unwrap(), Role::Primary).unwrap();
    let options = RunOptions { allowed_tools: vec!["fs.read_file".into(), "git.status".into()], forced_packs: vec![], tainted: false };
    let parent = with_run_options(options, || {
        let scope = RunScope::begin(Some(h.clone()));
        let id = scope.id().to_string();
        bind_agent(Some(&*h), &id, agent.clone()).unwrap();
        // In this process the run is narrowed by both at once.
        assert_eq!(skill_refusal_on(&h, &id, "fs.read_file"), None);
        assert!(skill_refusal_on(&h, &id, "git.status").is_some() && skill_refusal_on(&h, &id, "web.fetch").is_some(), "a call has to be in every list");
        Recording::begin_in(store.clone(), &id, "goal", "agent", "agent", None).expect("recorded").finish("question", 1, "which file?", &[]);
        id
    });
    assert_eq!(store.allowed_lists_of(&parent).unwrap(), vec![vec!["fs.read_file".to_string(), "git.status".to_string()], vec!["fs.read_file".to_string(), "web.fetch".to_string()]], "both lists are in the record");
    for i in 0..(ENDED_KEEP + 6) {
        note_ended(&h, &format!("run_two_lists_flood_{i}_{}", std::process::id()));
    }
    assert_eq!(ended_run_marks(&parent), None, "the process no longer remembers the run");
    inherit_marks_on(&h, "run_two_lists_child", &parent, false, Some(&store));
    assert_eq!(skill_refusal_on(&h, "run_two_lists_child", "fs.read_file"), None, "what both lists allow");
    for tool in ["git.status", "web.fetch", "fs.write_file", "term.run"] {
        assert!(skill_refusal_on(&h, "run_two_lists_child", tool).is_some(), "{tool} is in one list only (or none): the run that goes on is held to both");
    }
    // A run only an agent narrowed is kept as one list (the format of before), and one no list narrowed has none.
    let only_agent = RunScope::begin(Some(h.clone()));
    bind_agent(Some(&*h), only_agent.id(), agent).unwrap();
    Recording::begin_in(store.clone(), only_agent.id(), "goal", "agent", "agent", None).expect("recorded").finish("final", 1, "done", &[]);
    assert_eq!(store.allowed_lists_of(only_agent.id()).unwrap(), vec![vec!["fs.read_file".to_string(), "web.fetch".to_string()]]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Card R7-18: the counters of a run's limits are forgotten when its scope ends, as its taint is, and a call with no run behind it is never counted. (Only this test's own run id is
/// looked at: the process-wide counters are shared with the tests that run the loop.)
#[test]
fn the_counters_of_the_limits_go_with_the_run_scope_and_a_call_with_no_run_is_not_counted() {
    let scope = RunScope::begin(None);
    let id = scope.id().to_string();
    assert_eq!(limit_stop("fs.read", &json!({"path": "a.txt"})), None, "a call within the limits is given to the run");
    assert!(crate::autonomy::shared().running().iter().any(|r| r.run_id == id && r.tool_calls == 1), "and counted for this run");
    drop(scope);
    assert!(crate::autonomy::shared().running().iter().all(|r| r.run_id != id), "forgotten with the run");
    assert_eq!(current_run(), "", "no run is on this thread now");
    assert_eq!(limit_stop("fs.read", &json!({"path": "a.txt"})), None, "outside a run nothing is limited");
    assert!(crate::autonomy::shared().running().iter().all(|r| !r.run_id.is_empty()), "and nothing is counted under an empty id");
}

#[test]
fn a_run_that_goes_on_after_a_question_starts_as_tainted_and_as_private_as_the_run_before_it_ended() {
    let h = Arc::new(host());
    let ended = |taint: bool, private: bool| {
        let parent = RunScope::begin(Some(h.clone()));
        if taint {
            h.mark_tainted(parent.id());
        }
        if private {
            h.mark_private(parent.id());
        }
        parent.id().to_string()
    };
    let child_marks = |parent: &str, had_history: bool| {
        let child = RunScope::begin(Some(h.clone()));
        inherit_marks_on(&h, child.id(), parent, had_history, None);
        (h.is_tainted(child.id()), h.is_private(child.id()))
    };
    // It read a page and asked: the answer does not wash that off.
    let read_a_page = ended(true, false);
    assert_eq!(child_marks(&read_a_page, true), (true, false));
    // It held something private as well.
    let read_a_private_file = ended(true, true);
    assert_eq!(child_marks(&read_a_private_file, true), (true, true));
    let only_private = ended(false, true);
    assert_eq!(child_marks(&only_private, true), (false, true));
    // A clean run stays clean.
    let clean = ended(false, false);
    assert_eq!(child_marks(&clean, true), (false, false));
    // Nothing is known of the parent: with work carried over the safe guess is both, with none carried over nothing.
    assert_eq!(child_marks("run_that_was_forgotten", true), (true, true));
    assert_eq!(child_marks("run_that_was_forgotten", false), (false, false));
    // The list is bounded.
    for _ in 0..(ENDED_KEEP + 5) {
        let _ = ended(false, false);
    }
    assert!(registry().ended_len() <= ENDED_KEEP);
    assert_eq!(child_marks(&read_a_page, true), (true, true), "forgotten after 64 runs, so the safe guess");
}
