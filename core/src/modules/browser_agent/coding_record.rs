//! The record of a coding run (the goal, what was read and changed, the last check, what is left) as the words a prompt ends with, and what a coding run that stops tells the person (card OA3-05, move only).

use super::*;

/// The record of a coding run (what the goal is, what was read and changed, the last error, the next step) as one line of text, or nothing when the goal is not a coding goal.
pub(super) fn coding_record_text(goal: &str, history: &[(String, String)]) -> String {
    if !crate::modules::run_profile::coding_latched(goal) {
        return String::new();
    }
    // The coding record v2 (what the checks said, kept with the run) is for a person who turned `AQUA_CODING_RECORD` on; the record that is the first error for ever stays what is shown otherwise
    // (card OA4-04: the transcripts of `core/tests/fixtures/agent_scenarios/golden/` pin its text, and a golden is the owner's to write again).
    if !crate::flags::is_on(&crate::flags::CODING_RECORD) {
        return coding_record_v1(goal, history);
    }
    crate::modules::coding_driver::record_line(&coding_record_of(goal, history))
}

/// The record of a coding run as it always was: built from the history, the first error kept for ever (`AQUA_CODING_RECORD` off).
fn coding_record_v1(goal: &str, history: &[(String, String)]) -> String {
    let events: Vec<(&str, &str, &str)> = history.iter().map(|(tool, obs)| (tool.as_str(), "", obs.as_str())).collect();
    let mut record = crate::modules::coding_driver::compact(goal, &events);
    let (reads, changed) = crate::modules::coding_session::files_this_run();
    for path in reads {
        if !record.files_read.contains(&path) {
            record.files_read.push(path);
        }
    }
    for path in changed {
        if !record.files_changed.contains(&path) {
            record.files_changed.push(path);
        }
    }
    crate::modules::coding_driver::preamble(&record)
}

/// The step that records a check the loop ran after an edit: a `term.run` of the run, marked `automatic`, with what the tool answered. It is the same step the model's own call of the
/// same command would be, so the gate and the record read it as any other check.
pub(super) fn automatic_check_step(step: u32, arguments: Value, observation: &str) -> AgentStep {
    AgentStep {
        step,
        kind: "tool".into(),
        detail: json!({
            "id": format!("tool-{step}-term.run-check"),
            "phase": "act",
            "title": "term.run ok (the check after the edit)",
            "status": "completed",
            "tool": "term.run",
            "source": "host",
            "automatic": true,
            "arguments": arguments,
            "observation": record_observation(observation),
            "task_completed": false,
            "message": "The check that follows an edit ran.",
            "next": "Observe result",
        }),
    }
}

/// Makes the coding record of the run from its calls so far, for the prompts of the round that follows (see [`CODING_RECORD`]), and keeps it with the run when it is a coding run and
/// `AQUA_CODING_RECORD` is on (card OA4-04, WF-06): what the model was told it had done and what the checks said are still there when the run is read later (`record` of `GET /api/v1/runs/{id}`).
pub(super) fn remember_calls_for_record(goal: &str, steps: &[AgentStep]) {
    if !crate::flags::is_on(&crate::flags::CODING_RECORD) || !crate::modules::run_profile::coding_latched(goal) {
        return;
    }
    let calls = crate::modules::agent_experience::tool_calls_of(steps);
    let (reads, changed) = crate::modules::coding_session::files_this_run();
    let record = crate::modules::coding_driver::CodingRecord::build(goal, &calls, &reads, &changed);
    persist_coding_record(&record);
    set_coding_record(Some((goal.to_string(), record)));
}

/// What a coding run that stops before it gives an answer tells the person (card OA4-04, AG-09): the files it changed, what its last check said and why it stopped, from the facts of the run.
/// `None` for a run that is not a coding run, asks for information, or changed no file: its own sentence stands. The record of the run is kept as it is at the stop.
pub(super) fn coding_stop_note(goal: &str, steps: &[AgentStep], why: crate::modules::coding_driver::StopWhy) -> Option<String> {
    remember_calls_for_record(goal, steps);
    if !crate::modules::run_profile::coding_latched(goal) || goal_asks_for_information(goal) {
        return None;
    }
    let calls = crate::modules::agent_experience::tool_calls_of(steps);
    let changed = crate::modules::coding_driver::changed_files(&calls, &crate::modules::coding_session::files_this_run().1);
    crate::modules::coding_driver::stop_summary(&changed, &crate::modules::agent_experience::code_evidence_for_goal(steps, goal), why)
}

