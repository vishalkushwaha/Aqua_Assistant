//! The transcript of a run: its events folded into the rows a coding view shows (card OA10-06; task UX-14 of the OpenCode audit).
//!
//! A run says what it does as events (`events.rs`): a tool call is two of them, a question to the person is two, a phase is one that is said again when it ends. A screen that shows the
//! run as a conversation has to put them together, and a client that asks for the log from the start, or takes it up again after it was cut off, must get the same picture. [`fold`] is that
//! one function, the same for the app, a terminal client and a test: events in, rows out. It reads nothing and writes nothing, and every string in a row is text that an event already
//! held (masked and cut when it was written): a client shows it as text, never as markup.
//!
//! **Rows** (each has `kind` and `seq`, the number of the event that made it; a row that a later event changed also has `seq_end`):
//!
//! * `user`: what the run was asked (`run.started`): `text`, `caller`, `route`, `directory`.
//! * `step`: a phase (`phase`, `title`, `status`); a phase that was said running and then completed or failed is one row, not two.
//! * `tool`: a call (`call_id`, `tool`, `status`, `ok`, `ms`, `decision`, `approval`). For the UI token also `args_head` and `output`: `{ head, lines, collapsed, chars }`, the first ten lines of
//!   what the call returned and how many there are, so that a long output is one line in the transcript that can be opened.
//! * `approval`: a question to the person (`state`: `open` until it ends, then `approved`, `approved_session`, `denied`, `timeout` or `cancelled`; for the UI token `request_id`, `tool`,
//!   `request`). The question is answered in the app: nothing in a transcript answers it.
//! * `question`: the planner asked the person something (the text and the choices are for the UI token).
//! * `assistant`: the answer (`text`), its sentences joined by line breaks.
//! * `limit`: a cap of the autonomy setting ended the run (`stopped_by`, `used`, `cap`).
//! * `error`: the run failed in a way that has a name.
//! * `finished`: the end (`status`, `stopped_reason`, `why_it_stopped`, `answer_head`, `files_changed`, `ms`).
//!
//! **The summary** is what a card at the end of the run says: `status`, `stopped_reason`, `elapsed_ms`, `tokens_in`, `tokens_out`, `tool_calls`, `tool_errors`, `files_changed`, how many
//! questions were asked and how each ended, the last `check.run` and the limit that stopped the run, if one did.
//!
//! **Taking it up again.** An event that was folded already (its number is not above the last) is skipped, so a page that overlaps the last one, or a reconnect that replays a few events,
//! gives the same rows as the log read once. For the API token the fields that show what a call was given or returned, the exact question and what the planner asked are not in a row, as they
//! are not in the events it may read.

use super::events::Stored;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// How many lines of what a call returned a row shows.
pub const OUTPUT_HEAD_LINES: usize = 10;
/// How many rows a transcript has; when a run has more, the newest are kept and `rows_dropped` says how many went.
pub const MAX_ROWS: usize = 5_000;
/// The longest answer of one `assistant` row, in characters.
const MAX_ANSWER_CHARS: usize = 100_000;

/// The folded log of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub rows: Vec<Value>,
    pub summary: Value,
    /// The number of the last event that was folded: the `after` to read the log from next time.
    pub last_seq: i64,
    /// What the run was doing when its last `status` event was written (`running`, `waiting_approval`, `waiting_user`, ...), empty when it has none.
    pub state: String,
    /// The log has its `run.finished`.
    pub finished: bool,
    /// Rows that were left out because there were more than [`MAX_ROWS`].
    pub rows_dropped: usize,
}

/// The first [`OUTPUT_HEAD_LINES`] lines of what a call returned, and how many lines there are.
pub fn output_view(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();
    let head: Vec<&str> = lines.iter().take(OUTPUT_HEAD_LINES).copied().collect();
    json!({ "head": head.join("\n"), "lines": lines.len(), "collapsed": lines.len() > OUTPUT_HEAD_LINES, "chars": text.chars().count() })
}

