//! A coding session that ended well, as one task for the learner (card OA10-04, task CX-19 of the OpenCode audit; `AQUA_SESSION_SUMMARY=on`, experimental).
//!
//! The notes of a coding session are gone four hours after its last run; what a session came to is worth keeping longer only when it came to something: the project's code was changed and the
//! last check of it passed. This module makes that into a [`TaskRecord`], the learner's own input, so that every gate of the learner applies to it as to any task (the secrets masked, the
//! quality bar, the duplicate check, the status): nothing here stores anything.
//!
//! * **built from the record of the session, by the program, with no call of the model**: the goal, the names of the files that were changed, and what the last check said. A summary the
//!   model wrote would be a second source of text to hold against the record; this one is the record, cut to 240 characters, so every fact in it is checkable against it by construction (the
//!   test holds it to that). The model-written version of the audit's task is not built: it needs the model, and the idle time of the card would compete with the person's own turn;
//! * **only a session that ended on a passing check that came after the last change**: a session with no changed file, no check, a failing check or a check that the code changed after
//!   has no summary (`None`);
//! * **no file contents**: names and one line from the check, as the record has them;
//! * **at most one a session** (`coding_sessions.summarised`), and only for a session that was closed and has been idle (see `modules::coding_session_record::summarise_idle`).
//!
//! The experience the learner makes of it is offered to a later goal as every verified experience is (as data, never as an instruction); nothing is changed in how experiences are retrieved.

use super::learner::TaskRecord;
use super::model::{Verification, VerificationMethod};
use crate::modules::coding_session_record::SessionRecord;

/// The longest summary.
pub const SUMMARY_CHARS: usize = 240;