/// `answer` with what [`coding_stop_note`] says after it: the sentence the run would have ended with, and the facts of the files it changed.
pub(super) fn with_stop_note(answer: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{answer}\n{note}"),
        None => answer,
    }
}

/// Is a coding run that has used its rounds given more, because it is getting somewhere (card OA10-04, WF-12)? Only with `AQUA_CONTINUE` on, and only for a coding run whose newest check
/// named fewer failures than the check of the same kind before it (`coding_driver::failing_shrank`).
pub(super) fn extension_allowed(goal: &str, steps: &[AgentStep]) -> bool {
    crate::flags::is_on(&crate::flags::RUN_CONTINUE) && crate::modules::run_profile::coding_latched(goal) && crate::modules::coding_driver::failing_shrank(&crate::modules::agent_experience::tool_calls_of(steps))
}

/// What a coding run that stopped on its rounds or its time says about where it stopped, and keeps with its record so that `POST /api/v1/runs/{id}/continue` can go on from it (card OA10-04, WF-12,
/// `AQUA_CONTINUE`): the same record the model was shown, as one line of data. `None` for a run that is not a coding run, asks for information, or when the switch is off.
pub(super) fn continuation_note(goal: &str, steps: &[AgentStep]) -> Option<String> {
    if !crate::flags::is_on(&crate::flags::RUN_CONTINUE) || !crate::modules::run_profile::coding_latched(goal) || goal_asks_for_information(goal) {
        return None;
    }
    let calls = crate::modules::agent_experience::tool_calls_of(steps);
    let (reads, changed) = crate::modules::coding_session::files_this_run();
    let record = crate::modules::coding_driver::CodingRecord::build(goal, &calls, &reads, &changed);
    persist_coding_record(&record);
    Some(crate::modules::coding_driver::stopped_line(&record))
}

/// Writes the coding record of the run on this thread into its record, when it has something in it.
fn persist_coding_record(record: &crate::modules::coding_driver::CodingRecord) {
    if record.files_read.is_empty() && record.files_changed.is_empty() && record.last_check.is_none() && record.blocked.is_none() {
        return;
    }
    // What the run keeps of the goal is what its record keeps (`goal_redacted`): the words of the person, with what looks like a secret taken out.
    let mut kept = record.clone();
    kept.objective = crate::memory::experience::sanitize::redact(&kept.objective).0;
    if let Ok(json) = serde_json::to_string(&kept) {
        crate::agent_runs::note_coding_record(&json);
    }
}

/// The coding record of the run (what the goal is, what was read and changed, what the last check said, what is in the way and what to do next): the one made at the start of the round when it
/// is of this goal, else one made from the history (which has the results and not the arguments, so it cannot tell which of the calls were checks).
fn coding_record_of(goal: &str, history: &[(String, String)]) -> crate::modules::coding_driver::CodingRecord {
    if let Some(record) = coding_record_for(goal) {
        return record;
    }
    let calls: Vec<(String, Value, String)> = history.iter().map(|(tool, obs)| (tool.clone(), Value::Null, obs.clone())).collect();
    let (reads, changed) = crate::modules::coding_session::files_this_run();
    crate::modules::coding_driver::CodingRecord::build(goal, &calls, &reads, &changed)
}

/// Previous tool results, with the context manager when it is on or this run is a coding goal. The record stands in front of them, as it always did, unless the start of the prompt is
/// kept stable ([`coding_record_tail`] puts it at the end then).
pub(super) fn coding_record_prefix(history: &[(String, String)]) -> String {
    let goal = goal_for_context();
    if prefix_is_stable_for(&goal) {
        return String::new();
    }
    let text = coding_record_text(&goal, history);
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}

/// The record of a coding run as the last words of the prompt, before the closing switch (card OA4-02, task CX-05, `AQUA_PREFIX_STABLE=on`). It changes whenever the run reads, edits or
/// fails; in front of the results, as it used to be, every such change made the model read the results again. Nothing in the first round (there is nothing to record), and nothing for a
/// goal that is not a coding goal. The text is the record's own: it says it is data and not new instructions.
pub(super) fn coding_record_tail(goal: &str, history: &[(String, String)]) -> String {
    if history.is_empty() || !prefix_is_stable_for(goal) {
        return String::new();
    }
    format!(" {}", coding_record_text(goal, history))
}
