//! Test of card OA11-01 (task EX-04): a run that ends in an error ends too, so the `Stop` hooks are told what stopped it. They were told only when the loop came back with an answer,
//! a question or a stop, and a run the model server could not serve at its first call (an `Err`) was closed in the record and never said to the hooks.
//!
//! No model runs: the planner is a script whose first reply is an error.

use super::*;
use crate::test_support::{Reply, ScriptedPlanner};
use aqua_tool_host::{HookAnswer, HookCall, HookEvent, Hooks, PostAnswer};

/// Hooks that ask for the `Stop` event and keep (run, why it stopped) of each.
#[derive(Default)]
struct Stops(std::sync::Mutex<Vec<(String, String)>>);

impl Hooks for Stops {
    fn pre_tool(&self, _call: &HookCall) -> HookAnswer {
        HookAnswer::Pass
    }
    fn post_tool(&self, _call: &HookCall, _ok: bool, _text: &str) -> PostAnswer {
        PostAnswer::default()
    }
    fn on_event(&self, event: &HookEvent) -> HookAnswer {
        if event.event == "Stop" {
            self.0.lock().unwrap().push((event.run_id.clone(), event.stopped_reason.clone()));
        }
        HookAnswer::Pass
    }
    fn wants_event(&self, event: &str) -> bool {
        event == "Stop"
    }
}

#[test]
fn a_run_that_ends_in_an_error_still_tells_the_stop_hooks_what_stopped_it() {
    let _g = crate::modules::planner::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("AQUA_PLANNER_BATCH");
    std::env::remove_var("AQUA_PLANNER_CONSTRAINT");
    let _host = super::tests::shared_host();
    let _one = super::tests::LOOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let stops = Arc::new(Stops::default());
    let dir = std::env::temp_dir().join(format!("aqua_stop_hook_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(crate::agent_runs::AgentRunStore::open(dir.join("agent_runs.db")).unwrap());
    let learning = AgentLearning::at(&dir.join("learning"));
    // The first call of the model fails, and nothing has been done yet: the loop gives the error back instead of an answer.
    let planner = ScriptedPlanner::new(vec![Reply::Error("the model server went away".into())]);
    let out = crate::hooks::with_chain(stops.clone(), || {
        run_mcp_agent_core_with(&planner, &learning, Some(store.clone()), None, None, "read the file a.txt", 6, None, None, &mut super::super::agent_experience::ExperienceUse::default())
    });
    assert!(out.is_err(), "the run ended in an error: {out:?}");
    let seen: Vec<(String, String)> = stops.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "told once: {seen:?}");
    assert!(seen[0].1.starts_with("error: ") && seen[0].1.contains("LLM generate"), "{seen:?}");
    let record = store.get(&seen[0].0).unwrap().unwrap();
    assert_eq!(record["stopped_reason"].as_str(), Some(seen[0].1.as_str()), "and the record says the same: {record}");
}