/// How a check of the session is known to the learner.
pub fn method_of(kind: &str) -> VerificationMethod {
    match kind.trim().to_ascii_lowercase().as_str() {
        "tests" => VerificationMethod::Tests,
        "build" => VerificationMethod::Build,
        "type check" => VerificationMethod::TypeCheck,
        "lint" => VerificationMethod::Lint,
        _ => VerificationMethod::None,
    }
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The summary line of a session: its goal, the files it changed and what its last check said. Only what the record holds.
pub fn summary_line(record: &SessionRecord) -> Option<String> {
    let changed: Vec<&str> = record.files.iter().filter(|f| f.changed).map(|f| f.path.as_str()).take(6).collect();
    let check = record.last_check.as_ref()?;
    if changed.is_empty() || record.objective.trim().is_empty() {
        return None;
    }
    let mut line = format!("{}. Changed {}. Last check: {} {} {}.", cut(&record.objective, 100).trim_end_matches('.'), changed.join(", "), check.program, check.kind, if check.passed { "passed" } else { "failed" });
    if let Some(open) = record.open_questions.first() {
        line.push_str(&format!(" Open: {}", cut(open, 60)));
    }
    Some(cut(&line, SUMMARY_CHARS))
}

/// The task a session that ended well makes for the learner, or `None` when it did not end well (no changed file, no check, a check that failed or that the code changed after, a goal too
/// short to generalise from).
pub fn task_record(session_id: &str, project_key: &str, record: &SessionRecord) -> Option<TaskRecord> {
    let check = record.last_check.as_ref().filter(|c| c.passed && !c.stale)?;
    let method = method_of(&check.kind);
    let verification = Verification::new(true, method, format!("{} {} passed", check.program, check.kind));
    if !verification.verified || record.objective.split_whitespace().count() < 3 {
        return None;
    }
    let summary = summary_line(record)?;
    let changed: Vec<String> = record.files.iter().filter(|f| f.changed).map(|f| format!("changed {}", f.path)).take(6).collect();
    Some(TaskRecord {
        task_id: format!("session-{session_id}"),
        goal: record.objective.clone(),
        task_type: "coding_session".to_string(),
        project_key: format!("code:{project_key}"),
        steps: changed,
        tools_used: vec!["fs.edit".to_string(), "term.run".to_string()],
        result_summary: summary,
        finished: true,
        verification: Some(verification),
        tool_calls: record.files.len() as u32,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::coding_driver::{CheckBrief, CodingRecord};
    use crate::modules::coding_session_record::RunSummary;

    fn record_of(check: Option<CheckBrief>, changed: &[&str], goal: &str) -> SessionRecord {
        let built = CodingRecord {
            objective: goal.into(),
            files_read: vec!["src/lib.rs".into()],
            files_changed: changed.iter().map(|s| s.to_string()).collect(),
            edits_since_check: check.as_ref().is_some_and(|c| c.stale),
            last_check: check.clone(),
            checks: check.into_iter().collect(),
            blocked: None,
            next: "give the final answer".into(),
        };
        let mut r = SessionRecord::default();
        r.merge_run(&RunSummary { run_id: "run_1", goal, record: &built, plan: &[], question: None });
        r
    }

    fn check(passed: bool, stale: bool) -> CheckBrief {
        CheckBrief { kind: "tests".into(), passed, program: "cargo".into(), summary: "12 passed".into(), failing: if passed { vec![] } else { vec!["parse::quotes".into()] }, first_error: String::new(), stale }
    }

    #[test]
    fn a_session_that_ended_on_a_passing_check_after_its_last_change_is_a_task_for_the_learner() {
        let r = record_of(Some(check(true, false)), &["src/parse.rs", "src/lib.rs"], "Fix the quoting bug in the csv parser");
        let task = task_record("ses_1", "c:/work/aqua", &r).expect("a session that ended well");
        assert_eq!((task.task_type.as_str(), task.project_key.as_str(), task.task_id.as_str(), task.finished), ("coding_session", "code:c:/work/aqua", "session-ses_1", true));
        assert!(task.verification.as_ref().is_some_and(|v| v.verified && v.method == VerificationMethod::Tests));
        // (The files are in the order the session met them: lib.rs was read first.)
        assert_eq!(task.steps, vec!["changed src/lib.rs".to_string(), "changed src/parse.rs".to_string()]);
        assert!(task.result_summary.chars().count() <= SUMMARY_CHARS, "{}", task.result_summary);
        assert_eq!(task.result_summary, "Fix the quoting bug in the csv parser. Changed src/lib.rs, src/parse.rs. Last check: cargo tests passed.");
    }

    #[test]
    fn every_fact_in_the_summary_is_in_the_record_and_nothing_else_is() {
        let mut r = record_of(Some(check(true, false)), &["src/parse.rs"], "Fix the quoting bug in the csv parser");
        r.open_questions = vec!["Which delimiter does the second file use?".into()];
        r.notes.push(crate::modules::coding_session_record::Note { ts: 1, text: "SECRET-NOTE-TEXT".into(), tainted: false });
        let line = summary_line(&r).unwrap();
        assert!(line.contains("src/parse.rs") && line.contains("cargo tests passed") && line.contains("Which delimiter"), "{line}");
        assert!(!line.contains("SECRET-NOTE-TEXT") && !line.contains("12 passed"), "a note and a tool's own words are not in it: {line}");
        // The files it names are the changed ones and no others: the file that was only read is not among them.
        assert!(!line.contains("src/lib.rs"), "{line}");
    }

    #[test]
    fn a_session_with_nothing_changed_no_check_a_failed_check_or_a_stale_one_has_no_summary() {
        assert!(task_record("s", "p", &record_of(Some(check(true, false)), &[], "Explain how the parser works please")).is_none(), "nothing was changed");
        assert!(task_record("s", "p", &record_of(None, &["a.rs"], "Fix the quoting bug in the csv parser")).is_none(), "no check");
        assert!(task_record("s", "p", &record_of(Some(check(false, false)), &["a.rs"], "Fix the quoting bug in the csv parser")).is_none(), "the check failed");
        assert!(task_record("s", "p", &record_of(Some(check(true, true)), &["a.rs"], "Fix the quoting bug in the csv parser")).is_none(), "the code changed after the check");
        assert!(task_record("s", "p", &record_of(Some(check(true, false)), &["a.rs"], "Fix it")).is_none(), "a goal too short to generalise from");
        let mut odd = check(true, false);
        odd.kind = "something else".into();
        assert!(task_record("s", "p", &record_of(Some(odd), &["a.rs"], "Fix the quoting bug in the csv parser")).is_none(), "a check the learner cannot name is no evidence");
    }
}
