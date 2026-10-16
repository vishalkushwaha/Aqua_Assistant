//! The editor protocol (card OA10-07; task UX-12 of the OpenCode audit): the agent is driven with scripted JSON-RPC messages on an in-memory standard input and the frames it writes are read
//! back, against the stub of the core in `tests.rs` (no model, no real core, no editor).

use super::*;
use crate::headless::acp::{self, bounded, clean, goal_from_prompt, iso8601, stop_reason, tool_kind, AcpIo, RpcError, Translator};
use std::io::{BufRead, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};

/// An input the test writes to while the agent reads it: what the editor types.
struct Feed {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for Feed {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let available = self.fill_buf()?;
            let n = available.len().min(out.len());
            out[..n].copy_from_slice(&available[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Feed {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(bytes) => {
                    self.buf = bytes;
                    self.pos = 0;
                }
                Err(_) => {
                    self.buf.clear();
                    self.pos = 0;
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The agent on a thread, and the editor's end of its streams.
struct Editor {
    to_agent: Option<Sender<Vec<u8>>>,
    out: Shared,
    err: Shared,
    agent: Option<std::thread::JoinHandle<i32>>,
    next_id: i64,
}

impl Editor {
    fn start(core: &str, data: &Path, extra: &[&str]) -> Editor {
        let (tx, rx) = channel::<Vec<u8>>();
        let (out, err) = (Shared::default(), Shared::default());
        let mut args: Vec<String> = vec!["acp".into(), "--core".into(), core.to_string(), "--data-dir".into(), data.to_string_lossy().to_string()];
        args.extend(extra.iter().map(|a| a.to_string()));
        let io = AcpIo { input: Box::new(Feed { rx, buf: Vec::new(), pos: 0 }), output: Box::new(out.clone()), log: Box::new(err.clone()), stdin_is_terminal: false, backoff_unit: Duration::from_millis(5) };
        let data = data.to_path_buf();
        let agent = std::thread::spawn(move || acp::run(&args, &data, io));
        Editor { to_agent: Some(tx), out, err, agent: Some(agent), next_id: 0 }
    }

    fn send_raw(&self, bytes: Vec<u8>) {
        self.to_agent.as_ref().unwrap().send(bytes).unwrap();
    }

    fn send(&self, message: Value) {
        self.send_raw(format!("{message}\n").into_bytes());
    }

    /// A request; its id.
    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params }));
        self.next_id
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Every frame the agent has written so far; each line must be one JSON-RPC 2.0 message.
    fn frames(&self) -> Vec<Value> {
        let text = String::from_utf8_lossy(&self.out.0.lock().unwrap()).to_string();
        text.lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("a line of the output is not JSON ({e}): {l:?}"));
                assert_eq!(v["jsonrpc"], "2.0", "{l}");
                v
            })
            .collect()
    }

    /// Waits (15 s at most) until `done` is true of the frames, and gives them.
    fn wait(&self, what: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let until = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let frames = self.frames();
            if done(&frames) {
                return frames;
            }
            assert!(std::time::Instant::now() < until, "waited for {what}: {frames:#?}\nlog: {}", String::from_utf8_lossy(&self.err.0.lock().unwrap()));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn answer(&self, id: i64) -> Value {
        self.wait(&format!("the answer to {id}"), |f| f.iter().any(|m| m["id"] == id && m.get("method").is_none())).into_iter().find(|m| m["id"] == id && m.get("method").is_none()).unwrap()
    }

    /// `initialize`, and a session in `cwd`: its id.
    fn open(&mut self, cwd: &str) -> String {
        let init = self.request("initialize", json!({ "protocolVersion": 1, "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": true } }));
        assert_eq!(self.answer(init)["result"]["protocolVersion"], 1);
        let new = self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }));
        self.answer(new)["result"]["sessionId"].as_str().expect("a session id").to_string()
    }

    /// The editor closes the input: the exit code.
    fn close(mut self) -> i32 {
        self.to_agent.take();
        self.agent.take().unwrap().join().unwrap()
    }
}

fn updates(frames: &[Value]) -> Vec<Value> {
    frames.iter().filter(|m| m["method"] == "session/update").map(|m| m["params"]["update"].clone()).collect()
}

fn kinds(frames: &[Value]) -> Vec<String> {
    updates(frames).iter().map(|u| u["sessionUpdate"].as_str().unwrap().to_string()).collect()
}

fn prompt_params(session: &str, text: &str) -> Value {
    json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] })
}

const CWD: &str = "E:\\work\\project";

