//! The client against a stub of the core: a thread with a `TcpListener` that answers the requests of the test as the core does (JSON answers, a chunked server-sent stream), and writes
//! down what it was asked. No model, no real core.

use super::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "stub-api-token-0123456789abcdef";
const UI_TOKEN: &str = "stub-ui-token-fedcba9876543210";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    auth: String,
    body: String,
}

type Out = Box<dyn FnOnce(&mut TcpStream) + Send>;

enum Reply {
    Json(u16, Value),
    /// The head of a chunked stream, then whatever the closure writes (chunks) and returns; the connection closes after it.
    Stream(Out),
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("scratch_headless").join(format!("{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn data_dir(name: &str) -> PathBuf {
    let dir = scratch(name);
    std::fs::write(dir.join(".aqua_api_token"), format!("{TOKEN}\n")).unwrap();
    std::fs::write(dir.join(".aqua_ui_token"), format!("{UI_TOKEN}\n")).unwrap();
    dir
}

struct Stub {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Stub {
    fn start(script: impl Fn(&Seen, usize) -> Reply + Send + Sync + 'static) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, stop) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
        let script = Arc::new(script);
        let (seen2, stop2) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let (seen, script) = (seen2.clone(), script.clone());
                        std::thread::spawn(move || handle(conn, seen, script));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Stub { port, seen, stop }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| format!("{} {}", r.method, r.path)).collect()
    }
}

fn handle(conn: TcpStream, seen: Arc<Mutex<Vec<Seen>>>, script: Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let (mut auth, mut length) = (String::new(), 0usize);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("authorization:") {
            auth = v.trim().to_string();
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let request = Seen { method, path, auth, body: String::from_utf8_lossy(&body).to_string() };
    let n = {
        let mut all = seen.lock().unwrap();
        all.push(request.clone());
        all.iter().filter(|r| r.path.split('?').next() == request.path.split('?').next()).count()
    };
    let mut conn = conn;
    match script(&request, n) {
        Reply::Json(status, v) => {
            let text = v.to_string();
            let _ = write!(conn, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
        Reply::Stream(f) => {
            let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            f(&mut conn);
            let _ = write!(conn, "0\r\n\r\n");
        }
    }
    let _ = conn.flush();
}

fn chunk(conn: &mut TcpStream, text: &str) {
    let _ = write!(conn, "{:x}\r\n{text}\r\n", text.len());
    let _ = conn.flush();
}

/// One row of a run's log as the core sends it as a frame: `id:` is its number in the run.
fn row(seq: i64, mut event: Value) -> String {
    let kind = event["type"].as_str().unwrap().to_string();
    event["v"] = json!(1);
    event["id"] = json!(1000 + seq);
    event["seq"] = json!(seq);
    event["run_id"] = json!("run_stub_1");
    event["ts"] = json!(1_791_570_522_993_i64);
    format!("event: {kind}\nid: {seq}\ndata: {event}\n\n")
}

fn end_frame(last: i64) -> String {
    format!("event: end\ndata: {}\n\n", json!({ "v": 1, "type": "end", "last": last }))
}

fn finished(status: &str, reason: &str) -> Value {
    json!({ "type": "run.finished", "status": status, "stopped_reason": reason, "answer_head": "", "files_changed": 0, "ms": 1200 })
}

fn started() -> Value {
    json!({ "type": "run.started", "goal_head": "g", "caller": "agent", "route": "agent", "started_ms": 1 })
}

fn start_answer() -> Reply {
    Reply::Json(202, json!({ "run_id": "run_stub_1", "state": "running", "position": 0, "duplicate": false, "stream": "/api/v1/runs/run_stub_1/stream" }))
}

/// A stub whose stream sends `frames` and ends.
fn stub_with_stream(frames: Vec<String>) -> Stub {
    Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.starts_with("/api/v1/runs/run_stub_1/stream") {
            let frames = frames.clone();
            return Reply::Stream(Box::new(move |c| {
                for f in &frames {
                    chunk(c, f);
                }
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    })
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

fn go(stub_url: &str, data: &Path, args: &[&str], tweak: impl FnOnce(&mut Io)) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let interrupts = AtomicUsize::new(0);
    let confirm = |_: &str| false;
    let code = {
        let mut io = Io { out: &mut out, err: &mut err, stdin_text: None, stdin_is_terminal: false, color: false, interrupts: &interrupts, backoff_unit: Duration::from_millis(5), openapi: include_str!("../../../documentation/openapi.json"), confirm: &confirm };
        tweak(&mut io);
        let mut full: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        full.extend(["--core".into(), stub_url.to_string(), "--data-dir".into(), data.to_string_lossy().to_string()]);
        run_cli(&full, data, &mut io)
    };
    Ran { code, out: String::from_utf8_lossy(&out).to_string(), err: String::from_utf8_lossy(&err).to_string() }
}

#[test]
fn the_help_lists_every_command() {
    let help = HELP.join("\n");
    for word in COMMANDS {
        assert!(help.contains(&format!("aqua-cli {word} ")) || help.contains(&format!("aqua-cli {word}\n")) || help.contains(&format!("aqua-cli {word} [")), "the help lists {word}");
    }
    assert_eq!(HELP.len(), COMMANDS.len());
}

#[test]
fn exit_codes_follow_the_final_event() {
    let data = data_dir("exit_codes");
    for (frames, want) in [
        (vec![row(1, started()), row(2, finished("completed", "final")), end_frame(2)], 0),
        (vec![row(1, started()), row(2, finished("failed", "aborted")), end_frame(2)], 1),
        (vec![row(1, started()), row(2, json!({ "type": "question", "text": "", "choices": [] })), row(3, finished("waiting_user", "waiting_user")), end_frame(3)], 2),
        (vec![row(1, started()), row(2, json!({ "type": "approval.closed", "request_id": "a", "outcome": "timeout" })), row(3, finished("completed", "final")), end_frame(3)], 2),
        (vec![row(1, started()), row(2, json!({ "type": "limit", "stopped_by": "max_tool_calls", "used": 9, "cap": 8 })), row(3, finished("failed", "final")), end_frame(3)], 3),
        (vec![row(1, started()), row(2, finished("failed", "max_rounds")), end_frame(2)], 3),
        (vec![row(1, started()), row(2, finished("cancelled", "cancelled")), end_frame(2)], 4),
    ] {
        let stub = stub_with_stream(frames.clone());
        let ran = go(&stub.url(), &data, &["run", "do it"], |_| {});
        assert_eq!(ran.code, want, "{frames:?}\nout: {}\nerr: {}", ran.out, ran.err);
    }
}

#[test]
fn a_core_that_is_not_there_or_is_too_busy_is_exit_five() {
    let data = data_dir("unreachable");
    // A port nothing listens on: bound, then let go.
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ran = go(&format!("http://127.0.0.1:{free}"), &data, &["run", "do it"], |_| {});
    assert_eq!(ran.code, 5, "{}", ran.err);
    assert!(ran.err.contains("is Aqua running"), "{}", ran.err);
    for (status, code) in [(429, "limit_reached"), (503, "busy"), (503, "not_ready")] {
        let stub = Stub::start(move |_, _| Reply::Json(status, json!({ "error": "too much", "code": code })));
        let ran = go(&stub.url(), &data, &["run", "do it"], |_| {});
        assert_eq!(ran.code, 5, "{status} {code}: {}", ran.err);
    }
    let stub = Stub::start(|_, _| Reply::Json(400, json!({ "error": "say what to do", "code": "invalid_request" })));
    assert_eq!(go(&stub.url(), &data, &["run", "do it"], |_| {}).code, 1, "a request the core refused is a failure, not a missing core");
    let stub = Stub::start(|_, _| Reply::Json(401, json!({ "error": "no token", "code": "unauthorized" })));
    let ran = go(&stub.url(), &data, &["run", "do it"], |_| {});
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("token was refused"), "{}", ran.err);
}

#[test]
fn json_format_is_the_stream() {
    let data = data_dir("json_format");
    let frames = vec![
        row(1, started()),
        "event: heartbeat\ndata: {\"v\":1,\"type\":\"heartbeat\"}\n\n".to_string(),
        row(2, json!({ "type": "tool.started", "call_id": 1, "tool": "fs.read", "args_head": "" })),
        row(3, json!({ "type": "text", "index": 1, "text": "done." })),
        row(4, finished("completed", "final")),
        end_frame(4),
    ];
    let stub = stub_with_stream(frames.clone());
    let ran = go(&stub.url(), &data, &["run", "do it", "--format", "json"], |io| io.color = true);
    assert_eq!(ran.code, 0, "{}", ran.err);
    let lines: Vec<&str> = ran.out.lines().collect();
    // One JSON event per line, identical to the data of the frames, in order; the heartbeat is left out; nothing else is on the output and no colour is in it.
    let sent: Vec<String> = frames.iter().filter(|f| !f.starts_with("event: heartbeat")).map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")).unwrap().to_string()).collect();
    assert_eq!(lines, sent.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!ran.out.contains('\x1b'), "no colour in json");
    for l in lines {
        serde_json::from_str::<Value>(l).expect("every line is one JSON object");
    }
}

#[test]
fn the_text_format_prints_steps_and_sentences_and_colour_only_when_asked() {
    let data = data_dir("text_format");
    let frames = vec![
        row(1, started()),
        row(2, json!({ "type": "step", "step": 1, "phase": "plan", "title": "Plan", "status": "running" })),
        row(3, json!({ "type": "tool.started", "call_id": 1, "tool": "fs.read", "args_head": "" })),
        row(4, json!({ "type": "text", "index": 1, "text": "I added it.\u{1b}[2J" })),
        row(5, finished("completed", "final")),
        end_frame(5),
    ];
    let stub = stub_with_stream(frames);
    let plain = go(&stub.url(), &data, &["run", "do it"], |_| {});
    assert_eq!(plain.code, 0, "{}", plain.err);
    assert!(plain.out.contains("> Plan") && plain.out.contains("$ fs.read") && plain.out.contains("I added it.?[2J") && plain.out.contains("= completed (final)"), "{}", plain.out);
    assert!(!plain.out.contains('\x1b'), "an escape sequence in the answer is never drawn");
    let coloured = go(&stub.url(), &data, &["run", "do it"], |io| io.color = true);
    assert!(coloured.out.contains("\x1b[2m$ fs.read"), "{}", coloured.out);
}

#[test]
fn reconnects_with_after() {
    let data = data_dir("reconnect");
    let seen_after = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = seen_after.clone();
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if let Some(query) = req.path.strip_prefix("/api/v1/runs/run_stub_1/stream") {
            seen.lock().unwrap().push(query.to_string());
            let first = query == "?after=0";
            return Reply::Stream(Box::new(move |c| {
                if first {
                    // Two events, then the connection goes: no end frame.
                    chunk(c, &row(1, started()));
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                } else {
                    // The core replays one the client has (a replay is never printed twice), then the rest.
                    chunk(c, &row(2, json!({ "type": "text", "index": 1, "text": "one" })));
                    chunk(c, &row(3, json!({ "type": "text", "index": 2, "text": "two" })));
                    chunk(c, &row(4, finished("completed", "final")));
                    chunk(c, &end_frame(4));
                }
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let ran = go(&stub.url(), &data, &["run", "do it"], |_| {});
    assert_eq!(ran.code, 0, "{}\n{}", ran.out, ran.err);
    assert_eq!(*seen_after.lock().unwrap(), vec!["?after=0".to_string(), "?after=2".to_string()], "the second connection asks for what came after the last event it has");
    assert_eq!(ran.out.matches("one").count(), 1, "{}", ran.out);
    assert_eq!(ran.out.matches("two").count(), 1, "{}", ran.out);
    assert!(ran.err.contains("taking it up again"), "{}", ran.err);
}

#[test]
fn a_stream_that_keeps_closing_without_an_end_is_given_up_with_exit_five() {
    let data = data_dir("keeps_closing");
    let stub = Stub::start(|req, _| {
        if req.method == "POST" {
            return start_answer();
        }
        Reply::Stream(Box::new(|_| {}))
    });
    let ran = go(&stub.url(), &data, &["run", "do it"], |_| {});
    assert_eq!(ran.code, 5, "{}", ran.err);
    // Said as a stream that keeps closing, or (when a connection was shut before the answer's first line came, which a busy machine does now and then) as a connection that broke: either
    // way it is bounded and it is exit 5.
    assert!(ran.err.contains("keeps closing") || ran.err.contains("closed the connection") || ran.err.contains("no answer") || ran.err.contains("nothing answered"), "{}", ran.err);
    assert!(ran.err.matches("trying again").count() >= 1 || ran.err.contains("keeps closing"), "it tried again before it gave up: {}", ran.err);
}

#[test]
fn ctrl_c_sends_interrupt() {
    let data = data_dir("ctrl_c");
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let stub = Stub::start(move |req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.ends_with("/interrupt") {
            flag.store(true, Ordering::SeqCst);
            return Reply::Json(200, json!({ "run_id": "run_stub_1", "interrupted": true, "ended": false, "state": "stopping" }));
        }
        if req.path.starts_with("/api/v1/runs/run_stub_1/stream") {
            let flag = flag.clone();
            return Reply::Stream(Box::new(move |c| {
                chunk(c, &row(1, started()));
                // The run goes on until it is told to stop, and then ends as a stopped run does.
                let until = std::time::Instant::now() + Duration::from_secs(20);
                while !flag.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                chunk(c, &row(2, finished("cancelled", "cancelled")));
                chunk(c, &end_frame(2));
            }));
        }
        Reply::Json(404, json!({ "error": "no", "code": "not_found" }))
    });
    let interrupts = Arc::new(AtomicUsize::new(0));
    // Ctrl+C is pressed once the first event is out.
    let pressed = interrupts.clone();
    let watcher = {
        let seen = stub.seen.clone();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while !seen.lock().unwrap().iter().any(|r| r.path.contains("/stream")) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(100));
            pressed.fetch_add(1, Ordering::SeqCst);
        })
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let confirm = |_: &str| false;
    let code = {
        let mut io = Io { out: &mut out, err: &mut err, stdin_text: None, stdin_is_terminal: false, color: false, interrupts: &interrupts, backoff_unit: Duration::from_millis(5), openapi: "{}", confirm: &confirm };
        run_cli(&["run".into(), "do it".into(), "--core".into(), stub.url(), "--data".into(), data.to_string_lossy().to_string()], &data, &mut io)
    };
    watcher.join().unwrap();
    assert!(interrupted.load(Ordering::SeqCst), "the first Ctrl+C asked the core to end the run: {:?}", stub.paths());
    assert!(stub.paths().contains(&"POST /api/v1/runs/run_stub_1/interrupt".to_string()));
    assert_eq!(code, 4, "a run that was stopped is cancelled");
    assert!(String::from_utf8_lossy(&err).contains("stopping..."));
}

#[test]
fn a_second_ctrl_c_leaves_at_once_without_waiting_for_the_run() {
    let data = data_dir("ctrl_c_twice");
    let stub = Stub::start(|req, _| {
        if req.method == "POST" && req.path == "/api/v1/runs" {
            return start_answer();
        }
        if req.path.ends_with("/interrupt") {
            return Reply::Json(200, json!({ "run_id": "run_stub_1", "interrupted": true, "ended": false }));
        }
        // A run that never ends.
        Reply::Stream(Box::new(|c| {
            chunk(c, &row(1, started()));
            std::thread::sleep(Duration::from_secs(20));
        }))
    });
    let interrupts = AtomicUsize::new(2);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let confirm = |_: &str| false;
    let began = std::time::Instant::now();
    let code = {
        let mut io = Io { out: &mut out, err: &mut err, stdin_text: None, stdin_is_terminal: false, color: false, interrupts: &interrupts, backoff_unit: Duration::from_millis(5), openapi: "{}", confirm: &confirm };
        run_cli(&["run".into(), "do it".into(), "--core".into(), stub.url(), "--data".into(), data.to_string_lossy().to_string()], &data, &mut io)
    };
    assert_eq!(code, 4);
    assert!(began.elapsed() < Duration::from_secs(5), "it did not wait for the run");
}

#[test]
fn the_client_never_answers_a_question_and_never_prints_a_token() {
    let data = data_dir("never_approves");
    let frames = vec![
        row(1, started()),
        row(2, json!({ "type": "status", "state": "waiting_approval" })),
        row(3, json!({ "type": "approval.opened", "request": {} })),
        row(4, json!({ "type": "approval.closed", "request_id": "apr_1", "outcome": "timeout" })),
        row(5, finished("completed", "final")),
        end_frame(5),
    ];
    let stub = stub_with_stream(frames);
    let ran = go(&stub.url(), &data, &["run", "do it", "--wait-approval", "30"], |_| {});
    assert_eq!(ran.code, 2, "a question nobody answered is the person's: {}", ran.err);
    assert!(ran.out.contains("approval needed in the Aqua app"), "{}", ran.out);
    assert!(ran.err.contains("the run needs you"), "{}", ran.err);
    for p in stub.paths() {
        assert!(!p.contains("confirm") && !p.contains("answer") && !p.contains("approv"), "the client asked for {p}");
    }
    for r in stub.requests() {
        assert_eq!(r.auth, format!("bearer {}", TOKEN.to_ascii_lowercase()), "only the API token is ever sent, and as a bearer token");
    }
    assert!(!ran.out.contains(TOKEN) && !ran.err.contains(TOKEN), "a token is never printed");
    let body: Value = serde_json::from_str(&stub.requests()[0].body).unwrap();
    assert_eq!(body["approval_wait_secs"], 30);
}

#[test]
fn the_goal_the_directory_and_the_piped_text_are_what_the_core_is_asked() {
    let data = data_dir("start_body");
    let stub = stub_with_stream(vec![row(1, started()), row(2, finished("completed", "final")), end_frame(2)]);
    let ran = go(&stub.url(), &data, &["run", "review", "this", "--max-steps", "12", "--profile", "coding", "--dir", "E:/p", "--idempotency-key", "k1", "--session", "night.1"], |io| io.stdin_text = Some("diff --git a b\n+x\n".into()));
    assert_eq!(ran.code, 0, "{}", ran.err);
    let body: Value = serde_json::from_str(&stub.requests()[0].body).unwrap();
    assert_eq!(body["goal"], "review this\n\ndiff --git a b\n+x");
    assert_eq!((body["max_steps"].as_u64(), body["profile"].as_str(), body["idempotency_key"].as_str(), body["session_id"].as_str()), (Some(12), Some("coding"), Some("k1"), Some("night.1")));
    assert_eq!(body["approval_wait_secs"], 10, "with no --wait-approval the question is left open for the shortest time the core allows");
    assert!(body["directory"].as_str().unwrap().replace('\\', "/").ends_with("E:/p"), "{}", body["directory"]);
}

#[test]
fn detach_starts_the_run_and_leaves() {
    let data = data_dir("detach");
    let stub = Stub::start(|_, _| start_answer());
    let ran = go(&stub.url(), &data, &["run", "do it", "--detach"], |_| {});
    assert_eq!(ran.code, 0);
    assert!(ran.out.contains("run_stub_1") && ran.out.contains("runs events run_stub_1 --follow"), "{}", ran.out);
    assert_eq!(stub.paths(), vec!["POST /api/v1/runs".to_string()], "no stream was opened");
    let json = go(&stub.url(), &data, &["run", "do it", "--detach", "--format", "json"], |_| {});
    assert_eq!(serde_json::from_str::<Value>(json.out.trim()).unwrap()["run_id"], "run_stub_1");
}

#[test]
fn a_bad_command_line_is_refused_before_the_core_is_asked() {
    let data = data_dir("bad_args");
    let stub = Stub::start(|_, _| Reply::Json(200, json!({})));
    for args in [vec!["run"], vec!["run", "x", "--format", "xml"], vec!["run", "x", "--nope"], vec!["run", "x", "--max-steps", "many"], vec!["run", "x", "--session", "a b"], vec!["run", "x", "--session", ""], vec!["run", "x", "--preapproval", "pa_1"], vec!["run", "x", "--preapproval", "../x", "--dir", "E:/p"], vec!["runs", "frobnicate"], vec!["runs", "show", "../etc"], vec!["steer", "run_1"], vec!["stop", "a/b"]] {
        let ran = go(&stub.url(), &data, &args, |_| {});
        assert_eq!(ran.code, 1, "{args:?}: {}", ran.err);
        assert!(!ran.err.is_empty(), "{args:?} says why");
    }
    assert!(stub.requests().is_empty(), "{:?}", stub.paths());
    // A core address that is not this computer is refused too, and the token is never sent there.
    let ran = go("http://example.com:8767", &data, &["run", "x"], |_| {});
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("on this computer"), "{}", ran.err);
}

#[test]
fn stop_with_no_id_ends_the_one_run_that_is_going_and_steer_says_it_to_that_run() {
    let data = data_dir("stop_steer");
    let stub = Stub::start(|req, _| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/v1/runs/active") => Reply::Json(200, json!({ "runs": [{ "run_id": "run_stub_1", "state": "running", "goal": "g" }] })),
        ("POST", "/api/v1/runs/run_stub_1/interrupt") => Reply::Json(200, json!({ "run_id": "run_stub_1", "interrupted": true, "ended": true, "state": "cancelled" })),
        ("POST", "/api/v1/runs/run_stub_1/messages") => Reply::Json(202, json!({ "run_id": "run_stub_1", "delivery": "steer", "queued": 1, "note": "it is read at the next step" })),
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let stopped = go(&stub.url(), &data, &["stop"], |_| {});
    assert_eq!((stopped.code, stopped.out.trim()), (0, "run_stub_1: stopped"), "{}", stopped.err);
    let steered = go(&stub.url(), &data, &["steer", "run_stub_1", "use", "strict", "mode", "--queue"], |_| {});
    assert_eq!(steered.code, 0, "{}", steered.err);
    let body: Value = serde_json::from_str(&stub.requests().last().unwrap().body).unwrap();
    assert_eq!((body["text"].as_str(), body["delivery"].as_str()), (Some("use strict mode"), Some("queue")));
    // Two runs going: it will not guess.
    let two = Stub::start(|_, _| Reply::Json(200, json!({ "runs": [{ "run_id": "a" }, { "run_id": "b" }] })));
    let ran = go(&two.url(), &data, &["stop"], |_| {});
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("2 runs are going"), "{}", ran.err);
    assert_eq!(two.paths(), vec!["GET /api/v1/runs/active".to_string()]);
}

#[test]
fn runs_events_without_follow_reads_the_pages_to_the_end_and_export_is_json_lines() {
    let data = data_dir("events_pages");
    let stub = Stub::start(|req, _| {
        let after: i64 = req.path.split("after=").nth(1).and_then(|r| r.split('&').next()).and_then(|n| n.parse().ok()).unwrap_or(0);
        let all = vec![json!({ "seq": 1, "type": "run.started", "run_id": "run_stub_1", "goal_head": "g" }), json!({ "seq": 2, "type": "text", "index": 1, "text": "hello" }), json!({ "seq": 3, "type": "run.finished", "status": "completed", "stopped_reason": "final", "files_changed": 0, "ms": 1 })];
        let page: Vec<Value> = all.into_iter().filter(|e| e["seq"].as_i64().unwrap() > after).take(2).collect();
        let last = page.last().map_or(after, |e| e["seq"].as_i64().unwrap());
        Reply::Json(200, json!({ "run_id": "run_stub_1", "events": page, "last": last, "finished": last >= 3 }))
    });
    let text = go(&stub.url(), &data, &["runs", "events", "run_stub_1"], |_| {});
    assert_eq!(text.code, 0, "{}", text.err);
    assert!(text.out.contains("hello") && text.out.contains("= completed"), "{}", text.out);
    let export = go(&stub.url(), &data, &["runs", "export", "run_stub_1"], |_| {});
    let lines: Vec<Value> = export.out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.iter().map(|l| l["seq"].as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn api_calls_an_operation_by_its_name_and_never_one_that_needs_the_apps_token() {
    let data = data_dir("api_cmd");
    let stub = Stub::start(|req, _| match req.path.as_str() {
        "/api/v1/runs/active" => Reply::Json(200, json!({ "runs": [] })),
        p if p.starts_with("/api/v1/runs/run_9/events") => Reply::Json(200, json!({ "events": [], "last": 0, "finished": false })),
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    let ok = go(&stub.url(), &data, &["api", "getRunsActive"], |_| {});
    assert_eq!((ok.code, ok.out.trim()), (0, "{\"runs\":[]}"), "{}", ok.err);
    let with_params = go(&stub.url(), &data, &["api", "getRunsRunIdEvents", "--param", "run_id=run_9", "--param", "after=3"], |_| {});
    assert_eq!(with_params.code, 0, "{}", with_params.err);
    assert!(stub.paths().contains(&"GET /api/v1/runs/run_9/events?after=3".to_string()), "{:?}", stub.paths());
    let before = stub.requests().len();
    // `--data` is the body here (the folder is --data-dir); a UI operation and a stream are refused with no request.
    let ui = go(&stub.url(), &data, &["api", "getVault"], |_| {});
    assert_eq!(ui.code, 1);
    assert!(ui.err.contains("UI token"), "{}", ui.err);
    let streaming = go(&stub.url(), &data, &["api", "getRunsRunIdStream", "--param", "run_id=run_9"], |_| {});
    assert_eq!(streaming.code, 1);
    assert!(streaming.err.contains("stream"), "{}", streaming.err);
    let unknown = go(&stub.url(), &data, &["api", "getNothing"], |_| {});
    assert_eq!(unknown.code, 1);
    assert_eq!(stub.requests().len(), before, "none of the three made a request");
    let list = go(&stub.url(), &data, &["api", "--list"], |_| {});
    assert!(list.out.contains("getRunsActive") && list.out.contains("postRuns"), "{}", list.out);
}

#[test]
fn a_pre_approval_is_made_only_by_a_person_at_a_terminal_who_says_yes_and_with_the_apps_token() {
    let data = data_dir("preapprove");
    let stub = Stub::start(|req, _| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/v1/preapprovals") => Reply::Json(201, json!({ "id": "pa_0123456789abcdef", "folder": "E:/p", "kinds": ["tests"], "budget": 60, "minutes": 30, "trusted": true })),
        ("DELETE", "/api/v1/preapprovals/pa_0123456789abcdef") => Reply::Json(200, json!({ "revoked": true })),
        _ => Reply::Json(404, json!({ "error": "no", "code": "not_found" })),
    });
    // A script (standard input is not a terminal) is refused before anything is read or sent.
    let script = go(&stub.url(), &data, &["preapprove", "E:/p"], |_| {});
    assert_eq!(script.code, 1);
    assert!(script.err.contains("needs a person at a terminal"), "{}", script.err);
    assert!(stub.requests().is_empty());
    // A person who does not say yes: nothing is made.
    let no = go(&stub.url(), &data, &["preapprove", "E:/p", "--kinds", "tests"], |io| io.stdin_is_terminal = true);
    assert_eq!(no.code, 1);
    assert!(stub.requests().is_empty(), "no yes, no request");
    // A person who says yes: one request with the app's token, and the id is printed with how to use it.
    let said_yes = |_: &str| true;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let interrupts = AtomicUsize::new(0);
    let code = {
        let mut io = Io { out: &mut out, err: &mut err, stdin_text: None, stdin_is_terminal: true, color: false, interrupts: &interrupts, backoff_unit: Duration::from_millis(5), openapi: "{}", confirm: &said_yes };
        run_cli(&["preapprove".into(), "E:/p".into(), "--kinds".into(), "tests,build".into(), "--budget".into(), "40".into(), "--minutes".into(), "90".into(), "--core".into(), stub.url(), "--data".into(), data.to_string_lossy().to_string()], &data, &mut io)
    };
    let (out, err) = (String::from_utf8_lossy(&out).to_string(), String::from_utf8_lossy(&err).to_string());
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("pa_0123456789abcdef") && out.contains("--preapproval pa_0123456789abcdef"), "{out}");
    let made = &stub.requests()[0];
    assert_eq!(made.auth, format!("bearer {}", UI_TOKEN.to_ascii_lowercase()), "the app's token, and only here");
    let body: Value = serde_json::from_str(&made.body).unwrap();
    assert_eq!((body["kinds"].clone(), body["budget"].as_u64(), body["minutes"].as_u64()), (json!(["tests", "build"]), Some(40), Some(90)));
    assert!(!out.contains(UI_TOKEN) && !err.contains(UI_TOKEN));
    // A kind that is not one is refused before anything is asked.
    let bad = go(&stub.url(), &data, &["preapprove", "E:/p", "--kinds", "deploy"], |io| io.stdin_is_terminal = true);
    assert_eq!(bad.code, 1);
    assert_eq!(stub.requests().len(), 1);
    // And one can be taken back.
    let revoke = go(&stub.url(), &data, &["preapprovals", "revoke", "pa_0123456789abcdef"], |io| io.stdin_is_terminal = true);
    assert_eq!(revoke.code, 0, "{}", revoke.err);
}

#[test]
fn the_api_token_comes_from_a_file_or_the_environment_and_the_app_token_from_the_data_folder_only() {
    let data = data_dir("tokens");
    assert_eq!(http::read_token(http::TokenKind::Api, None, &data).unwrap().len(), TOKEN.len());
    assert_eq!(http::read_token(http::TokenKind::Ui, None, &data).unwrap(), UI_TOKEN);
    let elsewhere = data.join("mine.txt");
    std::fs::write(&elsewhere, "other-token\n").unwrap();
    assert_eq!(http::read_token(http::TokenKind::Api, Some(&elsewhere), &data).unwrap(), "other-token");
    assert!(http::read_token(http::TokenKind::Api, None, &data.join("nowhere")).is_err());
    // A token file that holds more than a token is not one.
    std::fs::write(&elsewhere, "a b\nc").unwrap();
    assert!(http::read_token(http::TokenKind::Api, Some(&elsewhere), &data).is_err());
    // The error says where it looked, never what was in the file.
    let e = http::read_token(http::TokenKind::Api, Some(&elsewhere), &data).unwrap_err();
    assert!(!e.contains("a b"), "{e}");
}

// The terminal view and the editor protocol (card OA10-07) are tested against the same stub of the core: they are children of this module to use its helpers.
mod acp_tests;
mod view_tests;
