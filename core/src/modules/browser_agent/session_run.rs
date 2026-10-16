//! The coding session in the loop (card OA10-04; tasks AG-16 and CX-08 of the OpenCode audit; `AQUA_CODING_SESSIONS=on`).
//!
//! `modules::coding_session_record` is the record and `agent_runs::session` is where it is kept; this file is the loop's side of it, in three moments:
//!
//! * **a run begins** ([`attach`]): a coding run in a project finds the session it belongs to, the one its request named or the run it goes on from is in, else the open session of
//!   the project (made if there is none, ended after four idle hours), reads its record, and keeps the block that tells the run what the session did so far. The block goes into the
//!   **system** part of the prompt (priority 2, so that it is the first of the "what Aqua knows" blocks to be kept when the window is short), worked out once: it is the same in every
//!   round, so the model's cache of the prompt does not change from round to round;
//! * **the run writes a note** ([`write_note`], the tool `agent.note`): at most six a run and twelve a session, 160 characters, masked, refused when they read like an order, marked when
//!   the run had read text from outside. A note is written to the record at once (a crash keeps it) and goes into the **tail** of the prompt (with the plan) for the rest of the run;
//! * **the run ends** ([`finish`]): what it came to (the files it read and changed, what its newest check said, its plan, the question it ended with) is joined into the record. This
//!   happens whatever the run came to: a run that stopped on its rounds or its time has changed files too.
//!
//! With the switch off none of this runs: the prompt is the same bytes as before, no tool is added, nothing is written. Nothing here reads a file's contents; the record holds names.

use super::state::with_session;
use super::McpAgentResult;
use crate::agent_runs::session::{IDLE_NAMED_MS, IDLE_PROJECT_MS};
use crate::agent_runs::AgentRunStore;
use crate::modules::coding_driver::CodingRecord;
use crate::modules::coding_session_record::{self as record, NoteRefused, RunSummary, SessionRecord};
use serde_json::Value;
use std::sync::Arc;
use tracing::warn;

/// The most notes one run may write.
pub(super) const NOTES_PER_RUN: usize = 6;
/// The most characters of the notes of this run that are shown at the end of every prompt.
const TAIL_NOTE_CHARS: usize = 80;

pub(super) struct Attached {
    store: Arc<AgentRunStore>,
    /// The id of the coding session.
    id: String,
    /// The notes this run wrote, in the order it wrote them.
    notes: Vec<String>,
    /// What the record said when the run began, as the block of the system prompt.
    block: String,
}

/// Does the run on this thread have a session (so that `agent.note` can be offered)?
pub(super) fn attached() -> bool {
    with_session(|a| a.borrow().is_some())
}

/// Lets go of the session of this thread.
pub(super) fn detach() {
    with_session(|a| *a.borrow_mut() = None);
}

/// The block that tells the run what its session did so far (data, not instructions): empty when the run has no session, or the session has nothing in it yet. At most 600 characters.
pub(super) fn block() -> String {
    with_session(|a| a.borrow().as_ref().map(|at| at.block.clone()).filter(|b| !b.is_empty()).map(|b| format!("\n{b}")).unwrap_or_default())
}

/// The notes this run wrote, for the end of every prompt after the first (empty until it writes one): the last three, cut.
pub(super) fn tail() -> String {
    with_session(|a| {
        let a = a.borrow();
        let Some(at) = a.as_ref().filter(|at| !at.notes.is_empty()) else { return String::new() };
        let shown: Vec<String> = at.notes.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().map(|n| n.chars().take(TAIL_NOTE_CHARS).collect()).collect();
        format!(" Your notes this run: {}.", shown.join("; "))
    })
}

/// A run begins: the coding session it belongs to is found (or made) and read. Does nothing unless `AQUA_CODING_SESSIONS` is on, the run is a coding run, and the goal is about a
/// project folder the agent works in.
pub(super) fn attach(store: &Arc<AgentRunStore>, run_id: &str, goal: &str) {
    detach();
    if !crate::flags::is_on(&crate::flags::CODING_SESSIONS) || !crate::modules::run_profile::coding(goal) {
        return;
    }
    // The project the brief and the checks are aimed at (a goal that names no folder is about the first one).
    let Some(dir) = super::goal_project_dir(goal).or_else(|| super::project_roots().first().cloned()) else { return };
    let key = record::project_key(&dir);
    let now = crate::agent_runs::now_ms();
    // The session the run was put in when it began: the one its request named or the run it goes on from is in; a run that is in a session of its own (its id) is found by its project.
    let given = crate::agent_runs::current_session_id().filter(|s| s != run_id);
    let row = match &given {
        Some(id) => store.named_session(id, &key, now),
        None => store.open_project_session(&key, &crate::agent_runs::sessions::new_session_id(), now, IDLE_PROJECT_MS),
    };
    let row = match row {
        Ok(row) => row,
        Err(e) => {
            warn!("coding session: {e} (this run has none)");
            return;
        }
    };
    if given.is_none() {
        crate::agent_runs::move_to_session(&row.id);
    }
    crate::agent_runs::note_directory(&dir.to_string_lossy());
    let idle = if given.is_some() { IDLE_NAMED_MS } else { IDLE_PROJECT_MS };
    let block = if row.usable(now, idle) { SessionRecord::from_json(&row.record).block() } else { String::new() };
    crate::metrics::inc("aqua_coding_sessions_total", &[("record", if block.is_empty() { "empty" } else { "told" })]);
    with_session(|a| *a.borrow_mut() = Some(Attached { store: store.clone(), id: row.id, notes: Vec::new(), block }));
}