// ---- the protocol --------------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn initialize_says_what_the_agent_takes_and_nothing_secret() {
    let data = data_dir("acp_init");
    let stub = Stub::start(|_, _| Reply::Json(404, json!({})));
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let id = editor.request("initialize", json!({ "protocolVersion": 7, "clientCapabilities": {} }));
    let result = editor.answer(id)["result"].clone();
    assert_eq!(result["protocolVersion"], 1, "the newest version it speaks, when the editor offers a newer");
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!((result["agentCapabilities"]["promptCapabilities"]["embeddedContext"].clone(), result["agentCapabilities"]["promptCapabilities"]["image"].clone()), (json!(true), json!(false)));
    assert!(result["agentCapabilities"]["sessionCapabilities"]["list"].is_object() && result["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
    assert_eq!(result["authMethods"], json!([]));
    assert_eq!(result["agentInfo"]["name"], "aqua");
    let text = serde_json::to_string(&editor.frames()).unwrap();
    assert!(!text.contains(TOKEN) && !text.contains(UI_TOKEN) && !text.contains("aqua_api_token"), "no token and no token file in the stream");
    assert_eq!(editor.close(), 0);
    assert!(stub.requests().is_empty(), "initialize asks the core nothing");
}

#[test]
fn a_prompt_runs_on_the_core_and_its_events_come_back_as_updates_with_a_waiting_approval_that_the_app_answers() {
    let data = data_dir("acp_prompt");
    let frames = vec![
        row(1, started()),
        row(2, json!({ "type": "step", "step": 1, "phase": "plan", "title": "Plan with llama.cpp", "status": "running" })),
        row(3, json!({ "type": "tool.started", "call_id": 1, "tool": "fs.read" })),
        row(4, json!({ "type": "tool.finished", "call_id": 1, "ok": true, "status": "completed", "ms": 5, "decision": "allowed", "approval": "" })),
        row(5, json!({ "type": "tool.started", "call_id": 2, "tool": "fs.edit" })),
        row(6, json!({ "type": "status", "state": "waiting_approval" })),
        row(7, json!({ "type": "approval.opened", "request": {} })),
        row(8, json!({ "type": "approval.closed", "request_id": "c_1", "outcome": "approved" })),
        row(9, json!({ "type": "status", "state": "running" })),
        row(10, json!({ "type": "tool.finished", "call_id": 2, "ok": true, "status": "completed", "ms": 80, "decision": "asked", "approval": "approved" })),
        row(11, json!({ "type": "usage", "tokens_in": 1000, "tokens_out": 80, "ctx_used": 1200, "ctx_window": 6144 })),
        row(12, json!({ "type": "text", "index": 1, "text": "I fixed it." })),
        row(13, finished("completed", "final")),
        end_frame(13),
    ];
    let stub = stub_with_stream(frames);
    let mut editor = Editor::start(&stub.url(), &data, &["--wait-approval", "90"]);
    let session = editor.open(CWD);
    let id = editor.request("session/prompt", json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "fix the parser" }, { "type": "resource", "resource": { "uri": "file:///E:/work/project/src/parse.rs", "text": "fn parse() {}", "mimeType": "text/rust" } }] }));
    let answer = editor.answer(id);
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let all = editor.frames();
    assert_eq!(kinds(&all), vec!["agent_thought_chunk", "tool_call", "tool_call_update", "tool_call", "tool_call_update", "tool_call_update", "tool_call_update", "usage_update", "agent_message_chunk"]);
    let ups = updates(&all);
    assert_eq!((ups[1]["toolCallId"].as_str(), ups[1]["title"].as_str(), ups[1]["kind"].as_str(), ups[1]["status"].as_str()), (Some("run_stub_1.1"), Some("fs.read"), Some("read"), Some("in_progress")));
    assert_eq!((ups[2]["toolCallId"].as_str(), ups[2]["status"].as_str()), (Some("run_stub_1.1"), Some("completed")));
    assert_eq!((ups[3]["title"].as_str(), ups[3]["kind"].as_str()), (Some("fs.edit"), Some("edit")));
    // The change waits: a tool call that is pending, and the words say where it is answered. Nothing was asked of the editor.
    assert_eq!((ups[4]["toolCallId"].as_str(), ups[4]["status"].as_str()), (Some("run_stub_1.2"), Some("pending")));
    assert!(ups[4]["content"][0]["content"]["text"].as_str().unwrap().contains("Waiting for your approval in the Aqua app"), "{}", ups[4]);
    assert_eq!(ups[5]["status"], "in_progress");
    assert_eq!((ups[6]["status"].as_str(), ups[6]["content"][0]["content"]["text"].as_str()), (Some("completed"), Some("Approval: approved.")));
    assert_eq!((ups[7]["used"].as_u64(), ups[7]["size"].as_u64()), (Some(1200), Some(6144)));
    assert_eq!(ups[8]["content"]["text"], "I fixed it.\n");
    for m in &all {
        assert!(m.get("method").is_none() || m["method"] == "session/update", "the agent sent the editor a request: {m}");
        assert_eq!(m["params"]["sessionId"].as_str().unwrap_or(session.as_str()), session);
    }
    // What the core was asked: the run, with the folder, the session, the coding profile, the question kept open as long as the person said, and the embedded context after the request.
    let start = stub.requests().into_iter().find(|r| r.method == "POST" && r.path == "/api/v1/runs").unwrap();
    let body: Value = serde_json::from_str(&start.body).unwrap();
    assert_eq!(body["goal"], "fix the parser\n\n[context from file:///E:/work/project/src/parse.rs]\nfn parse() {}");
    assert_eq!((body["directory"].as_str(), body["session_id"].as_str(), body["profile"].as_str(), body["approval_wait_secs"].as_u64()), (Some(CWD), Some(session.as_str()), Some("coding"), Some(90)));
    for r in stub.requests() {
        assert_eq!(r.auth, format!("bearer {}", TOKEN.to_ascii_lowercase()));
        assert!(!r.path.contains("confirm") && !r.path.contains("answer") && !r.path.contains("approv"), "the agent asked for {}", r.path);
    }
    let text = serde_json::to_string(&all).unwrap();
    assert!(!text.contains(TOKEN) && !text.contains(UI_TOKEN), "no token in the stream");
    assert_eq!(editor.close(), 0);
}

