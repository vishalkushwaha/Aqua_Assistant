use super::model::*;
use super::n8n::*;
use super::run::exec::*;
use super::run::manager::FlowRunner;
use super::run::model::*;
use super::run::store::RunStore;
use super::store::FlowStore;
use super::supervisor::{classify, ErrorClass};
use aqua_native_tools::secrets::{MemorySecrets, Secrets};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- a stand-in for n8n -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }
}

enum Reply {
    Status(u16, &'static str, String),
    /// Read the request, then never answer.
    Hang,
    /// Answer with a redirect.
    Redirect,
}

type Handler = Arc<dyn Fn(&Req) -> Reply + Send + Sync>;

fn fake_n8n(handler: Handler) -> (String, Arc<Mutex<Vec<Req>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let handler = handler.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (mut head_end, mut want) = (None, 0usize);
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if head_end.is_none() {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = Some(p + 4);
                            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                            want = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                        }
                    }
                    if let Some(h) = head_end {
                        if buf.len() >= h + want {
                            break;
                        }
                    }
                }
                let h = head_end.unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..h]).to_string();
                let mut lines = head.lines();
                let first = lines.next().unwrap_or("").to_string();
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                let req = Req { method, path, headers, body: String::from_utf8_lossy(&buf[h..]).to_string() };
                log.lock().unwrap().push(req.clone());
                match handler(&req) {
                    Reply::Status(code, ctype, body) => {
                        let resp = format!("HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        let _ = s.write_all(resp.as_bytes());
                    }
                    Reply::Redirect => {
                        let _ = s.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Reply::Hang => std::thread::sleep(Duration::from_secs(8)),
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn ok_json(body: Value) -> Handler {
    Arc::new(move |_| Reply::Status(200, "application/json", body.to_string()))
}

fn secrets_with(name: &str, value: &str) -> Arc<MemorySecrets> {
    let s = Arc::new(MemorySecrets::new());
    s.set(name, value).unwrap();
    s
}

fn node(config: Value, credential: Option<&str>) -> Node {
    let mut n: Node = serde_json::from_value(json!({ "id": "sheet_step", "kind": "n8n", "name": "Send to n8n", "config": config })).unwrap();
    n.credential = credential.map(String::from);
    n
}

fn call(ex: &Arc<N8nNodeExecutor>, config: Value, credential: Option<&str>) -> Result<NodeResult, NodeError> {
    let n = node(config, credential);
    let cancel = AtomicBool::new(false);
    ex.execute(&NodeCtx { run_id: 7, node: &n, config: n.config.clone(), cancel: &cancel, timezone: "" })
}

fn plain(base: &str) -> Arc<N8nNodeExecutor> {
    N8nNodeExecutor::new(Some(base), Arc::new(MemorySecrets::new()))
}

// ---- calling a workflow -------------------------------------------------------------------------------------------------------------

#[test]
fn a_workflow_is_called_by_its_webhook_with_the_data_and_its_answer_is_handed_on() {
    let (base, seen) = fake_n8n(ok_json(json!({ "rows_added": 2, "ok": true })));
    let r = call(&plain(&base), json!({ "workflow": "post-to-sheet", "payload": { "title": "Hello", "n": 3 } }), None).unwrap();
    assert_eq!((r.outputs["status"].clone(), r.outputs["result"]["rows_added"].clone()), (json!(200), json!(2)), "{r:?}");
    let req = seen.lock().unwrap()[0].clone();
    assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/webhook/post-to-sheet"));
    assert_eq!(serde_json::from_str::<Value>(&req.body).unwrap(), json!({ "title": "Hello", "n": 3 }), "the data reached n8n as JSON");
    assert_eq!(req.header("idempotency-key").as_deref(), Some("aqua-run7-sheet_step"), "a workflow can recognise a repeat");
    assert_eq!((req.header("x-aqua-run").as_deref(), req.header("x-aqua-step").as_deref()), (Some("7"), Some("sheet_step")));
    assert!(r.logs.iter().all(|l| !l.contains("Hello")), "what was sent is not copied into the log: {:?}", r.logs);
    // A payload written as text that holds JSON is understood; none at all sends an empty object.
    call(&plain(&base), json!({ "workflow": "a/b-c_d", "payload": "{\"x\": 1}" }), None).unwrap();
    call(&plain(&base), json!({ "workflow": "empty" }), None).unwrap();
    let all = seen.lock().unwrap().clone();
    assert_eq!((all[1].path.as_str(), all[1].body.as_str()), ("/webhook/a/b-c_d", "{\"x\":1}"));
    assert_eq!(all[2].body, "{}");
}

#[test]
fn the_test_address_get_with_a_query_text_answers_and_empty_answers_are_all_understood() {
    let (base, seen) = fake_n8n(Arc::new(|r| match r.path.split('?').next().unwrap() {
        "/webhook/text" => Reply::Status(200, "text/plain", "plain words".into()),
        "/webhook/nothing" => Reply::Status(204, "text/plain", String::new()),
        _ => Reply::Status(200, "application/json", "{\"echo\":true}".into()),
    }));
    let ex = plain(&base);
    call(&ex, json!({ "workflow": "wf", "mode": "test" }), None).unwrap();
    assert_eq!(seen.lock().unwrap()[0].path, "/webhook-test/wf");
    let g = call(&ex, json!({ "workflow": "wf", "method": "GET", "payload": { "topic": "AI agents", "count": 3 } }), None).unwrap();
    let req = seen.lock().unwrap()[1].clone();
    assert!(req.method == "GET" && req.path.starts_with("/webhook/wf?") && req.path.contains("topic=AI") && req.path.contains("count=3") && req.body.is_empty(), "{req:?}");
    assert_eq!(g.outputs["result"]["echo"], true);
    assert_eq!(call(&ex, json!({ "workflow": "text" }), None).unwrap().outputs["result"], "plain words");
    let empty = call(&ex, json!({ "workflow": "nothing" }), None).unwrap();
    assert_eq!((empty.outputs["result"].clone(), empty.outputs["status"].clone()), (Value::Null, json!(204)));
    let e = call(&ex, json!({ "workflow": "wf", "method": "GET", "payload": [1, 2] }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("must be a JSON object")), "{e:?}");
}

#[test]
fn a_very_long_answer_is_cut_at_a_megabyte() {
    let (base, _) = fake_n8n(Arc::new(|_| Reply::Status(200, "text/plain", "x".repeat(3 * 1024 * 1024))));
    let r = call(&plain(&base), json!({ "workflow": "big" }), None).unwrap();
    assert_eq!(r.outputs["result"].as_str().unwrap().len(), MAX_RESPONSE_BYTES as usize);
}

// ---- the credential -------------------------------------------------------------------------------------------------------------------

#[test]
fn the_webhooks_password_is_a_named_credential_sent_in_a_header_and_never_seen_anywhere_else() {
    let secret = "s3cr3t-webhook-value-9f8e7d";
    let (base, seen) = fake_n8n(Arc::new(move |r| if r.header("X-Aqua-Token").as_deref() == Some("s3cr3t-webhook-value-9f8e7d") { Reply::Status(200, "application/json", "{\"ok\":true}".into()) } else { Reply::Status(403, "application/json", "{\"message\":\"Authorization data is wrong!\"}".into()) }));
    let ex = N8nNodeExecutor::new(Some(&base), secrets_with("n8n/hook", secret));
    let r = call(&ex, json!({ "workflow": "wf" }), Some("n8n/hook")).unwrap();
    assert_eq!(r.outputs["result"]["ok"], true);
    assert!(!format!("{r:?}").contains(secret), "the secret is not in the outputs or the log");
    // Another header name, as the webhook's Header Auth may use.
    let (base2, seen2) = fake_n8n(ok_json(json!({}))) ;
    call(&N8nNodeExecutor::new(Some(&base2), secrets_with("n8n/hook", secret)), json!({ "workflow": "wf", "auth_header": "X-My-Key" }), Some("n8n/hook")).unwrap();
    assert_eq!(seen2.lock().unwrap()[0].header("x-my-key").as_deref(), Some(secret));
    // Without the credential the webhook says no, and the answer is read as a login problem.
    let e = call(&plain(&base), json!({ "workflow": "wf" }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("403") && m.contains("refused the credential")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Authentication);
    let wrong = N8nNodeExecutor::new(Some(&base), secrets_with("n8n/hook", "another-value-entirely"));
    let e = call(&wrong, json!({ "workflow": "wf" }), Some("n8n/hook")).unwrap_err();
    assert!(!e.message().contains("another-value-entirely") && !e.message().contains(secret), "an error never carries the secret: {}", e.message());
    // A credential that is not saved stops before anything is sent.
    let before = seen.lock().unwrap().len();
    let e = call(&N8nNodeExecutor::new(Some(&base), Arc::new(MemorySecrets::new())), json!({ "workflow": "wf" }), Some("n8n/missing")).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("not logged in") && m.contains("n8n/missing") && m.contains("not saved")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Authentication);
    assert_eq!(seen.lock().unwrap().len(), before, "nothing was sent");
    // A header that could break the request is refused.
    for bad in ["Host", "Content-Length", "X-Bad Header", "X\r\nEvil", "Idempotency-Key"] {
        let e = call(&ex, json!({ "workflow": "wf", "auth_header": bad }), Some("n8n/hook")).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains("cannot be used as the credential's header")), "{bad}: {e:?}");
    }
}

// ---- what went wrong, and what that means for trying again -----------------------------------------------------------------------------------

#[test]
fn each_way_a_call_can_fail_is_told_apart_by_whether_the_workflow_may_have_run() {
    let status = |code: u16| fake_n8n(Arc::new(move |_| Reply::Status(code, "application/json", "{\"message\":\"boom\"}".into()))).0;
    let one = |code: u16, wf: &str, mode: &str| call(&plain(&status(code)), json!({ "workflow": wf, "mode": mode }), None).unwrap_err();
    let e = one(404, "nope", "production");
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("was not found") && m.contains("only works when the workflow is active")), "{e:?}");
    assert!(matches!(&one(404, "nope", "test"), NodeError::Fatal(m) if m.contains("while n8n is waiting for a test event")));
    for code in [401, 403] {
        assert!(matches!(&one(code, "wf", "production"), NodeError::Fatal(m) if m.contains("unauthorized")), "{code}");
    }
    let e = one(429, "wf", "production");
    assert!(matches!(&e, NodeError::Retryable(m) if m.contains("Nothing was run")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::RateLimit);
    let e = one(400, "wf", "production");
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("refused the request (400)") && m.contains("boom")), "{e:?}");
    // A server error arrived after the request did: the workflow may have run.
    for code in [500, 502, 503] {
        let e = one(code, "wf", "production");
        assert!(matches!(&e, NodeError::Unknown(m) if m.contains("not known whether the workflow ran") && m.contains("executions")), "{code}: {e:?}");
    }
    // Nothing reached n8n: safe to try again.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let e = call(&plain(&format!("http://127.0.0.1:{port}")), json!({ "workflow": "wf" }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Retryable(m) if m.contains("could not connect") && m.contains("is it running")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Network);
    // The request went out and no answer came: unknown, never retried by itself.
    let (hang, seen) = fake_n8n(Arc::new(|_| Reply::Hang));
    let started = std::time::Instant::now();
    let e = call(&plain(&hang), json!({ "workflow": "slow", "timeout_secs": 1 }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Unknown(m) if m.contains("timed out after the request was sent")), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(6), "it gave up when it was told to");
    assert_eq!(seen.lock().unwrap().len(), 1, "and did not send it again");
    // A redirect is not followed: the request is not sent anywhere else.
    let (redir, seen) = fake_n8n(Arc::new(|_| Reply::Redirect));
    let e = call(&plain(&redir), json!({ "workflow": "wf" }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("redirect")), "{e:?}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

// ---- settings -----------------------------------------------------------------------------------------------------------------------------

#[test]
fn a_step_set_up_wrongly_says_so_and_sends_nothing() {
    let (base, seen) = fake_n8n(ok_json(json!({})));
    let ex = plain(&base);
    for (cfg, needle) in [
        (json!({ "workflow": "" }), "is not a webhook path"),
        (json!({ "workflow": "../admin" }), "is not a webhook path"),
        (json!({ "workflow": "a//b" }), "is not a webhook path"),
        (json!({ "workflow": "/rooted" }), "is not a webhook path"),
        (json!({ "workflow": "a b" }), "is not a webhook path"),
        (json!({ "workflow": "a?x=1" }), "is not a webhook path"),
        (json!({ "workflow": "wf", "method": "DELETE" }), "not supported"),
        (json!({ "workflow": "wf", "mode": "staging" }), "not an address to use"),
        (json!({ "workflow": "wf", "payload": "{not json" }), "must be valid JSON"),
    ] {
        let e = call(&ex, cfg.clone(), None).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{cfg}: {e:?}");
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing was sent for any of them");
    // An absurd wait is limited, not refused.
    call(&ex, json!({ "workflow": "wf", "timeout_secs": 999999 }), None).unwrap();
}

#[test]
fn n8n_must_be_set_up_and_on_this_computer() {
    let none = N8nNodeExecutor::new(None, Arc::new(MemorySecrets::new()));
    let e = call(&none, json!({ "workflow": "wf" }), None).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("n8n is not set up") && m.contains("AQUA_N8N_URL") && m.contains("Nothing was sent")), "{e:?}");
    for bad in ["http://example.com", "http://192.168.1.5:5678", "https://127.0.0.1:5678", "ftp://127.0.0.1", "127.0.0.1:5678"] {
        assert!(check_base(bad).is_err(), "{bad}");
        let e = call(&N8nNodeExecutor::new(Some(bad), Arc::new(MemorySecrets::new())), json!({ "workflow": "wf" }), None).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains("invalid setting")), "{bad}: {e:?}");
    }
    for good in ["http://127.0.0.1:5678", "http://localhost:5678/", "http://[::1]:5678"] {
        assert!(check_base(good).is_ok(), "{good}");
    }
    assert_eq!(check_base("http://localhost:5678/").unwrap(), "http://localhost:5678", "a trailing slash is dropped");
}

