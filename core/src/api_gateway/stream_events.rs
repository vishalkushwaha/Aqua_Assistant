//! The events of a streamed turn that are not the answer (card R6-22, task T6-22.2): the `approval` event, so that the app is told at once that Aqua is waiting for a yes,
//! instead of asking `GET /api/v1/browser/confirmations` every second or two.
//!
//! The stream of `POST /api/v1/pipeline/text/stream` and `/pipeline/wav/stream` carries `heard`, `step`, `speech`, `done` and `error` (see the comment above `forward_events` in
//! `server.rs`). `approval` is one more: it is sent when a question that needs the person's answer is opened (a site that is not known, a send or a payment, a desktop tool) while
//! the turn runs, with the same data as one entry of `pending` in `GET /api/v1/browser/confirmations` (what the action is, what it can do, the rule that made it ask, when an answer is
//! too late). It does not answer anything: the answer is `POST /api/v1/browser/confirm`, with the UI token, as before.
//!
//! **The UI token only.** The question shows the exact action and its target, so a stream that was asked for with the API token is never sent one (the same rule as the list
//! of questions, card R6-12): [`approvals_for`] gives nothing for it.

use crate::modules::browser_agent::{AgentStep, McpAgentResult};
use aqua_mcp_adapters::browser::{ConfirmRequest, ConfirmationBroker};
use tokio::sync::broadcast;

/// The kinds of event a streamed turn sends (`POST /api/v1/pipeline/text/stream` and `/pipeline/wav/stream`), and the only names `server.rs` can give an event of those streams:
/// its `sse_event` takes one of these and nothing else, so a kind that is not listed here cannot be sent, and the test of the page the app is built from
/// (`core/tests/ui_contract_test.rs`) asks that every one listed has its heading in `documentation/UI_CONTRACT.md`. Before (card OA3-01, AQ-04) that test searched the server's
/// source for `sse_event("...")` to find the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamEvent {
    /// What was heard, once, as soon as the words are known (a voice turn).
    Heard,
    /// The turn is an agent run that is being recorded: which run, and where its events can be read again (card OA10-01). Sent once, before the first `step` of the run.
    Run,
    /// What the agent is doing: a title and a status.
    Step,
    /// A question that needs the person's answer (the UI token's stream only).
    Approval,
    /// One spoken piece, in order.
    Speech,
    /// The result; the stream ends after it.
    Done,
    /// The turn failed; no `done` follows.
    Error,
}

impl StreamEvent {
    /// Every kind, in the order the page describes them.
    pub const ALL: [StreamEvent; 7] = [StreamEvent::Heard, StreamEvent::Run, StreamEvent::Step, StreamEvent::Approval, StreamEvent::Speech, StreamEvent::Done, StreamEvent::Error];

    /// The `event:` name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            StreamEvent::Heard => "heard",
            StreamEvent::Run => "run",
            StreamEvent::Step => "step",
            StreamEvent::Approval => "approval",
            StreamEvent::Speech => "speech",
            StreamEvent::Done => "done",
            StreamEvent::Error => "error",
        }
    }
}

/// The names of [`StreamEvent::ALL`], for a reader that has no use for the type (a test of the page, a client generator).
pub const STREAM_EVENT_NAMES: &[&str] = &[
    StreamEvent::Heard.name(),
    StreamEvent::Run.name(),
    StreamEvent::Step.name(),
    StreamEvent::Approval.name(),
    StreamEvent::Speech.name(),
    StreamEvent::Done.name(),
    StreamEvent::Error.name(),
];