#[test]
fn the_editor_can_cancel_a_prompt_and_the_run_is_ended_on_the_core() {
    let data = data_dir("acp_cancel");
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.ends_with("/interrupt") {
            flag.store(true, Ordering::SeqCst);
            return Reply::Json(200, json!({ "run_id": "run_stub_1", "interrupted": true, "ended": false }));
        }
        if req.path.starts_with("/api/v1/runs/run_stub_1/stream") {
            let flag = flag.clone();
            return Reply::Stream(Box::new(move |c| {
                chunk(c, &row(1, started()));
                chunk(c, &row(2, json!({ "type": "tool.started", "call_id": 1, "tool": "term.run" })));
                let until = std::time::Instant::now() + Duration::from_secs(20);
                while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                chunk(c, &row(3, finished("cancelled", "cancelled")));
                chunk(c, &end_frame(3));
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let session = editor.open(CWD);
    let id = editor.request("session/prompt", prompt_params(&session, "run the build"));
    editor.wait("the first tool call", |f| kinds(f).contains(&"tool_call".to_string()));
    editor.notify("session/cancel", json!({ "sessionId": session }));
    let answer = editor.answer(id);
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    assert!(interrupted.load(Ordering::SeqCst) && stub.paths().contains(&"POST /api/v1/runs/run_stub_1/interrupt".to_string()), "{:?}", stub.paths());
    // The session can be prompted again.
    assert_eq!(editor.close(), 0);
}

#[test]
fn when_the_editor_closes_the_input_a_run_that_is_going_is_asked_to_end() {
    let data = data_dir("acp_eof");
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.ends_with("/interrupt") {
            flag.store(true, Ordering::SeqCst);
            return Reply::Json(200, json!({ "interrupted": true, "ended": false }));
        }
        let flag = flag.clone();
        Reply::Stream(Box::new(move |c| {
            chunk(c, &row(1, started()));
            let until = std::time::Instant::now() + Duration::from_secs(20);
            while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            chunk(c, &row(2, finished("cancelled", "cancelled")));
            chunk(c, &end_frame(2));
        }))
    });
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let session = editor.open(CWD);
    editor.request("session/prompt", prompt_params(&session, "do it"));
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !stub.paths().iter().any(|p| p.contains("/stream")) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(editor.close(), 0);
    assert!(interrupted.load(Ordering::SeqCst), "closing the input asked the core to end the run: {:?}", stub.paths());
}

#[test]
fn a_limit_a_failure_and_a_question_are_said_in_the_chat_and_the_stop_reason_follows() {
    let data = data_dir("acp_stops");
    for (frames, reason, said) in [
        (vec![row(1, started()), row(2, json!({ "type": "limit", "stopped_by": "max_tool_calls", "used": 9, "cap": 8 })), row(3, finished("failed", "max_rounds")), end_frame(3)], "max_turn_requests", "Stopped by a limit: max_tool_calls (9 of 8)."),
        (vec![row(1, started()), row(2, finished("failed", "aborted")), end_frame(2)], "end_turn", "The run did not complete (failed)"),
        (vec![row(1, started()), row(2, json!({ "type": "question", "text": "", "choices": [] })), row(3, finished("waiting_user", "waiting_user")), end_frame(3)], "end_turn", "Answer it in the Aqua app: an editor cannot answer it."),
        (vec![row(1, started()), row(2, finished("cancelled", "cancelled")), end_frame(2)], "cancelled", ""),
    ] {
        let stub = stub_with_stream(frames.clone());
        let mut editor = Editor::start(&stub.url(), &data, &[]);
        let session = editor.open(CWD);
        let id = editor.request("session/prompt", prompt_params(&session, "do it"));
        let answer = editor.answer(id);
        assert_eq!(answer["result"]["stopReason"], reason, "{frames:?}\n{answer}");
        let said_all: String = updates(&editor.frames()).iter().filter_map(|u| u["content"]["text"].as_str()).collect::<Vec<_>>().join("");
        assert!(said_all.contains(said), "{said:?} in {said_all:?}");
        editor.close();
    }
}

#[test]
fn a_broken_stream_is_taken_up_again_and_no_update_is_sent_twice() {
    let data = data_dir("acp_reconnect");
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if let Some(query) = req.path.strip_prefix("/api/v1/runs/run_stub_1/stream") {
            let first = query == "?after=0";
            return Reply::Stream(Box::new(move |c| {
                if first {
                    chunk(c, &row(1, started()));
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                } else {
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                    chunk(c, &row(3, json!({ "type": "text", "index": 2, "text": "two" })));
                    chunk(c, &row(4, finished("completed", "final")));
                    chunk(c, &end_frame(4));
                }
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let session = editor.open(CWD);
    let id = editor.request("session/prompt", prompt_params(&session, "do it"));
    assert_eq!(editor.answer(id)["result"]["stopReason"], "end_turn");
    let said: Vec<String> = updates(&editor.frames()).iter().filter_map(|u| u["content"]["text"].as_str().map(str::to_string)).collect();
    assert_eq!(said, vec!["one\n".to_string(), "two\n".to_string()]);
    editor.close();
}

// ---- errors --------------------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_message_that_is_wrong_gets_a_json_rpc_error_with_a_stable_code_and_the_agent_goes_on() {
    let data = data_dir("acp_errors");
    let stub = Stub::start(|_, _| Reply::Json(404, json!({ "error": "no", "code": "not_found" })));
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    // Before initialize.
    let early = editor.request("session/new", json!({ "cwd": CWD }));
    let e = editor.answer(early)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32600), Some("invalid_request")), "{e}");
    // Not JSON, a batch, a bare value, a wrong version of JSON-RPC, a bad id.
    for (line, code, stable) in [
        ("{not json", -32700, "invalid_request"),
        ("[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}]", -32600, "invalid_request"),
        ("42", -32600, "invalid_request"),
        ("{\"jsonrpc\":\"1.0\",\"id\":9000,\"method\":\"initialize\"}", -32600, "invalid_request"),
        ("{\"jsonrpc\":\"2.0\",\"id\":{\"a\":1},\"method\":\"initialize\"}", -32600, "invalid_request"),
        ("{\"jsonrpc\":\"2.0\",\"id\":9001}", -32600, "invalid_request"),
    ] {
        let before = editor.frames().len();
        editor.send_raw(format!("{line}\n").into_bytes());
        let frames = editor.wait(line, |f| f.len() > before);
        let e = &frames[before]["error"];
        assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(code), Some(stable)), "{line}: {e}");
    }
    let init = editor.request("initialize", json!({ "protocolVersion": 1 }));
    assert!(editor.answer(init).get("result").is_some(), "it went on after every one of them");
    // A method it does not have, wrong params, a session it does not know.
    let unknown = editor.request("fs/read_text_file", json!({}));
    let e = editor.answer(unknown)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32601), Some("not_found")), "{e}");
    for (method, params) in [("session/new", json!({})), ("session/new", json!({ "cwd": "relative/path" })), ("session/new", json!({ "cwd": "x\u{7}y" })), ("session/prompt", json!({ "sessionId": "x" })), ("session/load", json!({ "sessionId": "../x", "cwd": CWD })), ("initialize", json!({ "protocolVersion": 0 }))] {
        let id = editor.request(method, params.clone());
        let e = editor.answer(id)["error"].clone();
        assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32602), Some("invalid_request")), "{method} {params}: {e}");
    }
    let id = editor.request("session/prompt", prompt_params("acp_unknown", "hello"));
    let e = editor.answer(id)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32002), Some("not_found")), "{e}");
    // A notification that is not known is ignored, and a response from the editor is not an error.
    editor.notify("$/cancel_request", json!({ "id": 1 }));
    editor.send(json!({ "jsonrpc": "2.0", "id": 77, "result": {} }));
    let before = editor.frames().len();
    let id = editor.request("session/list", json!({}));
    let frames = editor.wait("the list", |f| f.iter().any(|m| m["id"] == id));
    assert!(frames.len() == before + 1, "nothing was answered to the notification or to the response: {frames:#?}");
    editor.close();
}