#[test]
fn the_health_check_says_whether_n8n_is_up_and_what_to_do_if_it_is_not() {
    let (base, seen) = fake_n8n(Arc::new(|r| if r.path == "/healthz" { Reply::Status(200, "application/json", "{\"status\":\"ok\"}".into()) } else { Reply::Status(404, "text/plain", String::new()) }));
    let up = health(Ok(Some(base.clone())), Duration::from_secs(3));
    assert_eq!((up.configured, up.reachable, up.base_url.as_deref()), (true, true, Some(base.as_str())), "{up:?}");
    assert_eq!(seen.lock().unwrap()[0].path, "/healthz", "it asks only n8n's health address");
    let (busy, _) = fake_n8n(Arc::new(|_| Reply::Status(503, "text/plain", String::new())));
    let h = health(Ok(Some(busy)), Duration::from_secs(3));
    assert!(h.configured && !h.reachable && h.detail.contains("503"), "{h:?}");
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let down = health(Ok(Some(format!("http://127.0.0.1:{port}"))), Duration::from_secs(3));
    assert!(down.configured && !down.reachable && down.detail.contains("is it running"), "{down:?}");
    let none = health(Ok(None), Duration::from_secs(1));
    assert!(!none.configured && none.detail.contains("AQUA_N8N_URL"), "{none:?}");
    let bad = health(Err("the n8n address must be on this computer".into()), Duration::from_secs(1));
    assert!(!bad.configured && bad.detail.contains("on this computer"), "{bad:?}");
}

