//! The events of a run, as a type (card OA10-01; tasks EX-03 and UX-02 of the OpenCode audit).
//!
//! A run says what it does as rows of `run_events` in `data/agent_runs.db`: one [`Event`] per row, numbered inside the run (`seq`, 1, 2, 3 ... with no gap) and across all runs
//! (`id`, which only ever grows). The routes of `api_gateway::run_api` read the rows back as a page (`GET /api/v1/runs/{id}/events`) or as a stream that can be taken up again
//! from any number (`GET /api/v1/runs/{id}/stream?after=`, `GET /api/v1/events?after=`), so a client that hung up, or came late, misses nothing and sees the order everybody sees.
//!
//! * **One type.** [`Event`] is the whole vocabulary; [`Event::TYPES`] lists the names, `documentation/RUN_EVENTS.md` describes each, and a test keeps the two together. A row holds
//!   the event as [`Event::prepare`] made it: every string masked with the tool host's [`Redactor`], and the whole row at most [`MAX_EVENT_BYTES`] (the longest text is cut with an
//!   ellipsis and `"truncated": true` is set).
//! * **Two audiences.** The fields that show what a call was given or what it returned (a tool's arguments and result, the exact action of a question, what the planner asked) are
//!   kept apart as the `ui` part of the row ([`ui_fields`]); [`Stored::wire`] adds them for the UI token and leaves them out for the API token, so the API token's stream tells it what
//!   the run is doing and never what it touched (the rule of `stream_events::step_for_scope`, which this file's [`PhaseView`] now makes).
//! * **Not rows.** `heartbeat`, `end` and `server.connected` are frames of a stream, not events of a run: they are in the type so that the page describes them, and they are never stored.
//!
//! Nothing here can fail a run: a row that cannot be written is logged once and the run goes on (the log is a record, not a gate).

use crate::modules::browser_agent::AgentStep;
use aqua_tool_host::output::Redactor;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// The version of the wire format: every event carries it as `"v"`.
pub const VERSION: u32 = 1;
/// The most a row may take (the public part and the `ui` part together, as text).
pub const MAX_EVENT_BYTES: usize = 16 * 1024;
/// Text pieces of an answer: how many at most, and how long each.
const MAX_TEXT_PIECES: usize = 40;
const TEXT_PIECE_CHARS: usize = 600;
/// A phase's title, status and phase are cut at this many characters.
const PHASE_FIELD_CHARS: usize = 200;
/// Choices of a question that are kept.
const MAX_CHOICES: usize = 8;

/// Names that belong to the envelope of an event on the wire; a field of an event may not use one.
pub const RESERVED_FIELDS: [&str; 6] = ["v", "id", "seq", "run_id", "ts", "type"];

/// Whether the log is written. On unless `AQUA_RUN_EVENTS` says `off`, `0`, `false` or `no`: it is a record kept beside the run, and the one switch lets the person stop it.
/// Read on every call, like `AQUA_RUN_CAPTURE`.
pub fn enabled() -> bool {
    switch_allows(std::env::var("AQUA_RUN_EVENTS").ok().as_deref())
}

/// What the value of `AQUA_RUN_EVENTS` (unset: `None`) means: the log is written unless it says off.
pub fn switch_allows(value: Option<&str>) -> bool {
    !matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("off" | "0" | "false" | "no"))
}

/// What a run is doing, for a `status` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    WaitingApproval,
    WaitingUser,
    Retry,
    Idle,
}

/// How a question to the person ended, for an `approval.closed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Approved,
    ApprovedSession,
    Denied,
    Timeout,
    Cancelled,
}

impl ApprovalOutcome {
    /// The word on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalOutcome::Approved => "approved",
            ApprovalOutcome::ApprovedSession => "approved_session",
            ApprovalOutcome::Denied => "denied",
            ApprovalOutcome::Timeout => "timeout",
            ApprovalOutcome::Cancelled => "cancelled",
        }
    }

    /// From the word the confirmation broker uses (`aqua_mcp_adapters::browser::QuestionNote::Closed`); an unknown word is a denial.
    pub fn from_word(word: &str) -> Self {
        match word {
            "approved" => ApprovalOutcome::Approved,
            "approved_session" => ApprovalOutcome::ApprovedSession,
            "timeout" => ApprovalOutcome::Timeout,
            "cancelled" => ApprovalOutcome::Cancelled,
            _ => ApprovalOutcome::Denied,
        }
    }
}