#[test]
fn a_message_over_the_limit_is_skipped_to_its_end_and_refused_with_limit_reached() {
    let data = data_dir("acp_big");
    let stub = Stub::start(|_, _| Reply::Json(404, json!({})));
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    // A line of a bit more than a mebibyte, in pieces (so that the reader has to skip across reads), then a good message on the next line.
    let big = acp::MAX_MESSAGE_BYTES + 1000;
    editor.send_raw(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\",\"params\":\"".to_vec());
    for _ in 0..(big / 64_000 + 1) {
        editor.send_raw(vec![b'a'; 64_000]);
    }
    editor.send_raw(b"\"}\n".to_vec());
    let frames = editor.wait("the refusal", |f| !f.is_empty());
    let e = &frames[0]["error"];
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32600), Some("limit_reached")), "{e}");
    assert!(frames[0]["id"].is_null());
    assert!(e["message"].as_str().unwrap().len() < 300, "the refusal does not echo the message");
    let init = editor.request("initialize", json!({ "protocolVersion": 1 }));
    assert!(editor.answer(init).get("result").is_some(), "the next message was read");
    assert_eq!(editor.frames().len(), 2);
    editor.close();
}

#[test]
fn the_errors_of_the_core_keep_their_stable_code_and_a_core_that_is_not_there_can_be_tried_again() {
    let data = data_dir("acp_core_errors");
    for (status, body, number, stable) in [
        (403, json!({ "error": "that folder is not one Aqua may use", "code": "forbidden_scope" }), -32000, "forbidden_scope"),
        (401, json!({ "error": "no token", "code": "unauthorized" }), -32000, "unauthorized"),
        (429, json!({ "error": "eight runs are going", "code": "limit_reached" }), -32603, "limit_reached"),
        (503, json!({ "error": "not ready", "code": "not_ready" }), -32603, "not_ready"),
        (400, json!({ "error": "a goal is at most 4000 characters" }), -32602, "invalid_request"),
        (500, json!({ "error": "boom", "code": "a_word_that_is_not_one" }), -32603, "internal"),
    ] {
        let stub = Stub::start(move |_, _| Reply::Json(status, body.clone()));
        let mut editor = Editor::start(&stub.url(), &data, &[]);
        let session = editor.open(CWD);
        let id = editor.request("session/prompt", prompt_params(&session, "do it"));
        let e = editor.answer(id)["error"].clone();
        assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str(), e["data"]["status"].as_u64()), (Some(number), Some(stable), Some(status as u64)), "{status}: {e}");
        editor.close();
    }
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut editor = Editor::start(&format!("http://127.0.0.1:{free}"), &data, &[]);
    let session = editor.open(CWD);
    let id = editor.request("session/prompt", prompt_params(&session, "do it"));
    let e = editor.answer(id)["error"].clone();
    assert_eq!((e["data"]["code"].as_str(), e["data"]["retryable"].as_bool()), (Some("not_ready"), Some(true)), "{e}");
    assert!(e["message"].as_str().unwrap().contains("is Aqua running"), "{e}");
    editor.close();
    // A token that cannot be read: the agent starts, and the prompt says why it cannot go.
    let nowhere = data.join("no-such-folder").join("token.txt");
    let stub = Stub::start(|_, _| Reply::Json(200, json!({})));
    let mut editor = Editor::start(&stub.url(), &data, &["--token-file", &nowhere.to_string_lossy()]);
    let session = editor.open(CWD);
    let id = editor.request("session/prompt", prompt_params(&session, "do it"));
    let e = editor.answer(id)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32000), Some("unauthorized")), "{e}");
    assert!(stub.requests().is_empty());
    editor.close();
}

