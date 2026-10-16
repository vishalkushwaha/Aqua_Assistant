//! The tests of `modules::browser_agent::snapshot_metadata_args_tests`, moved here from an inline module of `browser_agent.rs` (card OA3-05, move only): the path of every test is as it was.

use super::*;

#[test]
fn title_only_snapshot_asks_for_window_metadata() {
    let goal = "Take a snapshot of the desktop and tell me only the front window title.";
    let args = snapshot_arguments_for_goal(goal, json!({"use_vision": false}));
    assert_eq!(args["window_metadata_only"], json!(true));
    assert_eq!(args["use_vision"], json!(false));
}

#[test]
fn a_snapshot_that_needs_elements_keeps_the_full_tree() {
    let goal = "Snapshot the desktop and click the Save button.";
    let args = snapshot_arguments_for_goal(goal, json!({"use_vision": false}));
    assert!(args.get("window_metadata_only").is_none());
}