/// Everything a run, or a stream of runs, can tell. The `type` on the wire is the name given to each variant.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    /// The run began.
    #[serde(rename = "run.started")]
    RunStarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        goal_head: String,
        caller: String,
        route: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
        started_ms: i64,
    },
    /// What the run is doing now.
    #[serde(rename = "status")]
    Status {
        state: RunState,
        #[serde(skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A phase of the run (the title the app shows: "Calling fs.read", "Plan with llama.cpp") and whether it is `running`, `completed` or `failed`.
    #[serde(rename = "step")]
    Step { step: u32, phase: String, title: String, status: String },
    /// One sentence of the answer, in order; no sound.
    #[serde(rename = "text")]
    Text { index: u32, text: String },
    /// A tool call is about to be made. `args_head` (the arguments, with bodies and secrets masked) is for the UI token only.
    #[serde(rename = "tool.started")]
    ToolStarted { call_id: i64, tool: String, args_head: String },
    /// A tool call came back. `observation_head` (the first characters of what it returned) is for the UI token only.
    #[serde(rename = "tool.finished")]
    ToolFinished { call_id: i64, ok: bool, status: String, ms: i64, decision: String, approval: String, observation_head: String },
    /// What a call that is still running has printed lately (card OA7-04, TM-14): the lines of up to a quarter of a second, masked like a result. A frame of the UI token's streams only,
    /// never a row of the log (a build that prints for ten minutes would fill it): a client that was not connected misses what it was not there for. The model never reads it.
    #[serde(rename = "tool.progress")]
    ToolProgress { call_id: i64, piece: u64, text: String, skipped: u64 },
    /// A question to the person is open. All of it is for the UI token: it shows the exact action and its target.
    #[serde(rename = "approval.opened")]
    ApprovalOpened { request: Value },
    /// The question ended.
    #[serde(rename = "approval.closed")]
    ApprovalClosed { request_id: String, outcome: ApprovalOutcome },
    /// What the model server measured for one reply.
    #[serde(rename = "usage")]
    Usage {
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_in: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens_out: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ttft_ms: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        decode_tps: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ctx_used: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ctx_window: Option<u32>,
    },
    /// A cap of the autonomy setting ended the run (`max_seconds`, `max_tool_calls`, `max_files_changed`, `max_changed_bytes`).
    #[serde(rename = "limit")]
    Limit { stopped_by: String, used: u64, cap: u64 },
    /// The planner asked the person something and the run waits for the answer. All of it is for the UI token.
    #[serde(rename = "question")]
    Question { text: String, choices: Vec<String> },
    /// The run ended; nothing follows it in the log.
    #[serde(rename = "run.finished")]
    RunFinished {
        status: String,
        stopped_reason: String,
        answer_head: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        why_it_stopped: Option<String>,
        files_changed: u64,
        ms: i64,
    },
    /// The run failed in a way that has a name. A stop by the person is `run.finished` with the status `cancelled`, not this.
    #[serde(rename = "error")]
    Error { code: String, message: String, retryable: bool },
    /// A frame of a stream every ten seconds when nothing else was sent, so that a client can tell a quiet run from a dead connection. Never stored.
    #[serde(rename = "heartbeat")]
    Heartbeat,
    /// The last frame of the stream of a run that has ended: `last` is the number of its last event. Never stored.
    #[serde(rename = "end")]
    End { last: i64 },
    /// The first frame of the global feed; `last` is the number of the newest event in the log at that moment. Never stored.
    #[serde(rename = "server.connected")]
    Connected { last: i64 },
}

