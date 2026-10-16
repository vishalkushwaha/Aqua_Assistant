//! The tests of `modules::browser_agent::planner_budget_tests`, moved here from an inline module of `browser_agent.rs` (card OA3-05, move only): the path of every test is as it was.

use super::*;
use crate::modules::llm_router::llama_engine::{PLANNER_ANSWER_TOKENS, PLANNER_CODE_TOKENS, PLANNER_TOOL_TOKENS};

#[test]
fn the_planner_gets_room_for_what_it_is_asked_to_write() {
    // The old guess gave every reply 120 tokens. A tool call needs more than that for a typed message, an answer needs
    // a few sentences, and a code edit carries the code.
    assert!(PLANNER_TOOL_TOKENS > 120);

    // After a failed tool: a tool call.
    let failed = vec![("App".to_string(), "ERROR [NOT_FOUND] no such app".to_string())];
    let (_, after_failure) = build_user_turn_with_budget("Open Notepad", &failed, "", "test-key");
    assert_eq!(after_failure, PLANNER_TOOL_TOKENS);

    // Evidence that the action is done: one short confirmation sentence.
    let opened = vec![("Snapshot".to_string(), "Window: Untitled - Notepad (in front). Notepad is open and ready for text.".to_string())];
    let (text, short_final) = build_user_turn_with_budget("open notepad", &opened, "", "test-key");
    assert!(text.contains("one short sentence"), "{text}");
    assert!(short_final >= PLANNER_TOOL_TOKENS && short_final < PLANNER_ANSWER_TOKENS);

    // A question with evidence: the actual answer, in a few sentences.
    let weather = vec![("browser_snapshot".to_string(), "Weather result: Sunny. 36 degrees Celsius. Wind 9 km/h. Humidity 14%.".to_string())];
    let (text, answer) = build_user_turn_with_budget("Tell me todays weather", &weather, "", "test-key");
    assert!(text.contains("the actual answer"), "{text}");
    assert_eq!(answer, PLANNER_ANSWER_TOKENS);

    // The first round and a "tool or final" round may end in an answer too.
    let (_, first) = build_user_turn_with_budget("Tell me a fun fact", &[], "", "test-key");
    assert_eq!(first, PLANNER_ANSWER_TOKENS);

    // Code: room for the edit, in every kind of round.
    let (_, code_first) = build_user_turn_with_budget("fix the failing tests in the project", &[], "", "test-key");
    let (_, code_after_failure) = build_user_turn_with_budget("fix the failing tests in the project", &failed, "", "test-key");
    assert_eq!((code_first, code_after_failure), (PLANNER_CODE_TOKENS, PLANNER_CODE_TOKENS));
}
