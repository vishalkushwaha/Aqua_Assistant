//! Tests of `observation.rs`: what the agent reads of a result (move only: they were part of the inline module `tests` of `agent_tools.rs`; the path of every test is now `agent_tools::observation_tests::<name>`).

use super::*;
use aqua_tool_host::*;

/// Card OA10-03 (AG-11): a refusal that came with the person's words shows them to the model as theirs, and says they do not approve the refused action; a plain refusal, a refusal
/// by time-out and every other failure read as they always did.
#[test]
fn a_refusal_that_came_with_words_shows_them_as_the_persons_own_and_a_plain_one_does_not() {
    let refused = |code: ErrorCode, said: Option<&str>| {
        let mut error = ToolError::new(code, "the user did not approve: Write config.toml");
        if let Some(said) = said {
            error = error.with_person_said(said);
        }
        aqua_tool_host::ToolResult::failure("fs.write", "r", error, 0)
    };
    let with = observation(&refused(ErrorCode::ApprovalDenied, Some("edit \"config.toml\"   instead")));
    assert_eq!(
        with,
        "ERROR [CONFIRMATION_DENIED] fs.write: The user did not approve: the user did not approve: Write config.toml. They said, in their own words: \"edit 'config.toml' instead\". That is what they want instead; it does not approve the refused action. Do not repeat the refused action."
    );
    assert!(denial_carries_words(&with));
    let plain = observation(&refused(ErrorCode::ApprovalDenied, None));
    assert!(plain.ends_with("Do not retry this action; tell the user.") && !denial_carries_words(&plain), "{plain}");
    // A time-out has no words (nobody was there to type them), whatever is in the details.
    let late = observation(&refused(ErrorCode::ApprovalTimeout, Some("late words")));
    assert!(!denial_carries_words(&late) && !late.contains("late words"), "{late}");
    // Words in the text of some other tool's result are not a refusal with words.
    assert!(!denial_carries_words("They said, in their own words: \"x\""));
    assert!(!denial_carries_words("a page said: ERROR [CONFIRMATION_DENIED] fs.write: They said, in their own words: \"x\""), "only a result that begins as a refusal counts");
}

/// Card OA5-03 (TL-03): what a call that worked has to say besides its text reaches the model, in one short line after it.
#[test]
fn observation_appends_warnings() {
    let worked = |text: &str, warnings: &[&str]| {
        let mut r = aqua_tool_host::ToolResult::failure("fs.read", "r", ToolError::new(ErrorCode::BackendError, ""), 0);
        r.ok = true;
        r.error = None;
        r.text = text.to_string();
        r.warnings = warnings.iter().map(|w| w.to_string()).collect();
        r
    };
    assert_eq!(observation(&worked("1\tfn main() {}", &[])), "1\tfn main() {}", "nothing to say, nothing added");
    assert_eq!(observation(&worked("page", &["offset_line 0 was raised to 1", "file_path was read as path"])), "page\n[note: offset_line 0 was raised to 1; file_path was read as path]");
    // An empty result with a note is still "Done." and the note.
    assert_eq!(observation(&worked("", &["only a note"])), "Done.\n[note: only a note]");
    // Each warning is one line with what cannot be seen taken out, and is cut at 200 characters; the line is at most about 400.
    let long = format!("start\u{200b} line\nbreak {}", "x".repeat(500));
    let shown = observation(&worked("page", &[&long, "second", "third"]));
    let note = shown.lines().last().unwrap();
    assert!(note.starts_with("[note: start line break ") && note.ends_with("…; second; third]") && !note.contains('\u{200b}'), "{note}");
    assert!(shown.lines().count() == 2 && note.chars().count() < 450, "{shown}");
    let many: Vec<String> = (0..10).map(|i| format!("warning number {i} {}", "y".repeat(100))).collect();
    let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(observation(&worked("page", &many_refs)).chars().count() < 4 + 450, "all of them together stay short");
    // A failure is as it was: notes belong to a call that worked.
    let mut failed = aqua_tool_host::ToolResult::failure("fs.read", "r", ToolError::new(ErrorCode::NotFound, "no such file"), 0);
    failed.warnings = vec!["a note".into()];
    assert!(!observation(&failed).contains("[note:"), "{}", observation(&failed));
}

#[test]
fn company_graph_records_join_the_existing_source_verifier() {
    let run = format!("company_sources_{}", std::process::id());
    crate::sources::begin(&run, "who owns the incident and why?");
    let mut result = ToolResult::failure("company.search_context", "r", ToolError::new(ErrorCode::BackendError, "unused"), 0);
    result.ok = true;
    result.error = None;
    result.text = "Connected company context from FalkorDB".into();
    result.data = serde_json::json!({"records": [{
        "ticket_id":"INC-1042", "ticket_title":"OAuth incident", "project_id":"identity",
        "project_name":"Identity Platform", "owner_id":"person_priya", "owner_name":"Priya Raman",
        "owner_role":"Staff Engineer", "team_name":"Identity Engineering", "decision_id":"DEC-014",
        "decision_title":"Staged PKCE migration", "decision_summary":"Roll out by client cohort",
        "source_id":"SRC-ADR-014", "source_title":"ADR-014", "source_kind":"document",
        "source_uri":"company://ADR-014"
    }]});
    let shown = note_company_result(&result, &run).unwrap();
    assert!(shown.contains("[1] SRC-ADR-014 (ADR-014)"), "{shown}");
    let sources = crate::sources::sources_of(&run);
    assert_eq!(sources.len(), 1);
    assert_eq!((sources[0].url.as_str(), sources[0].title.as_str()), ("company://ADR-014", "ADR-014"));
    assert!(sources[0].spans[0].contains("Priya Raman") && sources[0].spans[0].contains("DEC-014"));
    crate::sources::end(&run);
}