impl Event {
    /// Every `type` on the wire, in the order `documentation/RUN_EVENTS.md` describes them.
    pub const TYPES: [&'static str; 16] =
        ["run.started", "status", "step", "text", "tool.started", "tool.finished", "tool.progress", "approval.opened", "approval.closed", "usage", "limit", "question", "run.finished", "error", "heartbeat", "end"];

    /// The `type` of this event.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::RunStarted { .. } => "run.started",
            Event::Status { .. } => "status",
            Event::Step { .. } => "step",
            Event::Text { .. } => "text",
            Event::ToolStarted { .. } => "tool.started",
            Event::ToolFinished { .. } => "tool.finished",
            Event::ToolProgress { .. } => "tool.progress",
            Event::ApprovalOpened { .. } => "approval.opened",
            Event::ApprovalClosed { .. } => "approval.closed",
            Event::Usage { .. } => "usage",
            Event::Limit { .. } => "limit",
            Event::Question { .. } => "question",
            Event::RunFinished { .. } => "run.finished",
            Event::Error { .. } => "error",
            Event::Heartbeat => "heartbeat",
            Event::End { .. } => "end",
            Event::Connected { .. } => "server.connected",
        }
    }

    /// An event that is a row of the log. `heartbeat`, `end`, `server.connected` and `tool.progress` are frames of a stream and are never written.
    pub fn is_logged(&self) -> bool {
        !matches!(self, Event::Heartbeat | Event::End { .. } | Event::Connected { .. } | Event::ToolProgress { .. })
    }

    /// The event with its fields masked and cut, split into the part everybody gets and the part only the UI token gets.
    pub fn prepare(&self) -> Prepared {
        let kind = self.kind();
        let mut all = match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        all.remove("type");
        let mut ui = Map::new();
        for name in ui_fields(kind) {
            if let Some(value) = all.remove(*name) {
                ui.insert((*name).to_string(), value);
            }
        }
        let redactor = Redactor::new();
        let mut public = redact_map(&redactor, all);
        let mut ui = redact_map(&redactor, ui);
        if fit(&mut public, &mut ui) {
            public.insert("truncated".into(), Value::Bool(true));
        }
        Prepared { kind, public, ui }
    }
}

/// The fields of an event of this `type` that only the UI token is sent.
pub fn ui_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "tool.started" => &["args_head"],
        "tool.finished" => &["observation_head"],
        "approval.opened" => &["request"],
        "question" => &["text", "choices"],
        _ => &[],
    }
}

/// An event ready to be written: masked, cut, and split by audience.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub kind: &'static str,
    pub public: Map<String, Value>,
    pub ui: Map<String, Value>,
}

fn redact_map(redactor: &Redactor, map: Map<String, Value>) -> Map<String, Value> {
    match redactor.redact_json(&Value::Object(map)) {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn size_of(public: &Map<String, Value>, ui: &Map<String, Value>) -> usize {
    Value::Object(public.clone()).to_string().len() + if ui.is_empty() { 0 } else { Value::Object(ui.clone()).to_string().len() }
}

/// Cuts the row down to [`MAX_EVENT_BYTES`]: again and again the longest text anywhere in it is halved (with an ellipsis). `true` when anything was cut. A row that cannot be brought under
/// the limit this way (thousands of short texts) loses its `ui` part, and as a last resort its public part: the number and the type of the event still stand.
fn fit(public: &mut Map<String, Value>, ui: &mut Map<String, Value>) -> bool {
    let limit = MAX_EVENT_BYTES - 32;
    let mut cut = false;
    for _ in 0..64 {
        if size_of(public, ui) <= limit {
            return cut;
        }
        let (a, b) = (longest_text(&Value::Object(public.clone())), longest_text(&Value::Object(ui.clone())));
        let target = a.max(b);
        if target < 32 {
            break;
        }
        let done = if a >= b { halve_first(public, target) } else { halve_first(ui, target) };
        if !done {
            break;
        }
        cut = true;
    }
    if size_of(public, ui) > limit {
        ui.clear();
        cut = true;
    }
    if size_of(public, ui) > limit {
        public.clear();
        cut = true;
    }
    cut
}

fn longest_text(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.iter().map(longest_text).max().unwrap_or(0),
        Value::Object(o) => o.values().map(longest_text).max().unwrap_or(0),
        _ => 0,
    }
}

fn halve_first(map: &mut Map<String, Value>, len: usize) -> bool {
    map.values_mut().any(|v| halve_in(v, len))
}

fn halve_in(v: &mut Value, len: usize) -> bool {
    match v {
        Value::String(s) if s.chars().count() == len => {
            let keep = (len / 2).max(1);
            let mut cut: String = s.chars().take(keep).collect();
            cut.push('…');
            *s = cut;
            true
        }
        Value::Array(a) => a.iter_mut().any(|x| halve_in(x, len)),
        Value::Object(o) => o.values_mut().any(|x| halve_in(x, len)),
        _ => false,
    }
}

/// `text` cut at `max` characters, with an ellipsis when it was longer.
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// A row of the log as it was read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    /// The number across all runs; only grows, also after rows are deleted.
    pub id: i64,
    pub run_id: String,
    /// The number inside the run: 1, 2, 3 ... with no gap.
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub data: Map<String, Value>,
    pub ui: Map<String, Value>,
}