// ---- inside a real run --------------------------------------------------------------------------------------------------------------------

fn n8n_flow(approval: &str) -> FlowDef {
    serde_json::from_value(json!({
        "name": "Tell n8n",
        "settings": { "approval": approval },
        "nodes": [
            { "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } },
            { "id": "call", "kind": "n8n", "name": "Add to the sheet", "config": { "workflow": "add-row", "payload": { "who": "Ada" } } },
            { "id": "say", "kind": "notification", "config": { "channel": "in_app", "message": "n8n said {{nodes.call.status}}" } }
        ],
        "edges": [{ "from": "start", "to": "call" }, { "from": "call", "to": "say" }]
    }))
    .unwrap()
}

fn rig(name: &str, base: &str, approval: &str) -> (Arc<FlowRunner>, Arc<RunStore>, i64) {
    let dir = std::env::temp_dir().join(format!("aqua_n8n_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(plain(base))), Arc::new(|_, c| !c.load(std::sync::atomic::Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    let id = flows.create("local", n8n_flow(approval), "user", "", 1_800_000_000).unwrap().id;
    (runner, runs, id)
}

#[test]
fn a_call_to_n8n_asks_first_then_calls_once_and_counts_as_a_success_with_its_answer_as_proof() {
    let (base, seen) = fake_n8n(ok_json(json!({ "ok": true })));
    let (runner, runs, id) = rig("gate", &base, "always");
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Waiting, "it stops before calling n8n");
    assert!(seen.lock().unwrap().is_empty(), "nothing was sent yet");
    let gate = runs.node_run(run.id, "call").unwrap().unwrap();
    assert!(matches!(&gate.wait, Some(WaitRequest::Approval { implicit: true, .. })), "{:?}", gate.wait);
    runner.respond("local", run.id, "call", Response { decision: Some("approved".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Succeeded, "{}", done.error);
    assert_eq!(seen.lock().unwrap().len(), 1, "called exactly once");
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("n8n said 200")), "{events:?}");
    let nodes = runs.node_runs(run.id).unwrap();
    let v = super::supervisor::judge(&done, &nodes, &done.def);
    assert_eq!(v.kind, super::supervisor::VerdictKind::Success, "n8n's own answer is the evidence: {v:?}");
    assert!(v.evidence.iter().any(|e| e.contains("200")));
}

#[test]
fn a_failed_answer_stops_for_a_person_and_a_call_that_was_made_is_never_made_twice_after_a_crash() {
    let (base, seen) = fake_n8n(Arc::new(|_| Reply::Status(500, "application/json", "{}".into())));
    let (runner, runs, id) = rig("unsure", &base, "never");
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let n = runs.node_run(run.id, "call").unwrap().unwrap();
    assert_eq!(n.status, NodeStatus::NeedsAction, "not success, not a silent failure");
    assert!(matches!(&n.wait, Some(WaitRequest::Failure { message, choices }) if message.contains("Check whether it happened") && choices.contains(&"done".to_string())), "{:?}", n.wait);
    assert_eq!(seen.lock().unwrap().len(), 1);
    runner.respond("local", run.id, "call", Response { choice: Some("done".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(seen.lock().unwrap().len(), 1, "\"it was done\" moves on without calling again");

    // A success, then the run is picked up again as after a restart: the recorded call is reused.
    let (base, seen) = fake_n8n(ok_json(json!({ "ok": true })));
    let (runner, runs, id) = rig("ledger", &base, "never");
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    runs.set_status(run.id, RunStatus::Interrupted, "restart", 1_800_000_100).unwrap();
    let mut n = runs.node_run(run.id, "call").unwrap().unwrap();
    n.status = NodeStatus::Pending;
    runs.save_node(&n).unwrap();
    runner.resume("local", run.id).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(seen.lock().unwrap().len(), 1, "the workflow was not started a second time");
}

#[test]
fn a_dry_run_shows_the_call_and_sends_nothing() {
    let (base, seen) = fake_n8n(ok_json(json!({})));
    let (runner, runs, id) = rig("dry", &base, "always");
    let run = runner.start("local", id, Map::new(), Value::Null, true, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert!(seen.lock().unwrap().is_empty());
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("Would run \"Add to the sheet\" here. Nothing was sent.")), "{events:?}");
}

#[test]
fn the_address_check_reads_the_host_properly_including_ipv6_and_tricks() {
    use super::net::{http_host, is_local_http};
    assert_eq!(http_host("http://127.0.0.1:5678/x").as_deref(), Some("127.0.0.1"));
    assert_eq!(http_host("http://[::1]:5678").as_deref(), Some("[::1]"));
    assert_eq!(http_host("http://[::1]").as_deref(), Some("[::1]"));
    assert_eq!(http_host("https://127.0.0.1"), None);
    for good in ["http://127.0.0.1", "http://localhost:80/a?b=1", "http://[::1]:7860/x"] {
        assert!(is_local_http(good), "{good}");
    }
    // A local name used as a user name or as part of another host does not make it local.
    for bad in ["http://localhost:80@evil.example/x", "http://127.0.0.1@evil.example/", "http://localhost.evil.example", "http://evil.example/127.0.0.1", "http://127.0.0.1.evil.example:5678", "http://[::2]", "http://", "localhost"] {
        assert!(!is_local_http(bad), "{bad}");
    }
}