#[test]
fn a_bad_command_line_ends_the_agent_with_one_and_says_why_on_the_error_stream_only() {
    let data = data_dir("acp_args");
    for extra in [vec!["--wait-approval", "soon"], vec!["--profile", "nope"], vec!["--nope"], vec!["stray"]] {
        let editor = Editor::start("http://127.0.0.1:1", &data, &extra);
        let err = editor.err.clone();
        let out = editor.out.clone();
        assert_eq!(editor.close(), 1, "{extra:?}");
        assert!(!err.0.lock().unwrap().is_empty() && out.0.lock().unwrap().is_empty(), "{extra:?}: the reason is on the error stream and the protocol stream is empty");
    }
    let editor = Editor::start("http://example.com:8767", &data, &[]);
    let err = editor.err.clone();
    assert_eq!(editor.close(), 1);
    assert!(String::from_utf8_lossy(&err.0.lock().unwrap()).contains("on this computer"));
}

// ---- sessions ------------------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn sessions_are_listed_loaded_and_resumed_through_the_sessions_api() {
    let data = data_dir("acp_sessions");
    let stub = Stub::start(|req, _| match (req.method.as_str(), req.path.as_str()) {
        ("GET", p) if p.starts_with("/api/v1/sessions?") => Reply::Json(
            200,
            json!({ "sessions": [
                { "id": "acp_1_a", "title": "Fix the parser", "directory": "E:\\work\\project", "runs": 2, "started_at": 1_700_000_000_000_i64, "updated_at": 1_700_000_100_000_i64, "going": false },
                { "id": "bad id", "title": "x", "directory": "", "runs": 1, "started_at": 1, "updated_at": 2, "going": false }
            ], "next": "cursor-2" }),
        ),
        ("GET", "/api/v1/sessions/acp_1_a") => Reply::Json(
            200,
            json!({ "id": "acp_1_a", "title": "Fix the parser", "directory": "E:\\work\\project", "runs": [
                { "id": "run_1", "goal": "fix the parser", "final_answer": "I fixed it.", "status": "completed" },
                { "id": "run_2", "goal": "add a test", "final_answer": "", "status": "failed" }
            ] }),
        ),
        ("GET", "/api/v1/sessions/acp_gone") => Reply::Json(404, json!({ "error": "no session called acp_gone", "code": "not_found" })),
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let init = editor.request("initialize", json!({ "protocolVersion": 1 }));
    editor.answer(init);
    let list = editor.request("session/list", json!({ "cwd": CWD, "cursor": "abc-1=" }));
    let result = editor.answer(list)["result"].clone();
    assert_eq!(result["sessions"].as_array().unwrap().len(), 1, "a name the core would not take in a path is not offered: {result}");
    assert_eq!((result["sessions"][0]["sessionId"].as_str(), result["sessions"][0]["cwd"].as_str(), result["sessions"][0]["title"].as_str(), result["sessions"][0]["updatedAt"].as_str()), (Some("acp_1_a"), Some("E:\\work\\project"), Some("Fix the parser"), Some("2023-11-14T22:15:00.000Z")));
    assert_eq!(result["nextCursor"], "cursor-2");
    assert!(stub.paths()[0].starts_with("GET /api/v1/sessions?limit=50&directory=E%3A%5Cwork%5Cproject&cursor=abc-1%3D"), "{:?}", stub.paths());
    let bad = editor.request("session/list", json!({ "cursor": "a b\r\nGET /" }));
    assert_eq!(editor.answer(bad)["error"]["code"], -32602);
    // `session/load` brings the runs back as messages, then answers; `session/resume` does not replay.
    let load = editor.request("session/load", json!({ "sessionId": "acp_1_a", "cwd": CWD, "mcpServers": [] }));
    let answer = editor.answer(load);
    assert!(answer["result"].is_null() && answer.get("error").is_none(), "{answer}");
    let all = editor.frames();
    let replay: Vec<(String, String)> = updates(&all).iter().map(|u| (u["sessionUpdate"].as_str().unwrap().to_string(), u["content"]["text"].as_str().unwrap().to_string())).collect();
    assert_eq!(replay, vec![("user_message_chunk".to_string(), "fix the parser".to_string()), ("agent_message_chunk".to_string(), "I fixed it.".to_string()), ("user_message_chunk".to_string(), "add a test".to_string())]);
    let order: Vec<usize> = all.iter().enumerate().filter(|(_, m)| m["method"] == "session/update" || m["id"] == load).map(|(i, _)| i).collect();
    assert_eq!(*order.last().unwrap(), all.iter().position(|m| m["id"] == load).unwrap(), "the replay comes before the answer");
    let resume = editor.request("session/resume", json!({ "sessionId": "acp_1_a", "cwd": CWD }));
    editor.answer(resume);
    assert_eq!(updates(&editor.frames()).len(), 3, "no replay for a resume");
    let gone = editor.request("session/load", json!({ "sessionId": "acp_gone", "cwd": CWD }));
    let e = editor.answer(gone)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32002), Some("not_found")), "{e}");
    // A loaded session can be prompted: it is the core's session that the run is put in.
    editor.close();
}