/// How the agent stream (`POST /api/v1/mcp/agent/stream`) tells its client what happens: the one place where an event of that stream is made, with the scope of the caller applied to
/// every one of them (card OA1-04, UX-01). Its handler holds the sender of the stream only through this, so a step, the result or a question cannot be sent whole to a caller that
/// should be sent less; before card OA3-01 (AQ-04) that was kept true by a test that searched the handler's source for the calls. The agent runs on a thread that is not async, so
/// the methods block while the stream's buffer is full; they say `false` once the client has gone.
#[derive(Clone)]
pub struct AgentStreamSink {
    ui: bool,
    tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>,
    /// Has the `run` event been sent? Shared by the clones of the sink (the run's steps come from another thread than the gate's).
    announced: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AgentStreamSink {
    /// `ui`: the caller holds the UI token (the person's own app). Any other caller gets the phases and the answer only.
    pub fn new(ui: bool, tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>) -> Self {
        Self { ui, tx, announced: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) }
    }

    fn send(&self, name: &str, body: serde_json::Value) -> bool {
        self.tx.blocking_send(axum::response::sse::Event::default().event(name).data(body.to_string())).is_ok()
    }

    /// A `step` event: the step whole for the UI token, reduced as [`step_for_scope`] does for any other. A step of a run that is being recorded carries the number of its place in the
    /// run's log as `id:` (card OA10-01), and the first one is preceded by a `run` event that says which run, so a client that is cut off can take the log up again after the last
    /// `id` it saw (`GET /api/v1/runs/{id}/stream?after=`) instead of starting over. The events and their bodies are what they always were.
    pub fn step(&self, step: &AgentStep) -> bool {
        let mut step = step.clone();
        let cursor = crate::agent_runs::take_cursor(&mut step);
        let mut ok = true;
        if let Some((run, _)) = &cursor {
            if !self.announced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                ok = self.send(StreamEvent::Run.name(), run_event_data(run));
            }
        }
        let event = axum::response::sse::Event::default().event("step").data(serde_json::json!({ "type": "step", "step": step_for_scope(&step, self.ui) }).to_string());
        let event = match cursor {
            Some((_, seq)) => event.id(seq.to_string()),
            None => event,
        };
        ok && self.tx.blocking_send(event).is_ok()
    }

    /// The final `done` event: the result whole for the UI token, with its steps reduced as [`result_for_scope`] does for any other.
    pub fn done(&self, result: &McpAgentResult) -> bool {
        self.send("done", serde_json::json!({ "type": "done", "result": result_for_scope(result, self.ui) }))
    }

    /// The final `error` event, when the run could not be made.
    pub fn error(&self, message: &str) -> bool {
        self.send("error", serde_json::json!({ "type": "error", "error": message, "code": super::server::describe_pipeline_fault(message).1.as_str() }))
    }

    /// The `confirmation` events of this stream: the questions the broker opens while the run goes on, for the UI token's stream only (see [`forward_confirmations`]).
    pub fn confirmations(&self, broker: &ConfirmationBroker) -> Option<tokio::task::JoinHandle<()>> {
        forward_confirmations(self.ui, broker, self.tx.clone())
    }
}

/// The questions this stream may be told of: those the broker opens from now on, when the stream is the person's own (the UI token), and nothing for any other.
pub fn approvals_for(ui: bool, broker: &ConfirmationBroker) -> Option<broadcast::Receiver<ConfirmRequest>> {
    ui.then(|| broker.subscribe())
}

/// One step of an agent run as the client that asked for it may see it (card OA1-04, UX-01). The person's own app (the UI token) gets the step whole, as it always did. A script
/// with the API token gets what the voice stream gives it, `step`, `kind`, and the `title`, `status`, `phase` and `progress` of a phase, and nothing else: a tool's arguments and what it
/// returned (a file, a page, a command's output) are not for it, and neither is the `message` of a step, which for some phases quotes an observation. A step with no title (a tool call,
/// the model's own words) is its number and kind only.
pub fn step_for_scope(step: &AgentStep, ui: bool) -> serde_json::Value {
    if ui {
        return serde_json::to_value(step).unwrap_or_else(|_| serde_json::json!({ "step": step.step, "kind": step.kind }));
    }
    // The one reduction of a step that every client that is not told everything is sent (card OA10-01): the log of the run, the voice turn and this stream make theirs from it.
    crate::agent_runs::events::PhaseView::of(step).to_value()
}