impl Stored {
    /// The event as a client reads it: `{"v":1,"id":..,"seq":..,"run_id":..,"ts":..,"type":..,<fields>}`. The fields only the UI token is sent are added when `ui` is true.
    pub fn wire(&self, ui: bool) -> Value {
        let mut out = Map::new();
        out.insert("v".into(), json!(VERSION));
        out.insert("id".into(), json!(self.id));
        out.insert("seq".into(), json!(self.seq));
        out.insert("run_id".into(), json!(self.run_id));
        out.insert("ts".into(), json!(self.ts));
        out.insert("type".into(), json!(self.kind));
        let mut shown = vec![&self.data];
        if ui {
            shown.push(&self.ui);
        }
        for map in shown {
            for (k, v) in map.iter() {
                if !RESERVED_FIELDS.contains(&k.as_str()) {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(out)
    }
}

/// The frame of a stream that is not a row: its `type` and the same envelope, with no `seq`.
pub fn frame(event: &Event) -> Value {
    let mut out = match serde_json::to_value(event) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    out.insert("v".into(), json!(VERSION));
    Value::Object(out)
}

/// The notice that tells a waiting stream the log has grown. A reader arms [`Notified`](tokio::sync::futures::Notified) with `enable()` **before** it reads, then reads, then waits:
/// an append between the read and the wait still wakes it.
static GREW: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The log of any run has a new row.
pub fn changed() -> &'static tokio::sync::Notify {
    &GREW
}

pub(crate) fn wake() {
    GREW.notify_waiters();
}

/// The frame of a stream for one piece of live output (card OA7-04): the envelope of a frame, the run it is for (the app's feed carries every run's), and the fields of
/// [`Event::ToolProgress`]. It has no `seq` and no `id:` (it is no row), so it never moves the number a client takes the stream up again from.
pub fn progress_frame(run_id: &str, event: &Event) -> Value {
    let mut frame = frame(event);
    if let Value::Object(map) = &mut frame {
        map.insert("run_id".into(), json!(run_id));
    }
    frame
}

/// What the reducer makes of one step of the agent loop, for everything that tells a client what the agent is doing: the `step` event of the log, the `step` of the agent stream for
/// the API token (`stream_events::step_for_scope`), and the `step` of the voice turn (`pipeline::step_event`). One function, so the three cannot drift apart. It holds the number
/// and kind of the step and, **as text and numbers only**, the title, status, phase and progress of a phase. A tool's arguments, what it returned and the loop's own `message`
/// (which for some phases quotes an observation) are not in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseView {
    pub step: u32,
    pub kind: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub phase: Option<String>,
    pub progress: Option<(u64, u64)>,
}

impl PhaseView {
    pub fn of(step: &AgentStep) -> Self {
        let text = |key: &str| step.detail.get(key).and_then(|v| v.as_str()).map(|t| t.chars().take(PHASE_FIELD_CHARS).collect::<String>());
        let progress = step.detail.get("progress").and_then(|p| Some((p.get("current")?.as_u64()?, p.get("total")?.as_u64()?)));
        PhaseView { step: step.step, kind: step.kind.clone(), title: text("title"), status: text("status"), phase: text("phase"), progress }
    }