/// The tool `agent.note {text}`: what the run says to remember for the next goal in this project. The answer is what the model is told.
pub(super) fn write_note(arguments: &Value) -> String {
    with_session(|a| {
        let mut a = a.borrow_mut();
        let Some(at) = a.as_mut() else { return "ERROR [UNAVAILABLE] agent.note: this run has no session to keep a note in".to_string() };
        let text = arguments.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if text.is_empty() || text.chars().count() > record::NOTE_CHARS * 3 {
            return format!("ERROR [INVALID_ARGUMENTS] agent.note requires text, one short note of at most {} characters.", record::NOTE_CHARS);
        }
        if at.notes.len() >= NOTES_PER_RUN {
            return format!("ERROR [NOTE_CAP] this run has written {NOTES_PER_RUN} notes already; put what matters in the answer.");
        }
        // The run had read text from outside (a page, a tool's output that is not Aqua's own): the note may repeat it, and is shown with that mark.
        let tainted = crate::agent_runs::scope::run_marks(&crate::agent_runs::scope::current_run()).is_some_and(|(tainted, _)| tainted);
        let now = crate::agent_runs::now_ms();
        let mut outcome: Result<(), NoteRefused> = Ok(());
        let wrote = at.store.update_coding_session(&at.id, now, &mut |old| {
            let mut r = SessionRecord::from_json(old);
            outcome = r.add_note(&text, tainted, now);
            r.to_json()
        });
        match (wrote, outcome) {
            (Ok(true), Ok(())) => {
                at.notes.push(record::one_line(&text, record::NOTE_CHARS));
                crate::metrics::inc("aqua_coding_session_notes_total", &[("result", "kept")]);
                format!("Noted ({} of {} this run).", at.notes.len(), NOTES_PER_RUN)
            }
            (_, Err(NoteRefused::Instruction)) => {
                crate::metrics::inc("aqua_coding_session_notes_total", &[("result", "dropped")]);
                "ERROR [NOTE_REFUSED] a note is your own memory of the work, not a place for orders (to skip a question, ignore a rule, reveal something): it was not kept.".to_string()
            }
            (_, Err(NoteRefused::Empty)) => format!("ERROR [INVALID_ARGUMENTS] agent.note requires text, one short note of at most {} characters.", record::NOTE_CHARS),
            (Ok(false), _) => "ERROR [UNAVAILABLE] agent.note: the session was deleted while this run went on".to_string(),
            (Err(e), _) => format!("ERROR [UNAVAILABLE] agent.note: {e}"),
        }
    })
}

/// The plan the run wrote, as its record kept it (empty when it wrote none).
fn plan_of(store: &AgentRunStore, run_id: &str) -> Vec<String> {
    store.get(run_id).ok().flatten().and_then(|run| run["plan"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())).unwrap_or_default()
}

/// The run ended with `result`: what it came to is joined into the record of its session. A run that did not begin as a coding run has none, and does nothing here.
pub(super) fn finish(goal: &str, run_id: &str, result: &McpAgentResult) {
    let Some(at) = with_session(|a| a.borrow_mut().take()) else { return };
    let calls = crate::modules::agent_experience::tool_calls_of(&result.steps);
    let (reads, changed) = crate::modules::coding_session::files_this_run();
    let built = CodingRecord::build(goal, &calls, &reads, &changed);
    let plan = plan_of(&at.store, run_id);
    let question = if result.stopped_reason == "waiting_user" { result.question.clone() } else { None };
    let now = crate::agent_runs::now_ms();
    let wrote = at.store.update_coding_session(&at.id, now, &mut |old| {
        let mut r = SessionRecord::from_json(old);
        r.merge_run(&RunSummary { run_id, goal, record: &built, plan: &plan, question: question.as_deref() });
        r.to_json()
    });
    if let Err(e) = wrote {
        warn!("coding session: {e} (what this run came to was not kept)");
    }
}