fn row(kind: &str, seq: i64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), json!(kind));
    m.insert("seq".into(), json!(seq));
    m
}

fn str_of(m: &Map<String, Value>, key: &str) -> String {
    m.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_of(m: &Map<String, Value>, key: &str) -> i64 {
    m.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Puts the events of a run (oldest first) together into the rows of its transcript. `ui` says whether the reader holds the UI token: only then do the rows carry what a call was given or
/// returned, the exact question and what the planner asked.
pub fn fold(events: &[Stored], ui: bool) -> Transcript {
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    let mut tool_rows: HashMap<i64, usize> = HashMap::new();
    let mut open_approvals: Vec<usize> = Vec::new();
    let mut last_seq = 0i64;
    let (mut state, mut finished) = (String::new(), false);
    let (mut tokens_in, mut tokens_out, mut decode_tps, mut tool_calls, mut tool_errors) = (0i64, 0i64, None::<f64>, 0u64, 0u64);
    let (mut asked, mut approved, mut denied, mut timed_out, mut cancelled) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut last_check: Value = Value::Null;
    let mut limit: Value = Value::Null;
    let (mut first_ts, mut last_ts) = (None::<i64>, 0i64);
    let mut end: Option<Map<String, Value>> = None;
    let mut answer_chars = 0usize;

    for e in events {
        if e.seq <= last_seq {
            // Folded already: a page that overlaps the last, or events that a reconnect sent again.
            continue;
        }
        last_seq = e.seq;
        first_ts.get_or_insert(e.ts);
        last_ts = e.ts;
        let ui_value = |key: &str| if ui { e.ui.get(key).cloned() } else { None };
        match e.kind.as_str() {
            "run.started" => {
                let mut r = row("user", e.seq);
                r.insert("text".into(), json!(str_of(&e.data, "goal_head")));
                for key in ["caller", "route", "directory", "session_id"] {
                    if let Some(v) = e.data.get(key) {
                        r.insert(key.into(), v.clone());
                    }
                }
                rows.push(r);
            }
            "status" => {
                state = str_of(&e.data, "state");
            }
            "step" => {
                let (phase, title, status) = (str_of(&e.data, "phase"), str_of(&e.data, "title"), str_of(&e.data, "status"));
                // A phase that is said again with a new status is one row.
                let same = rows.last_mut().filter(|r| r.get("kind").and_then(Value::as_str) == Some("step") && str_of(r, "phase") == phase && str_of(r, "title") == title && str_of(r, "status") == "running");
                match same {
                    Some(r) => {
                        r.insert("status".into(), json!(status));
                        r.insert("seq_end".into(), json!(e.seq));
                    }
                    None => {
                        let mut r = row("step", e.seq);
                        r.insert("phase".into(), json!(phase));
                        r.insert("title".into(), json!(title));
                        r.insert("status".into(), json!(status));
                        rows.push(r);
                    }
                }
            }
            "text" => {
                let piece = str_of(&e.data, "text");
                match rows.last_mut().filter(|r| r.get("kind").and_then(Value::as_str) == Some("assistant")) {
                    Some(r) => {
                        if answer_chars < MAX_ANSWER_CHARS {
                            let mut text = str_of(r, "text");
                            text.push('\n');
                            text.push_str(&piece);
                            answer_chars += piece.chars().count() + 1;
                            r.insert("text".into(), json!(text));
                            r.insert("seq_end".into(), json!(e.seq));
                        }
                    }
                    None => {
                        let mut r = row("assistant", e.seq);
                        answer_chars = piece.chars().count();
                        r.insert("text".into(), json!(piece));
                        rows.push(r);
                    }
                }
            }
            "tool.started" => {
                let call_id = num_of(&e.data, "call_id");
                let mut r = row("tool", e.seq);
                r.insert("call_id".into(), json!(call_id));
                r.insert("tool".into(), json!(str_of(&e.data, "tool")));
                r.insert("status".into(), json!("running"));
                if let Some(args) = ui_value("args_head") {
                    r.insert("args_head".into(), args);
                }
                tool_rows.insert(call_id, rows.len());
                rows.push(r);
            }
            "tool.finished" => {
                let call_id = num_of(&e.data, "call_id");
                let at = match tool_rows.get(&call_id) {
                    Some(&at) => at,
                    None => {
                        // The start of the call was not in what was folded (a page that begins after it): the row is made from its end.
                        let mut r = row("tool", e.seq);
                        r.insert("call_id".into(), json!(call_id));
                        r.insert("tool".into(), json!(""));
                        tool_rows.insert(call_id, rows.len());
                        rows.push(r);
                        rows.len() - 1
                    }
                };
                let ok = e.data.get("ok").and_then(Value::as_bool).unwrap_or(false);
                tool_calls += 1;
                if !ok {
                    tool_errors += 1;
                }
                let r = &mut rows[at];
                r.insert("status".into(), json!(str_of(&e.data, "status")));
                r.insert("ok".into(), json!(ok));
                r.insert("error".into(), json!(!ok));
                r.insert("seq_end".into(), json!(e.seq));
                for key in ["ms", "decision", "approval"] {
                    if let Some(v) = e.data.get(key) {
                        r.insert(key.into(), v.clone());
                    }
                }
                if let Some(Value::String(text)) = ui_value("observation_head") {
                    r.insert("output".into(), output_view(&text));
                }
                if str_of(r, "tool") == "check.run" {
                    last_check = json!({ "tool": "check.run", "ok": ok, "ms": num_of(&e.data, "ms"), "call_id": call_id });
                }
            }
            "approval.opened" => {
                asked += 1;
                let mut r = row("approval", e.seq);
                r.insert("state".into(), json!("open"));
                if let Some(request) = ui_value("request") {
                    for key in ["request_id", "tool"] {
                        if let Some(v) = request.get(key) {
                            r.insert(key.into(), v.clone());
                        }
                    }
                    r.insert("request".into(), request);
                }
                open_approvals.push(rows.len());
                rows.push(r);
            }
            "approval.closed" => {
                let (request_id, outcome) = (str_of(&e.data, "request_id"), str_of(&e.data, "outcome"));
                match outcome.as_str() {
                    "approved" | "approved_session" => approved += 1,
                    "timeout" => timed_out += 1,
                    "cancelled" => cancelled += 1,
                    _ => denied += 1,
                }
                // The question it closes: the one with its id, or (when the reader may not see ids, so that no open question has one) the oldest that is open.
                let ids_known = open_approvals.iter().any(|&i| rows[i].contains_key("request_id"));
                let position = open_approvals
                    .iter()
                    .position(|&i| !request_id.is_empty() && rows[i].get("request_id").and_then(Value::as_str) == Some(request_id.as_str()))
                    .or(if ids_known || open_approvals.is_empty() { None } else { Some(0) });
                let at = match position {
                    Some(p) => open_approvals.remove(p),
                    None => {
                        let mut r = row("approval", e.seq);
                        r.insert("request_id".into(), json!(request_id));
                        rows.push(r);
                        rows.len() - 1
                    }
                };
                rows[at].insert("state".into(), json!(outcome));
                rows[at].insert("seq_end".into(), json!(e.seq));
                if !request_id.is_empty() {
                    rows[at].entry("request_id").or_insert(json!(request_id));
                }
            }
            "usage" => {
                tokens_in += num_of(&e.data, "tokens_in");
                tokens_out += num_of(&e.data, "tokens_out");
                if let Some(tps) = e.data.get("decode_tps").and_then(Value::as_f64) {
                    decode_tps = Some(tps);
                }
            }
            "limit" => {
                let mut r = row("limit", e.seq);
                for key in ["stopped_by", "used", "cap"] {
                    if let Some(v) = e.data.get(key) {
                        r.insert(key.into(), v.clone());
                    }
                }
                limit = Value::Object(r.clone());
                rows.push(r);
            }
            "question" => {
                let mut r = row("question", e.seq);
                for key in ["text", "choices"] {
                    if let Some(v) = ui_value(key) {
                        r.insert(key.into(), v);
                    }
                }
                rows.push(r);
            }
            "error" => {
                let mut r = row("error", e.seq);
                for key in ["code", "message", "retryable"] {
                    if let Some(v) = e.data.get(key) {
                        r.insert(key.into(), v.clone());
                    }
                }
                rows.push(r);
            }
            "run.finished" => {
                finished = true;
                let mut r = row("finished", e.seq);
                for key in ["status", "stopped_reason", "why_it_stopped", "answer_head", "files_changed", "ms"] {
                    if let Some(v) = e.data.get(key) {
                        r.insert(key.into(), v.clone());
                    }
                }
                end = Some(r.clone());
                rows.push(r);
            }
            _ => {}
        }
    }

    let elapsed_ms = end.as_ref().map(|r| num_of(r, "ms")).filter(|ms| *ms > 0).unwrap_or_else(|| first_ts.map_or(0, |first| (last_ts - first).max(0)));
    let summary = json!({
        "status": end.as_ref().map(|r| str_of(r, "status")).unwrap_or_else(|| if state.is_empty() { "running".to_string() } else { state.clone() }),
        "stopped_reason": end.as_ref().map(|r| str_of(r, "stopped_reason")),
        "why_it_stopped": end.as_ref().and_then(|r| r.get("why_it_stopped").cloned()),
        "elapsed_ms": elapsed_ms,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "decode_tps": decode_tps,
        "tool_calls": tool_calls,
        "tool_errors": tool_errors,
        "files_changed": end.as_ref().map(|r| num_of(r, "files_changed")),
        "questions": { "asked": asked, "approved": approved, "denied": denied, "timed_out": timed_out, "cancelled": cancelled },
        "last_check": last_check,
        "limit": limit,
    });
    let rows_dropped = rows.len().saturating_sub(MAX_ROWS);
    let rows: Vec<Value> = rows.into_iter().skip(rows_dropped).map(Value::Object).collect();
    Transcript { rows, summary, last_seq, state, finished, rows_dropped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runs::events::{ApprovalOutcome, Event, RunState};

    /// An event as the log gives it back: prepared (masked, split by audience) and numbered.
    fn stored(seq: i64, event: Event) -> Stored {
        let p = event.prepare();
        Stored { id: 100 + seq, run_id: "run_t".into(), seq, ts: 1_000 + seq * 10, kind: p.kind.to_string(), data: p.public, ui: p.ui }
    }

    fn approval_flow() -> Vec<Stored> {
        let mut seq = 0;
        let mut next = |e: Event| {
            seq += 1;
            stored(seq, e)
        };
        vec![
            next(Event::RunStarted { session_id: Some("s1".into()), goal_head: "Fix the parser".into(), caller: "api".into(), route: "agent".into(), directory: Some("D:/proj".into()), started_ms: 1 }),
            next(Event::Status { state: RunState::Running, position: None, reason: None }),
            next(Event::Step { step: 1, phase: "plan".into(), title: "Plan with llama.cpp".into(), status: "running".into() }),
            next(Event::Step { step: 1, phase: "plan".into(), title: "Plan with llama.cpp".into(), status: "completed".into() }),
            next(Event::ToolStarted { call_id: 7, tool: "fs.edit".into(), args_head: r#"{"path":"src/parse.rs"}"#.into() }),
            next(Event::Status { state: RunState::WaitingApproval, position: None, reason: None }),
            next(Event::ApprovalOpened { request: json!({ "request_id": "c_1", "tool": "fs.edit", "preview": "--- a/src/parse.rs\n+++ b/src/parse.rs" }) }),
            next(Event::ApprovalClosed { request_id: "c_1".into(), outcome: ApprovalOutcome::Approved }),
            next(Event::Status { state: RunState::Running, position: None, reason: None }),
            next(Event::ToolFinished { call_id: 7, ok: true, status: "completed".into(), ms: 12, decision: "asked".into(), approval: "approved".into(), observation_head: "Edited src/parse.rs: 1 place".into() }),
            next(Event::ToolStarted { call_id: 8, tool: "check.run".into(), args_head: "{}".into() }),
            next(Event::ToolFinished { call_id: 8, ok: false, status: "failed".into(), ms: 900, decision: "allowed".into(), approval: String::new(), observation_head: (1..=25).map(|i| format!("error line {i}")).collect::<Vec<_>>().join("\n") }),
            next(Event::Usage { tokens_in: Some(1_000), tokens_out: Some(80), ttft_ms: Some(300), decode_tps: Some(6.5), ctx_used: Some(1_200), ctx_window: Some(6_144) }),
            next(Event::Usage { tokens_in: Some(1_400), tokens_out: Some(60), ttft_ms: None, decode_tps: Some(6.0), ctx_used: None, ctx_window: None }),
            next(Event::Text { index: 0, text: "I fixed the parser.".into() }),
            next(Event::Text { index: 1, text: "The last check still fails.".into() }),
            next(Event::RunFinished { status: "completed".into(), stopped_reason: "final".into(), answer_head: "I fixed the parser.".into(), why_it_stopped: None, files_changed: 1, ms: 4_500 }),
        ]
    }

    fn kinds(t: &Transcript) -> Vec<String> {
        t.rows.iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn the_events_of_an_approval_flow_become_the_rows_of_a_conversation() {
        let t = fold(&approval_flow(), true);
        assert_eq!(kinds(&t), ["user", "step", "tool", "approval", "tool", "assistant", "finished"]);
        assert_eq!(t.rows[0]["text"], "Fix the parser");
        assert_eq!(t.rows[0]["directory"], "D:/proj");
        // A phase said running and then completed is one row.
        assert_eq!((t.rows[1]["status"].as_str(), t.rows[1]["seq"].as_i64(), t.rows[1]["seq_end"].as_i64()), (Some("completed"), Some(3), Some(4)));
        // The call that waited for the person: its row has its end, and the question is a row of its own that says how it ended.
        assert_eq!((t.rows[2]["tool"].as_str(), t.rows[2]["status"].as_str(), t.rows[2]["ok"].as_bool(), t.rows[2]["approval"].as_str()), (Some("fs.edit"), Some("completed"), Some(true), Some("approved")));
        assert_eq!(t.rows[2]["args_head"], r#"{"path":"src/parse.rs"}"#);
        assert_eq!(t.rows[2]["output"]["head"], "Edited src/parse.rs: 1 place");
        assert_eq!((t.rows[3]["state"].as_str(), t.rows[3]["request_id"].as_str(), t.rows[3]["tool"].as_str()), (Some("approved"), Some("c_1"), Some("fs.edit")));
        // A failed call keeps its error, and a long output is one collapsed row with its first ten lines.
        assert_eq!((t.rows[4]["error"].as_bool(), t.rows[4]["output"]["lines"].as_i64(), t.rows[4]["output"]["collapsed"].as_bool()), (Some(true), Some(25), Some(true)));
        assert_eq!(t.rows[4]["output"]["head"].as_str().unwrap().lines().count(), OUTPUT_HEAD_LINES);
        assert_eq!(t.rows[5]["text"], "I fixed the parser.\nThe last check still fails.");
        assert_eq!(t.rows[6]["files_changed"], 1);
        assert!(t.finished && t.state == "running" && t.last_seq == 17 && t.rows_dropped == 0);
        // The summary card.
        let s = &t.summary;
        assert_eq!((s["status"].as_str(), s["stopped_reason"].as_str(), s["elapsed_ms"].as_i64(), s["files_changed"].as_i64()), (Some("completed"), Some("final"), Some(4_500), Some(1)));
        assert_eq!((s["tokens_in"].as_i64(), s["tokens_out"].as_i64(), s["tool_calls"].as_i64(), s["tool_errors"].as_i64()), (Some(2_400), Some(140), Some(2), Some(1)));
        assert_eq!((s["questions"]["asked"].as_i64(), s["questions"]["approved"].as_i64(), s["questions"]["denied"].as_i64()), (Some(1), Some(1), Some(0)));
        assert_eq!((s["last_check"]["tool"].as_str(), s["last_check"]["ok"].as_bool()), (Some("check.run"), Some(false)));
    }

    #[test]
    fn the_api_token_gets_no_row_that_shows_what_a_call_touched_or_what_was_asked() {
        let mut events = approval_flow();
        let finish = events.pop().unwrap();
        events.push(stored(17, Event::Question { text: "Which file?".into(), choices: vec!["a.rs".into(), "b.rs".into()] }));
        events.push(Stored { seq: 18, ..finish });
        let api = fold(&events, false);
        let wire = serde_json::to_string(&api.rows).unwrap();
        for hidden in ["args_head", "src/parse.rs", "Edited", "error line", "\"request\"", "preview", "Which file", "a.rs", "choices", "output"] {
            assert!(!wire.contains(hidden), "{hidden} is for the UI token: {wire}");
        }
        // What it is told: that a call was made, how it ended, that a question was asked and how it ended.
        assert_eq!(kinds(&api), ["user", "step", "tool", "approval", "tool", "assistant", "question", "finished"]);
        assert_eq!((api.rows[3]["state"].as_str(), api.rows[2]["tool"].as_str()), (Some("approved"), Some("fs.edit")));
        let ui = fold(&events, true);
        assert!(serde_json::to_string(&ui.rows).unwrap().contains("Which file"));
    }

    #[test]
    fn a_run_stopped_by_a_limit_has_a_limit_row_and_says_which() {
        let mut seq = 0;
        let mut next = |e: Event| {
            seq += 1;
            stored(seq, e)
        };
        let events = vec![
            next(Event::RunStarted { session_id: None, goal_head: "Big job".into(), caller: "ui".into(), route: "agent".into(), directory: None, started_ms: 1 }),
            next(Event::ToolStarted { call_id: 1, tool: "term.run".into(), args_head: "{}".into() }),
            next(Event::ToolFinished { call_id: 1, ok: true, status: "completed".into(), ms: 5, decision: "allowed".into(), approval: String::new(), observation_head: "ok".into() }),
            next(Event::Limit { stopped_by: "max_tool_calls".into(), used: 1, cap: 1 }),
            next(Event::RunFinished { status: "stopped".into(), stopped_reason: "max_tool_calls".into(), answer_head: String::new(), why_it_stopped: Some("I reached the limit of 1 tool call.".into()), files_changed: 0, ms: 80 }),
        ];
        let t = fold(&events, true);
        assert_eq!(kinds(&t), ["user", "tool", "limit", "finished"]);
        assert_eq!((t.rows[2]["stopped_by"].as_str(), t.rows[2]["used"].as_i64(), t.rows[2]["cap"].as_i64()), (Some("max_tool_calls"), Some(1), Some(1)));
        assert_eq!((t.summary["status"].as_str(), t.summary["limit"]["stopped_by"].as_str(), t.summary["why_it_stopped"].as_str()), (Some("stopped"), Some("max_tool_calls"), Some("I reached the limit of 1 tool call.")));
    }

    #[test]
    fn a_run_that_is_still_going_has_its_state_and_an_open_question_and_a_running_call() {
        let events: Vec<Stored> = approval_flow().into_iter().take(7).collect();
        let t = fold(&events, true);
        assert!(!t.finished);
        assert_eq!(t.state, "waiting_approval");
        assert_eq!((t.rows[2]["status"].as_str(), t.rows[3]["state"].as_str()), (Some("running"), Some("open")));
        assert_eq!(t.summary["status"], "waiting_approval", "no end yet: the summary says what the run is doing");
        assert_eq!(t.summary["questions"]["asked"], 1);
    }

    /// A client that was cut off asks again from where it was: overlapping pages and replayed events give the rows of the log read once, in any split.
    #[test]
    fn a_page_that_overlaps_or_a_replay_after_a_reconnect_gives_the_same_rows() {
        let all = approval_flow();
        let once = fold(&all, true);
        let overlapping: Vec<Stored> = all[..9].iter().chain(all[5..].iter()).cloned().collect();
        assert_eq!(fold(&overlapping, true), once, "events 6 to 9 came twice");
        let replayed: Vec<Stored> = all.iter().cloned().chain(all[10..].iter().cloned()).collect();
        assert_eq!(fold(&replayed, true), once, "the tail of the log came again after the end");
        // Pages of three, each starting a little before the last ended: the same.
        let mut seen: Vec<Stored> = Vec::new();
        let mut from = 0;
        while from < all.len() {
            let to = (from + 3).min(all.len());
            seen.extend(all[from.saturating_sub(1)..to].iter().cloned());
            from = to;
        }
        assert_eq!(fold(&seen, true), once);
        // Folding what was read so far and then the rest is not the same thing as folding both (a row is made from events of both), which is why the route reads the whole log.
        assert!(fold(&all[..7], true).rows.len() < once.rows.len());
    }

    /// A call whose start was not read (a page that begins after it) still gets its row, from its end.
    #[test]
    fn a_call_whose_start_is_missing_is_a_row_made_from_its_end() {
        let all = approval_flow();
        let tail: Vec<Stored> = all[9..].to_vec();
        let t = fold(&tail, true);
        assert_eq!(t.rows[0]["kind"], "tool");
        assert_eq!((t.rows[0]["call_id"].as_i64(), t.rows[0]["ok"].as_bool()), (Some(7), Some(true)));
    }

    /// What a call returned is text for the screen to show as text: markup and diffs in it are strings, cut to ten lines, and a key in it was masked when the event was written.
    #[test]
    fn hostile_text_stays_text_and_is_cut_and_masked() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let hostile = format!("<script>alert(1)</script>\n[x](javascript:alert(1))\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n```\nlast {secret}\n{}", (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
        let events = vec![
            stored(1, Event::ToolStarted { call_id: 1, tool: "term.run".into(), args_head: "{}".into() }),
            stored(2, Event::ToolFinished { call_id: 1, ok: true, status: "completed".into(), ms: 1, decision: "allowed".into(), approval: String::new(), observation_head: hostile }),
        ];
        let t = fold(&events, true);
        let head = t.rows[0]["output"]["head"].as_str().unwrap();
        assert!(head.starts_with("<script>alert(1)</script>\n[x](javascript:alert(1))\n--- a/x"), "{head}");
        assert_eq!(head.lines().count(), OUTPUT_HEAD_LINES);
        assert!(t.rows[0]["output"]["collapsed"].as_bool().unwrap());
        let wire = serde_json::to_string(&t.rows).unwrap();
        assert!(!wire.contains(secret), "masked when the event was written: {wire}");
        assert!(wire.contains("REDACTED"));
    }

    #[test]
    fn a_very_long_run_keeps_its_newest_rows_and_says_how_many_went() {
        let mut events = Vec::new();
        for i in 1..=(MAX_ROWS as i64 + 25) {
            events.push(stored(i, Event::Limit { stopped_by: "x".into(), used: 1, cap: 2 }));
        }
        let t = fold(&events, true);
        assert_eq!((t.rows.len(), t.rows_dropped), (MAX_ROWS, 25));
        assert_eq!(t.rows[0]["seq"], 26);
    }

    #[test]
    fn an_empty_log_is_an_empty_transcript() {
        let t = fold(&[], false);
        assert!(t.rows.is_empty() && !t.finished && t.last_seq == 0 && t.summary["status"] == "running" && t.summary["elapsed_ms"] == 0);
    }
}