/// The data of a `run` event: which run is being recorded and where to read its events again (`GET /api/v1/runs/{id}/events`, `/stream?after=`).
pub fn run_event_data(run_id: &str) -> serde_json::Value {
    serde_json::json!({ "type": "run", "run_id": run_id, "events": format!("/api/v1/runs/{run_id}/events"), "stream": format!("/api/v1/runs/{run_id}/stream") })
}

/// The result of an agent run for the final `done` event: whole for the UI token; for any other the same, with each step reduced as [`step_for_scope`] does. The answer, the gate's
/// decision, the run id, the question and the sources stay: they are what the run says to whoever asked.
pub fn result_for_scope(result: &McpAgentResult, ui: bool) -> serde_json::Value {
    let mut out = serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({ "final_answer": result.final_answer }));
    if !ui {
        if let Some(object) = out.as_object_mut() {
            object.insert("steps".into(), serde_json::Value::Array(result.steps.iter().map(|s| step_for_scope(s, false)).collect()));
        }
    }
    out
}

/// The `confirmation` events of the agent stream (`POST /api/v1/mcp/agent/stream`): each question the broker opens while the run goes on, sent to `tx` until the stream is closed or the
/// broker is gone. `None` and nothing sent when the stream is not the person's own (`ui` false): the same rule as [`approvals_for`] (card OA1-04, UX-01). Before, this stream subscribed
/// whoever asked, and a script with the API token was told the exact action and target of every question the person was asked.
pub fn forward_confirmations(ui: bool, broker: &ConfirmationBroker, tx: tokio::sync::mpsc::Sender<axum::response::sse::Event>) -> Option<tokio::task::JoinHandle<()>> {
    let mut questions = approvals_for(ui, broker)?;
    Some(tokio::spawn(async move {
        loop {
            match questions.recv().await {
                Ok(request) => {
                    let data = serde_json::json!({ "type": "confirmation", "request": request });
                    if tx.send(axum::response::sse::Event::default().event("confirmation").data(data.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

/// The data of an `approval` event: the question as the list of questions shows it.
pub fn approval_data(question: &ConfirmRequest) -> serde_json::Value {
    serde_json::to_value(question).unwrap_or_else(|_| serde_json::json!({ "request_id": question.request_id }))
}

/// Hands each question that is opened to `send` (which says whether anybody is still listening) until nobody is, or the broker is gone, or the task is aborted. A client that
/// was too slow and missed some is not told: it takes the list of questions, which is always the whole truth.
pub fn forward_approvals(mut questions: broadcast::Receiver<ConfirmRequest>, send: impl Fn(serde_json::Value) -> bool + Send + 'static) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match questions.recv().await {
                Ok(question) => {
                    if !send(approval_data(&question)) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_mcp_adapters::browser::{ConfirmKind, Decision};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn tool_step() -> AgentStep {
        AgentStep { step: 3, kind: "tool".into(), detail: json!({ "tool": "fs.read", "arguments": { "path": "C:/secret/plan.txt" }, "observation": "the file said: hunter2", "status": "completed" }) }
    }

    fn phase_step() -> AgentStep {
        AgentStep {
            step: 2,
            kind: "phase".into(),
            detail: json!({ "id": "act", "phase": "act", "title": "Calling fs.read", "status": "running", "message": "Stopped by a limit: the page said hunter2", "next": "Done", "progress": { "current": 2, "total": 6 }, "arguments": { "path": "x" }, "observation": "hunter2" }),
        }
    }

    #[test]
    fn api_scope_step_has_no_arguments_or_observation() {
        let tool = step_for_scope(&tool_step(), false);
        assert_eq!(tool, json!({ "step": 3, "kind": "tool", "status": "completed" }), "a tool step is its number, kind and status");
        let phase = step_for_scope(&phase_step(), false);
        assert_eq!(phase, json!({ "step": 2, "kind": "phase", "title": "Calling fs.read", "status": "running", "phase": "act", "progress": { "current": 2, "total": 6 } }));
        for reduced in [&tool, &phase] {
            let text = reduced.to_string();
            for secret in ["arguments", "observation", "hunter2", "C:/secret", "message", "raw", "answer"] {
                assert!(!text.contains(secret), "{secret} is in {text}");
            }
        }
        // The model's own words (kind llm, no title) are its number and kind only.
        let llm = AgentStep { step: 1, kind: "llm".into(), detail: json!({ "raw": "{\"tool\":\"term.run\"} the planner's own words" }) };
        assert_eq!(step_for_scope(&llm, false), json!({ "step": 1, "kind": "llm" }));
        // A progress field that is not two numbers is not passed on.
        let odd = AgentStep { step: 4, kind: "phase".into(), detail: json!({ "title": "t", "progress": { "current": "a page of text", "total": 2 } }) };
        assert_eq!(step_for_scope(&odd, false), json!({ "step": 4, "kind": "phase", "title": "t" }));
    }

    #[test]
    fn ui_scope_step_is_unchanged() {
        for step in [tool_step(), phase_step()] {
            assert_eq!(step_for_scope(&step, true), serde_json::to_value(&step).unwrap());
        }
        assert_eq!(step_for_scope(&tool_step(), true)["detail"]["arguments"]["path"], "C:/secret/plan.txt");
    }

    #[test]
    fn the_final_result_keeps_the_answer_for_every_scope_and_the_steps_only_for_the_ui() {
        let result = McpAgentResult {
            final_answer: "The file says to call Pune.".into(),
            steps: vec![phase_step(), tool_step()],
            tool_rounds: 1,
            stopped_reason: "final".into(),
            gate: None,
            run_id: Some("run-1".into()),
            question: None,
            choices: Vec::new(),
            sources: Vec::new(),
        };
        let whole = result_for_scope(&result, true);
        assert_eq!(whole, serde_json::to_value(&result).unwrap(), "the UI gets the result as it always was");
        let reduced = result_for_scope(&result, false);
        assert_eq!((reduced["final_answer"].as_str(), reduced["run_id"].as_str(), reduced["tool_rounds"].as_u64()), (Some("The file says to call Pune."), Some("run-1"), Some(1)));
        assert_eq!(reduced["steps"].as_array().map(|s| s.len()), Some(2));
        let text = reduced.to_string();
        assert!(!text.contains("arguments") && !text.contains("observation") && !text.contains("hunter2") && !text.contains("C:/secret"), "{text}");
    }

    fn ask_in_the_background(broker: &Arc<ConfirmationBroker>, tool: &'static str) -> std::thread::JoinHandle<Decision> {
        let broker = broker.clone();
        std::thread::spawn(move || broker.request(ConfirmKind::Tool, tool, "run a command on this computer".into(), None, None, Duration::from_secs(20)).1)
    }

    #[tokio::test]
    async fn a_question_opened_while_the_turn_runs_reaches_the_ui_stream_as_an_approval_with_what_the_list_of_questions_shows() {
        let broker = Arc::new(ConfirmationBroker::default());
        let feed = approvals_for(true, &broker).expect("the person's own stream is told of questions");
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = forward_approvals(feed, move |data| tx.send(data).is_ok());
        let asker = ask_in_the_background(&broker, "term.run");
        let data = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.expect("the event came").expect("the stream is open");
        assert_eq!((data["kind"].as_str(), data["tool"].as_str()), (Some("tool"), Some("term.run")), "{data}");
        assert_eq!(data["reason"], "run a command on this computer");
        assert!(data["request_id"].as_str().is_some_and(|id| !id.is_empty()) && data["expires_unix_ms"].as_u64().is_some_and(|t| t > 0), "{data}");
        // It is the question the list of questions holds, so the answer to it is the one route there is.
        let listed = broker.pending();
        assert_eq!(listed.len(), 1);
        assert_eq!(data, approval_data(&listed[0]));
        assert_eq!(broker.resolve_typed(&listed[0].request_id, Decision::Deny), aqua_mcp_adapters::browser::Resolved::Applied);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        forwarder.abort();
    }

    /// Card OA1-04 (UX-01): the agent stream sent `confirmation` (the exact action and target of a question) to whoever asked. The person's own stream still gets it; the API token's gets nothing.
    #[tokio::test]
    async fn the_agent_stream_sends_a_confirmation_to_the_ui_token_and_none_to_the_api_token() {
        let broker = Arc::new(ConfirmationBroker::default());
        // The API token's stream: nothing is forwarded, the sender is dropped at once (the stream would end with the run), and a question that is opened and answered leaves no event.
        let (tx, mut api_rx) = tokio::sync::mpsc::channel(8);
        let handle = forward_confirmations(false, &broker, tx);
        assert!(handle.is_none(), "no forwarding task for a stream that may not be told");
        let (tx, mut ui_rx) = tokio::sync::mpsc::channel(8);
        let forwarder = forward_confirmations(true, &broker, tx).expect("the person's own stream is told of questions");
        let asker = ask_in_the_background(&broker, "term.run");
        let event = tokio::time::timeout(Duration::from_secs(10), ui_rx.recv()).await.expect("the event came").expect("the stream is open");
        let text = format!("{event:?}");
        assert!(text.contains("confirmation") && text.contains("term.run") && text.contains("request_id"), "{text}");
        let open = broker.pending();
        assert_eq!(open.len(), 1);
        broker.resolve_typed(&open[0].request_id, Decision::Deny);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        forwarder.abort();
        assert!(api_rx.try_recv().is_err(), "the API token's stream was told nothing, and its channel is closed");
        assert!(api_rx.recv().await.is_none(), "the API token's channel was closed by the call, not kept open for a forwarder");
    }

    /// Card OA3-01 (AQ-04): the kinds of event are a type, the list of their names is made from it, and no two have the same name.
    #[test]
    fn every_kind_of_event_a_streamed_turn_sends_has_a_name_of_its_own_and_the_list_of_names_is_all_of_them() {
        // A kind added to the type and not to this match does not compile: the list below has to be read again.
        for kind in StreamEvent::ALL {
            match kind {
                StreamEvent::Heard | StreamEvent::Run | StreamEvent::Step | StreamEvent::Approval | StreamEvent::Speech | StreamEvent::Done | StreamEvent::Error => {}
            }
        }
        let names: Vec<&str> = StreamEvent::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names, STREAM_EVENT_NAMES);
        assert_eq!(names, ["heard", "run", "step", "approval", "speech", "done", "error"], "the names are the wire's: a client reads them");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "two kinds with one name");
    }

    /// The events a sink made, as (name, id, data), read to the end of the stream they were sent on.
    async fn blocks_sent(rx: tokio::sync::mpsc::Receiver<axum::response::sse::Event>) -> Vec<(String, Option<String>, serde_json::Value)> {
        use axum::response::IntoResponse;
        use tokio_stream::StreamExt;
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
        let bytes = http_body_util::BodyExt::collect(axum::response::sse::Sse::new(stream).into_response().into_body()).await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes)
            .split("\n\n")
            .filter(|block| block.contains("data:"))
            .map(|block| {
                let name = block.lines().find_map(|l| l.strip_prefix("event:")).unwrap_or("message").trim().to_string();
                let id = block.lines().find_map(|l| l.strip_prefix("id:")).map(|i| i.trim().to_string());
                let data: String = block.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect::<Vec<_>>().join("\n");
                (name, id, serde_json::from_str(&data).unwrap_or(serde_json::Value::Null))
            })
            .collect()
    }

    /// The events a sink made, as (name, data), read to the end of the stream they were sent on.
    async fn events_sent(rx: tokio::sync::mpsc::Receiver<axum::response::sse::Event>) -> Vec<(String, serde_json::Value)> {
        blocks_sent(rx).await.into_iter().map(|(name, _, data)| (name, data)).collect()
    }

    fn a_result() -> McpAgentResult {
        McpAgentResult {
            final_answer: "The file says to call Pune.".into(),
            steps: vec![phase_step(), tool_step()],
            tool_rounds: 1,
            stopped_reason: "final".into(),
            gate: None,
            run_id: Some("run-1".into()),
            question: None,
            choices: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// Card OA3-01 (AQ-04): this was a search of the handler's source for `step_for_scope(`, `result_for_scope(` and `forward_confirmations(`. The handler holds the stream only through
    /// the sink, so what a caller is sent is what the sink makes: the same steps and the same result go to a script with the API token and to the app with the UI token, and only the
    /// first is reduced. The agent runs on a thread that is not async; so does this.
    #[tokio::test]
    async fn the_agent_stream_sends_the_ui_token_every_step_and_the_whole_result_and_the_api_token_the_phases_and_the_answer() {
        for ui in [false, true] {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let sink = AgentStreamSink::new(ui, tx);
            let worker = tokio::task::spawn_blocking(move || {
                assert!(sink.step(&phase_step()) && sink.step(&tool_step()), "ui={ui}");
                assert!(sink.done(&a_result()));
                assert!(sink.error("the run could not be made"));
            });
            worker.await.unwrap();
            let events = events_sent(rx).await;
            let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, ["step", "step", "done", "error"], "ui={ui}");
            assert_eq!((events[0].1["type"].as_str(), events[2].1["type"].as_str(), events[3].1["type"].as_str()), (Some("step"), Some("done"), Some("error")));
            assert_eq!(events[3].1["error"], "the run could not be made");
            let done = &events[2].1["result"];
            // Whoever asked has the answer, the run and the rounds.
            assert_eq!((done["final_answer"].as_str(), done["run_id"].as_str(), done["tool_rounds"].as_u64()), (Some("The file says to call Pune."), Some("run-1"), Some(1)), "ui={ui}");
            let everything = serde_json::to_string(&events).unwrap();
            if ui {
                assert_eq!(events[1].1["step"]["detail"]["arguments"]["path"], "C:/secret/plan.txt", "the person's own app is sent the step whole");
                assert_eq!(done["steps"][1]["detail"]["observation"], "the file said: hunter2");
            } else {
                assert_eq!(events[1].1["step"], json!({ "step": 3, "kind": "tool", "status": "completed" }), "a script is sent a tool step as its number, kind and status");
                assert_eq!(events[0].1["step"]["title"], "Calling fs.read");
                assert_eq!(done["steps"].as_array().map(|s| s.len()), Some(2), "the steps of the result are reduced one by one");
                for secret in ["arguments", "observation", "hunter2", "C:/secret", "Stopped by a limit"] {
                    assert!(!everything.contains(secret), "{secret} reached a caller that holds only the API token: {everything}");
                }
            }
        }
    }

    /// Card OA10-01 (UX-02): the agent stream is a shim over the log of the run. A step of a run that is being recorded carries the number of its place in the log as `id:`, the first is
    /// preceded by a `run` event that says which run, the bodies are what they always were (no mark in them), and a step before the run exists (the gate's) has no id.
    #[tokio::test]
    async fn a_step_of_a_recorded_run_is_sent_with_its_place_in_the_log_and_the_first_is_preceded_by_a_run_event() {
        use crate::agent_runs::{observe_step, tag_cursor, AgentRunStore, Recording};
        for ui in [true, false] {
            let dir = std::env::temp_dir().join(format!("aqua_stream_sink_{ui}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let sink = AgentStreamSink::new(ui, tx);
            let worker = tokio::task::spawn_blocking(move || {
                // Before the run exists: the gate.
                assert!(sink.step(&phase_step()));
                let store = Arc::new(AgentRunStore::open(dir.join("agent_runs.db")).unwrap());
                let rec = Recording::begin_in(store, "run-9", "g", "api", "agent", None).unwrap();
                // The loop: the record hears the step, then the copy that is sent is marked.
                for step in [phase_step(), tool_step()] {
                    observe_step(&step);
                    let mut sent = step.clone();
                    tag_cursor(&mut sent);
                    assert!(sink.step(&sent), "ui={ui}");
                }
                rec.finish("final", 1, "ok", &[]);
                let _ = std::fs::remove_dir_all(&dir);
            });
            worker.await.unwrap();
            let blocks = blocks_sent(rx).await;
            let names: Vec<&str> = blocks.iter().map(|(n, _, _)| n.as_str()).collect();
            assert_eq!(names, ["step", "run", "step", "step"], "ui={ui}: the run event comes once, before the first step that has a place in the log");
            assert_eq!(blocks[0].1, None, "a step before the run exists has no id");
            assert_eq!(blocks[1].1, None, "the run event is not a row of the log");
            assert_eq!(blocks[1].2, run_event_data("run-9"));
            assert_eq!(blocks[1].2["stream"], "/api/v1/runs/run-9/stream");
            // The phase made two rows after run.started and status (3), the tool step written whole made two more (tool.started 4, tool.finished 5).
            assert_eq!((blocks[2].1.as_deref(), blocks[3].1.as_deref()), (Some("3"), Some("5")), "ui={ui}");
            let everything = serde_json::to_string(&blocks).unwrap();
            assert!(!everything.contains("_cursor"), "the mark is taken off before the step is sent: {everything}");
            if ui {
                assert_eq!(blocks[3].2["step"]["detail"]["arguments"]["path"], "C:/secret/plan.txt", "the person's stream still has the step whole");
            } else {
                assert_eq!(blocks[3].2["step"], json!({ "step": 3, "kind": "tool", "status": "completed" }));
            }
        }
    }

    /// Card OA3-01 (AQ-04): and the questions. The sink forwards them to the UI token's stream and has nothing to forward for the API token's.
    #[tokio::test]
    async fn the_agent_stream_forwards_the_questions_of_the_run_to_the_ui_token_only() {
        let broker = Arc::new(ConfirmationBroker::default());
        let (tx, mut api_rx) = tokio::sync::mpsc::channel(8);
        assert!(AgentStreamSink::new(false, tx).confirmations(&broker).is_none(), "nothing to forward for a script with the API token");
        let (tx, mut ui_rx) = tokio::sync::mpsc::channel(8);
        let forwarder = AgentStreamSink::new(true, tx).confirmations(&broker).expect("the person's own stream is told of questions");
        let asker = ask_in_the_background(&broker, "term.run");
        let event = tokio::time::timeout(Duration::from_secs(10), ui_rx.recv()).await.expect("the event came").expect("the stream is open");
        let text = format!("{event:?}");
        assert!(text.contains("confirmation") && text.contains("term.run"), "{text}");
        let open = broker.pending();
        assert_eq!(open.len(), 1);
        broker.resolve_typed(&open[0].request_id, Decision::Deny);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
        forwarder.abort();
        assert!(api_rx.recv().await.is_none(), "the API token's stream was told nothing and its channel was closed");
    }

    #[test]
    fn a_stream_asked_for_with_the_api_token_is_never_told_of_a_question() {
        let broker = ConfirmationBroker::default();
        assert!(approvals_for(false, &broker).is_none(), "the question shows the exact action and its target: it is the UI's");
    }

    #[tokio::test]
    async fn the_forwarding_ends_when_nobody_is_listening_any_more() {
        let broker = Arc::new(ConfirmationBroker::default());
        let feed = approvals_for(true, &broker).unwrap();
        let (tx, seen) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        drop(seen); // the client hung up
        let forwarder = forward_approvals(feed, move |data| tx.send(data).is_ok());
        let asker = ask_in_the_background(&broker, "term.run");
        tokio::time::timeout(Duration::from_secs(10), forwarder).await.expect("the task ended by itself").unwrap();
        let open = broker.pending();
        assert_eq!(open.len(), 1, "the question itself is still there for the list of questions");
        broker.resolve_typed(&open[0].request_id, Decision::Deny);
        assert_eq!(asker.join().unwrap(), Decision::Deny);
    }
}