    /// The step as the API token's stream sends it: number, kind, and whichever of title, status, phase and progress it has.
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("step".into(), self.step.into());
        out.insert("kind".into(), self.kind.clone().into());
        for (key, value) in [("title", &self.title), ("status", &self.status), ("phase", &self.phase)] {
            if let Some(text) = value {
                out.insert(key.into(), text.clone().into());
            }
        }
        if let Some((current, total)) = self.progress {
            out.insert("progress".into(), json!({ "current": current, "total": total }));
        }
        Value::Object(out)
    }

    /// The `step` event of the log: a step with a title and a status. (A tool call or a reply of the model has none; they are `tool.*` and `usage` events.)
    pub fn step_event(&self) -> Option<Event> {
        let (title, status) = (self.title.clone()?, self.status.clone()?);
        Some(Event::Step { step: self.step, phase: self.phase.clone().unwrap_or_else(|| self.kind.clone()), title, status })
    }
}

/// The answer cut into the sentences the `text` events carry: at most [`MAX_TEXT_PIECES`], each at most [`TEXT_PIECE_CHARS`] long. A sentence ends at `.`, `!`, `?` or `…` followed by
/// a space, and at a line break; what is left over when the limit is reached is joined to the last piece, which is cut like any other.
pub fn text_pieces(answer: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = answer.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '\n' || *c == '\r' {
            if !current.trim().is_empty() {
                pieces.push(current.trim().to_string());
            }
            current.clear();
            continue;
        }
        current.push(*c);
        let ends = matches!(*c, '.' | '!' | '?' | '…' | '。') && chars.get(i + 1).map_or(true, |n| n.is_whitespace());
        if ends {
            if !current.trim().is_empty() {
                pieces.push(current.trim().to_string());
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current.trim().to_string());
    }
    if pieces.len() > MAX_TEXT_PIECES {
        let rest = pieces.split_off(MAX_TEXT_PIECES - 1).join(" ");
        pieces.push(rest);
    }
    pieces.into_iter().map(|p| cut(&p, TEXT_PIECE_CHARS)).collect()
}