#[test]
fn a_session_that_is_loaded_takes_a_prompt_in_its_own_name_and_two_prompts_at_once_are_refused() {
    let data = data_dir("acp_busy");
    let release = Arc::new(AtomicBool::new(false));
    let hold = release.clone();
    let stub = Stub::start(move |req, _| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/v1/runs") => start_answer(),
        ("GET", "/api/v1/sessions/acp_1_a") => Reply::Json(200, json!({ "id": "acp_1_a", "runs": [] })),
        (_, p) if p.starts_with("/api/v1/runs/run_stub_1/stream") => {
            let hold = hold.clone();
            Reply::Stream(Box::new(move |c| {
                chunk(c, &row(1, started()));
                let until = std::time::Instant::now() + Duration::from_secs(20);
                while !hold.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                chunk(c, &row(2, finished("completed", "final")));
                chunk(c, &end_frame(2));
            }))
        }
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let init = editor.request("initialize", json!({ "protocolVersion": 1 }));
    editor.answer(init);
    let load = editor.request("session/load", json!({ "sessionId": "acp_1_a", "cwd": CWD }));
    editor.answer(load);
    let first = editor.request("session/prompt", prompt_params("acp_1_a", "first"));
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !stub.paths().iter().any(|p| p.contains("/stream")) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let second = editor.request("session/prompt", prompt_params("acp_1_a", "second"));
    let e = editor.answer(second)["error"].clone();
    assert_eq!((e["code"].as_i64(), e["data"]["code"].as_str()), (Some(-32600), Some("conflict")), "{e}");
    release.store(true, Ordering::SeqCst);
    assert_eq!(editor.answer(first)["result"]["stopReason"], "end_turn");
    let starts: Vec<Value> = stub.requests().iter().filter(|r| r.method == "POST" && r.path == "/api/v1/runs").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(starts.len(), 1, "the refused prompt started nothing");
    assert_eq!(starts[0]["session_id"], "acp_1_a");
    editor.close();
}

#[test]
fn at_most_sixty_four_sessions_are_kept_and_the_oldest_idle_one_is_let_go() {
    let data = data_dir("acp_many");
    let stub = Stub::start(|_, _| Reply::Json(404, json!({})));
    let mut editor = Editor::start(&stub.url(), &data, &[]);
    let first = editor.open(CWD);
    let mut last = first.clone();
    for _ in 0..70 {
        let id = editor.request("session/new", json!({ "cwd": CWD }));
        last = editor.answer(id)["result"]["sessionId"].as_str().unwrap().to_string();
    }
    // The oldest was let go (the core still has its record: it can be loaded again); the newest is there.
    let gone = editor.request("session/prompt", prompt_params(&first, "hello"));
    assert_eq!(editor.answer(gone)["error"]["data"]["code"], "not_found");
    let there = editor.request("session/prompt", prompt_params(&last, "hello"));
    let e = editor.answer(there)["error"].clone();
    // The newest is kept: the agent took the prompt to the core (which this stub answers 404 to), so the message is the core's and not the agent's "no such session".
    assert_eq!(e["data"]["code"], "not_found");
    assert!(!e["message"].as_str().unwrap().contains("no such session"), "{e}");
    editor.close();
}

// ---- what goes into a goal -------------------------------------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_goal_is_the_request_first_then_the_context_cut_to_what_a_run_takes() {
    let (goal, cut) = goal_from_prompt(&[json!({ "type": "text", "text": "explain this" }), json!({ "type": "resource", "resource": { "uri": "file:///a.rs", "text": "fn a() {}" } }), json!({ "type": "resource_link", "uri": "file:///b.rs", "name": "b.rs" })]).unwrap();
    assert_eq!((goal.as_str(), cut), ("explain this\n\n[context from file:///a.rs]\nfn a() {}\n\n[file: file:///b.rs (b.rs)]", false));
    // Context before the request still comes after it; a very large context cannot push the request out.
    let (goal, cut) = goal_from_prompt(&[json!({ "type": "resource", "resource": { "uri": "file:///big.txt", "text": "x".repeat(50_000) } }), json!({ "type": "text", "text": "what is in it?" })]).unwrap();
    assert!(goal.starts_with("what is in it?\n\n[context from file:///big.txt]\n") && goal.chars().count() <= 4_000 && cut, "{}", goal.chars().count());
    // A request that is itself too long is cut, and says so.
    let (goal, cut) = goal_from_prompt(&[json!({ "type": "text", "text": "y".repeat(9_000) })]).unwrap();
    assert_eq!((goal.chars().count(), cut), (4_000, true));
    // A blob is not text: its place is named and nothing of it is taken. Control characters are replaced.
    let (goal, _) = goal_from_prompt(&[json!({ "type": "text", "text": "go\x1b[2J" }), json!({ "type": "resource", "resource": { "uri": "file:///a.bin", "blob": "AAAA" } })]).unwrap();
    assert_eq!(goal, "go?[2J\n\n[context from file:///a.bin: not text, left out]");
    // No text, an image, a sound, a block it does not know: refused with invalid params.
    for blocks in [vec![], vec![json!({ "type": "text", "text": "   " })], vec![json!({ "type": "text", "text": "x" }), json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" })], vec![json!({ "type": "audio" })], vec![json!({ "type": "video" })]] {
        let e = goal_from_prompt(&blocks).unwrap_err();
        assert_eq!(e.code, -32602, "{blocks:?}");
    }
}

#[test]
fn the_translator_names_the_kind_of_a_tool_and_cleans_what_it_says() {
    for (tool, kind) in [("fs.read", "read"), ("fs.grep", "search"), ("fs.edit", "edit"), ("fs.patch", "edit"), ("fs.delete", "delete"), ("fs.move", "move"), ("term.run", "execute"), ("check.run", "execute"), ("git.commit", "execute"), ("web.fetch", "fetch"), ("browser.click", "other"), ("mystery.tool", "other")] {
        assert_eq!(tool_kind(tool), kind, "{tool}");
    }
    let mut t = Translator::new("run_9");
    let start = t.updates(&json!({ "type": "tool.started", "call_id": 4, "tool": "fs.edit\x1b[2J\u{202e}" }));
    assert_eq!(start[0]["title"], "fs.edit?[2J<U+202E>");
    let end = t.updates(&json!({ "type": "tool.finished", "call_id": 4, "ok": false, "status": "denied\x07", "ms": 1, "decision": "denied", "approval": "denied" }));
    assert_eq!((end[0]["status"].as_str(), end[0]["content"][0]["content"]["text"].as_str()), (Some("failed"), Some("denied? (denied).")));
    // A waiting approval with no call open is a message; one with a call open is that call.
    assert_eq!(t.updates(&json!({ "type": "approval.opened", "request": {} }))[0]["sessionUpdate"], "agent_message_chunk");
    // The text of an event is bounded.
    let big = t.updates(&json!({ "type": "text", "text": "z".repeat(200_000) }));
    assert!(big[0]["content"]["text"].as_str().unwrap().len() <= 64 * 1024);
    // A usage event without a window says nothing.
    assert!(t.updates(&json!({ "type": "usage", "tokens_in": 5 })).is_empty());
    assert_eq!(clean("a\r\nb\rc\x1b"), "a\nb?c?");
    assert_eq!(bounded("h\u{e9}llo w\u{f6}rld, a longer text than the limit", 30).chars().count() <= 30, true);
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso8601(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
    assert_eq!(iso8601(1_791_570_522_993), "2026-10-09T18:28:42.993Z");
    assert_eq!(iso8601(-1), "1969-12-31T23:59:59.999Z");
}

#[test]
fn how_a_run_ended_is_the_stop_reason_and_a_cancel_of_the_editor_wins() {
    let mut o = crate::headless::run::Outcome::default();
    o.observe(&json!({ "type": "run.finished", "status": "completed", "stopped_reason": "final" }));
    assert_eq!(stop_reason(&o, false), "end_turn");
    assert_eq!(stop_reason(&o, true), "cancelled");
    let mut limit = crate::headless::run::Outcome::default();
    limit.observe(&json!({ "type": "run.finished", "status": "failed", "stopped_reason": "max_rounds" }));
    assert_eq!(stop_reason(&limit, false), "max_turn_requests");
    let mut cancelled = crate::headless::run::Outcome::default();
    cancelled.observe(&json!({ "type": "run.finished", "status": "cancelled", "stopped_reason": "cancelled" }));
    assert_eq!(stop_reason(&cancelled, false), "cancelled");
}

#[test]
fn an_error_carries_the_protocols_number_and_one_of_the_stable_codes() {
    let e = RpcError::from_core(409, "a second answer", "");
    assert_eq!((e.code, e.data["code"].as_str(), e.data["status"].as_u64()), (-32603, Some("conflict"), Some(409)), "the code of the status when the body names none");
    let e = RpcError::from_core(503, "busy", "busy");
    assert_eq!(e.data["retryable"], true);
    let e = RpcError::invalid_params(&"x".repeat(2_000));
    assert!(e.message.len() <= 600 + 20);
    for stable in ["invalid_request", "not_found", "unauthorized", "forbidden_scope", "limit_reached", "not_ready", "conflict", "internal"] {
        assert!(std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("documentation").join("ERROR_CODES.md")).unwrap().contains(&format!("`{stable}`")), "{stable} is a code of the contract");
    }
}

#[test]
fn the_agent_reads_and_writes_no_file_and_asks_the_editor_for_nothing() {
    let source = include_str!("../acp.rs");
    // What this process does not do is read in its source: no file API, no process, no request to the editor.
    for needle in ["std::fs", "File::", "read_to_string", "Command::new", "\"fs/read_text_file\"", "\"fs/write_text_file\"", "\"terminal/create\"", "\"session/request_permission\"", "browser/confirm", "/answer", "/plan/approve", "UI_TOKEN", "TokenKind::Ui"] {
        let code: String = source.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        assert!(!code.contains(needle), "acp.rs uses {needle}");
    }
}