/// At most [`MAX_CHOICES`] choices of a question, each cut.
pub fn question_choices(choices: &[String]) -> Vec<String> {
    choices.iter().take(MAX_CHOICES).map(|c| cut(c, 200)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Event> {
        vec![
            Event::RunStarted { session_id: None, goal_head: "g".into(), caller: "api".into(), route: "agent".into(), directory: None, started_ms: 1 },
            Event::Status { state: RunState::WaitingApproval, position: None, reason: None },
            Event::Step { step: 1, phase: "plan".into(), title: "Plan".into(), status: "running".into() },
            Event::Text { index: 0, text: "Done.".into() },
            Event::ToolStarted { call_id: 1, tool: "fs.read".into(), args_head: "{}".into() },
            Event::ToolFinished { call_id: 1, ok: true, status: "completed".into(), ms: 3, decision: "allowed".into(), approval: String::new(), observation_head: "x".into() },
            Event::ToolProgress { call_id: 1, piece: 3, text: "Compiling aqua-core".into(), skipped: 0 },
            Event::ApprovalOpened { request: json!({"request_id": "c_1"}) },
            Event::ApprovalClosed { request_id: "c_1".into(), outcome: ApprovalOutcome::ApprovedSession },
            Event::Usage { tokens_in: Some(1), tokens_out: Some(2), ttft_ms: Some(3), decode_tps: Some(4.5), ctx_used: Some(1), ctx_window: Some(6144) },
            Event::Limit { stopped_by: "max_tool_calls".into(), used: 5, cap: 5 },
            Event::Question { text: "which one?".into(), choices: vec!["a".into(), "b".into()] },
            Event::RunFinished { status: "completed".into(), stopped_reason: "final".into(), answer_head: "ok".into(), why_it_stopped: None, files_changed: 0, ms: 9 },
            Event::Error { code: "internal".into(), message: "boom".into(), retryable: false },
            Event::Heartbeat,
            Event::End { last: 3 },
            Event::Connected { last: 3 },
        ]
    }

    #[test]
    fn every_kind_of_event_has_its_name_in_the_list_the_page_is_checked_against_and_the_name_on_the_wire_is_that_name() {
        let mut seen = Vec::new();
        for e in samples() {
            let wire = serde_json::to_value(&e).unwrap();
            assert_eq!(wire["type"], e.kind(), "the serde name and kind() are one name");
            seen.push(e.kind());
        }
        // `server.connected` belongs to the global feed only; the other fifteen are the page's list.
        let mut listed: Vec<&str> = Event::TYPES.to_vec();
        listed.push("server.connected");
        assert_eq!(seen.len(), listed.len());
        let mut a = seen.clone();
        let mut b = listed.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "a kind added to the type and not to the list (or the other way) is a page that lies");
        a.dedup();
        assert_eq!(a.len(), seen.len(), "two kinds with one name");
    }

    /// The page `RUN_EVENTS.md` describes the events the type has: a row of its table for each kind, and no row for a kind that is not there.
    #[test]
    fn the_page_describes_every_kind_of_event_and_only_those() {
        const PAGE: &str = include_str!("../../../documentation/RUN_EVENTS.md");
        let section = PAGE.split("## 2. The kinds").nth(1).and_then(|s| s.split("## 3.").next()).expect("section 2 of the page");
        let rows: Vec<&str> = section.lines().filter_map(|l| l.strip_prefix("| `")).filter_map(|l| l.split('`').next()).filter(|name| *name != "type").collect();
        let mut described = rows.clone();
        described.sort_unstable();
        let mut known = Event::TYPES.to_vec();
        known.sort_unstable();
        assert_eq!(described, known, "the rows of section 2 of documentation/RUN_EVENTS.md are the kinds of Event::TYPES");
        assert!(PAGE.contains("`server.connected`"), "the frame of the app's feed is described too");
        // The fields marked UI on the page are the ones the type keeps apart.
        for kind in Event::TYPES {
            let row = section.lines().find(|l| l.starts_with(&format!("| `{kind}`"))).unwrap();
            for field in ui_fields(kind) {
                assert!(row.contains(&format!("**UI** `{field}`")) || row.contains(&format!("`{field}`")), "{kind}: the page does not mark `{field}` as the UI token's");
            }
            assert_eq!(row.contains("**UI**"), !ui_fields(kind).is_empty(), "{kind}: the page and ui_fields() disagree about whether it has a UI-only field");
        }
    }

    #[test]
    fn no_field_of_an_event_uses_a_name_of_the_envelope() {
        for e in samples() {
            let p = e.prepare();
            for key in p.public.keys().chain(p.ui.keys()) {
                assert!(!RESERVED_FIELDS.contains(&key.as_str()), "{} has a field called {key}", e.kind());
            }
        }
    }

    #[test]
    fn the_fields_that_show_what_a_call_touched_are_kept_apart_for_the_ui_token() {
        let started = Event::ToolStarted { call_id: 7, tool: "fs.read".into(), args_head: r#"{"path":"C:/private/notes.txt"}"#.into() }.prepare();
        assert_eq!(started.public.get("tool"), Some(&json!("fs.read")));
        assert!(started.public.get("args_head").is_none() && started.ui.get("args_head").is_some());
        let finished = Event::ToolFinished { call_id: 7, ok: true, status: "completed".into(), ms: 3, decision: "allowed".into(), approval: String::new(), observation_head: "the file said: hunter2".into() }.prepare();
        assert!(finished.public.get("observation_head").is_none() && finished.ui.get("observation_head").is_some());
        assert_eq!(finished.public.get("ms"), Some(&json!(3)));
        let opened = Event::ApprovalOpened { request: json!({"request_id": "c_1", "tool": "term.run"}) }.prepare();
        assert!(opened.public.is_empty() && opened.ui.contains_key("request"), "the whole question is the UI's");
        let asked = Event::Question { text: "which file?".into(), choices: vec!["a.txt".into()] }.prepare();
        assert!(asked.public.is_empty() && asked.ui.len() == 2);
        // Every other kind has nothing apart.
        for e in samples().into_iter().filter(|e| !matches!(e.kind(), "tool.started" | "tool.finished" | "approval.opened" | "question")) {
            assert!(e.prepare().ui.is_empty(), "{}", e.kind());
        }
    }

    #[test]
    fn the_wire_form_has_the_envelope_and_the_ui_fields_only_for_the_ui() {
        let row = Stored { id: 41, run_id: "run_a".into(), seq: 3, ts: 1_000, kind: "tool.started".into(), data: Event::ToolStarted { call_id: 2, tool: "fs.read".into(), args_head: "{\"path\":\"a\"}".into() }.prepare().public, ui: Event::ToolStarted { call_id: 2, tool: "fs.read".into(), args_head: "{\"path\":\"a\"}".into() }.prepare().ui };
        let api = row.wire(false);
        assert_eq!(api, json!({"v": 1, "id": 41, "seq": 3, "run_id": "run_a", "ts": 1000, "type": "tool.started", "call_id": 2, "tool": "fs.read"}));
        let ui = row.wire(true);
        assert_eq!(ui["args_head"], "{\"path\":\"a\"}");
        assert_eq!((ui["seq"].clone(), ui["v"].clone()), (json!(3), json!(1)));
    }

    #[test]
    fn every_string_of_an_event_is_masked_before_it_is_kept() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let e = Event::ToolFinished { call_id: 1, ok: true, status: "completed".into(), ms: 1, decision: "allowed".into(), approval: String::new(), observation_head: format!("token={secret} was echoed") }.prepare();
        let all = format!("{:?}", e);
        assert!(!all.contains(secret), "{all}");
        let e = Event::RunStarted { session_id: None, goal_head: format!("use password=hunter2secret99 and {secret}"), caller: "api".into(), route: "agent".into(), directory: None, started_ms: 1 }.prepare();
        let all = format!("{:?}", e);
        assert!(!all.contains(secret) && !all.contains("hunter2secret99"), "{all}");
        let e = Event::ApprovalOpened { request: json!({"tool": "web.fetch", "reason": format!("send {secret}"), "headers": {"Authorization": "Bearer abcdefghijklmnop123456"}}) }.prepare();
        let all = format!("{:?}", e);
        assert!(!all.contains(secret) && !all.contains("abcdefghijklmnop123456"), "{all}");
    }

    #[test]
    fn a_row_never_takes_more_than_sixteen_kib_and_says_when_it_was_cut() {
        let e = Event::ToolFinished { call_id: 1, ok: true, status: "completed".into(), ms: 1, decision: "allowed".into(), approval: String::new(), observation_head: "x".repeat(100_000) }.prepare();
        assert!(size_of(&e.public, &e.ui) <= MAX_EVENT_BYTES, "{}", size_of(&e.public, &e.ui));
        assert_eq!(e.public.get("truncated"), Some(&json!(true)));
        let kept = e.ui.get("observation_head").and_then(|v| v.as_str()).unwrap();
        assert!(kept.ends_with('…') && kept.starts_with("xxx"));
        // Two long texts: the longer one is cut first, the other may stay whole.
        let e = Event::Error { code: "internal".into(), message: "m".repeat(20_000), retryable: false }.prepare();
        assert!(size_of(&e.public, &e.ui) <= MAX_EVENT_BYTES);
        // A row that fits is not marked.
        let e = Event::Text { index: 0, text: "short".into() }.prepare();
        assert!(e.public.get("truncated").is_none());
        // Thousands of short texts cannot be halved to fit: the UI part goes, then the public part, and the event still exists.
        let many: Vec<String> = (0..4_000).map(|i| format!("choice-{i}")).collect();
        let e = Event::Question { text: "q".into(), choices: many }.prepare();
        assert!(size_of(&e.public, &e.ui) <= MAX_EVENT_BYTES);
        assert_eq!(e.public.get("truncated"), Some(&json!(true)));
    }

    #[test]
    fn a_phase_is_reduced_the_way_the_api_stream_and_the_voice_turn_always_reduced_it() {
        let phase = AgentStep { step: 2, kind: "phase".into(), detail: json!({"title": "Calling fs.read", "status": "running", "phase": "act", "message": "the page said hunter2", "progress": {"current": 2, "total": 6}, "arguments": {"path": "x"}}) };
        let view = PhaseView::of(&phase);
        assert_eq!(view.to_value(), json!({"step": 2, "kind": "phase", "title": "Calling fs.read", "status": "running", "phase": "act", "progress": {"current": 2, "total": 6}}));
        assert_eq!(view.step_event(), Some(Event::Step { step: 2, phase: "act".into(), title: "Calling fs.read".into(), status: "running".into() }));
        assert!(!view.to_value().to_string().contains("hunter2"));
        // No title or no status: no event; a progress that is not two numbers is dropped; a text is cut at 200 characters.
        let tool = AgentStep { step: 3, kind: "tool".into(), detail: json!({"tool": "fs.read", "status": "completed"}) };
        assert_eq!(PhaseView::of(&tool).step_event(), None);
        assert_eq!(PhaseView::of(&tool).to_value(), json!({"step": 3, "kind": "tool", "status": "completed"}));
        let odd = AgentStep { step: 4, kind: "phase".into(), detail: json!({"title": "t".repeat(300), "status": "running", "progress": {"current": "a page of text", "total": 2}}) };
        let v = PhaseView::of(&odd);
        assert_eq!((v.progress, v.title.as_deref().map(|t| t.chars().count())), (None, Some(200)));
        // With no `phase` the kind stands in for it.
        let llm = AgentStep { step: 5, kind: "llm".into(), detail: json!({"title": "llama.cpp (m)", "status": "completed"}) };
        assert_eq!(PhaseView::of(&llm).step_event(), Some(Event::Step { step: 5, phase: "llm".into(), title: "llama.cpp (m)".into(), status: "completed".into() }));
    }

    #[test]
    fn an_answer_is_cut_into_sentences_in_order_and_a_long_one_is_bounded() {
        assert_eq!(text_pieces("It says Project Aurora. Call Pune at 3.5 hours! Really? Yes"), vec!["It says Project Aurora.", "Call Pune at 3.5 hours!", "Really?", "Yes"]);
        assert_eq!(text_pieces("line one\nline two\r\n\r\nline three"), vec!["line one", "line two", "line three"]);
        assert!(text_pieces("   \n  ").is_empty());
        let long = (0..100).map(|i| format!("Sentence {i}.")).collect::<Vec<_>>().join(" ");
        let pieces = text_pieces(&long);
        assert_eq!(pieces.len(), MAX_TEXT_PIECES);
        assert!(pieces[MAX_TEXT_PIECES - 1].starts_with("Sentence 39. Sentence 40."), "what is left over is joined to the last piece");
        assert!(pieces[MAX_TEXT_PIECES - 1].chars().count() <= TEXT_PIECE_CHARS + 1, "and that piece is cut like any other");
        assert!(text_pieces(&"a".repeat(5_000))[0].chars().count() <= TEXT_PIECE_CHARS + 1);
    }

    #[test]
    fn the_switch_turns_the_log_off_only_when_it_says_so() {
        // The meaning of the value, without touching the environment every other test thread shares.
        for (value, on) in [("off", false), ("OFF", false), (" 0 ", false), ("false", false), ("no", false), ("on", true), ("", true), ("1", true), ("yes", true)] {
            assert_eq!(switch_allows(Some(value)), on, "{value:?}");
        }
        assert!(switch_allows(None), "not set: the log is written");
    }
}
