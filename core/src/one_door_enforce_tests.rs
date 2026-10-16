//! Card OA8-05 (tasks SE-16, AQ-08 and AG-13 of the OpenCode audit, decision D-03): what `AQUA_DOOR=on` does to a browser call and a desktop call, shown by running them, and what the door
//! costs. No model, no browser and no desktop is started: the providers are the real ones over a browser and a desktop that are never started (so every call here is refused or
//! answered no before it reaches either), or a fake provider that answers at once.
//!
//! * With the switch on, the agent loop's one dispatch (`call_named_tool`) sends a browser call and a desktop call through `ToolHost::call`: the arguments are validated, the door decides,
//!   the person is asked when the rules say so (through the host's approver, and the old route's own question is not put as well), the arguments are masked in the question, the
//!   answer and the ledger, and the ledger line is signed. With the switch at its default (`shadow`) nothing of that happens: the old route asks and the host's ledger stays empty.
//! * A question of the host is answered only from the app: the API token is turned away and the question stays open; a spoken request is refused at once and nobody is asked.
//! * `measure_what_the_door_adds_to_a_browser_call_against_the_shadow_comparison` (ignored: it measures, it does not check) prints the time the door adds around a call.
//!
//! What is not shown here is what the door does not take (the gaps are listed in `documentation/ONE_DOOR_PLAN.md`, with the file and line of each).

use crate::agent_runs::scope::RunScope;
use crate::agent_tools::{door_call, one_door_compare, one_door_preview, with_host};
use crate::api_gateway::auth::{require_local_auth, ApiAuth};
use crate::door_shadow::{tally_so_far, with_mode, DoorMode};
use crate::modules::browser_agent::call_named_tool;
use crate::test_support::{app_state, wait_until, BrokerAnswerer, ScriptedApprover, TempWorld};
use aqua_mcp_adapters::browser::{BrowserBackend, BrowserPolicyConfig, BrowserProvider, SiteMode};
use aqua_mcp_adapters::windows_mcp::guard::Foreground;
use aqua_mcp_adapters::windows_mcp::tiers;
use aqua_mcp_adapters::{new_browser_client, new_windows_client, WindowsUiProvider};
use aqua_native_tools::secrets::MemorySecrets;
use aqua_tool_host::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const API: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const UI: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A secret-shaped word that the redactor knows (an AWS access key id). Made here, so that no file of the repository holds one.
fn secret() -> String {
    format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE")
}

/// The tool host with the real browser provider and the real desktop provider over a browser and a desktop that are never started, a signed ledger, and a person who says no to every
/// question (and remembers the questions).
struct World {
    host: Arc<ToolHost>,
    backend: Arc<BrowserBackend>,
    ledger: Arc<AuditLedger>,
    approver: Arc<ScriptedApprover>,
    keys: Vec<Vec<u8>>,
    /// Keeps the folder of the test until the world is dropped.
    _world: TempWorld,
}

fn world(name: &str) -> World {
    let world = TempWorld::new(&format!("door_enforce_{name}"));
    let backend = Arc::new(BrowserBackend::new(&world.path("browser"), new_browser_client()));
    let mut policy = BrowserPolicyConfig::default();
    policy.mode = SiteMode::Ask;
    policy.allow = vec!["docs.example.org".into()];
    backend.set_policy(policy).unwrap();
    let ledger = Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")));
    let data = world.path("data");
    let signer = crate::audit_key::load_or_create(&MemorySecrets::new(), &data).unwrap();
    ledger.set_head_signer(Arc::new(signer));
    let keys = crate::audit_key::known_public_keys(&data);
    let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), ledger.clone()));
    host.register(Arc::new(BrowserProvider::new(backend.clone()))).unwrap();
    let front = Arc::new(Mutex::new(Some(Foreground { exe: "notepad.exe".into(), title: "Untitled - Notepad".into(), window: 1 })));
    host.register(Arc::new(WindowsUiProvider::isolated(Arc::new(Mutex::new(new_windows_client())), Arc::new(move || front.lock().unwrap().clone()), false, tiers::Policy::Strict))).unwrap();
    let approver = Arc::new(ScriptedApprover::denying());
    host.set_approver(approver.clone());
    World { host, backend, ledger, approver, keys, _world: world }
}

impl World {
    /// The newest ledger line of the run.
    fn newest(&self, run: &str) -> Value {
        self.ledger.recent(20, Some(run)).into_iter().next().unwrap_or_else(|| panic!("no ledger line for run {run:?}"))
    }

    /// The ledger, sealed with the signed head: the chain and the heads check with the key, and nothing is left unsigned.
    fn assert_the_ledger_is_signed(&self) {
        assert!(self.ledger.seal().unwrap().is_some(), "a head was written over the lines");
        let check = self.ledger.check(&self.keys);
        assert!(check.problems.is_empty() && check.broken_at_line.is_none(), "{check:?}");
        assert!(check.lines > 0 && check.heads >= 1 && check.unsealed_lines == 0 && check.covered_lines == check.lines, "{check:?}");
    }

    /// Runs `f` as the agent loop runs a call: the host of this thread, the mode, a run of its own (marked as having read outside text when `tainted`) and the limits of the test's own.
    /// `f` is given the run's id and what `call_named_tool` takes of the browser and the desktop.
    fn in_a_run<R>(&self, mode: DoorMode, tainted: bool, f: impl FnOnce(&str, &Option<Arc<BrowserBackend>>, &Option<Arc<Mutex<aqua_mcp_adapters::StdioMcpClient>>>) -> R) -> R {
        let (browser, windows) = (Some(self.backend.clone()), Some(Arc::new(Mutex::new(new_windows_client()))));
        let counters = Arc::new(crate::autonomy::Autonomy::new(crate::autonomy::Settings::default()));
        crate::autonomy::with_autonomy(counters, || {
            with_host(self.host.clone(), || {
                with_mode(mode, || {
                    let scope = RunScope::begin(Some(self.host.clone()));
                    let run = scope.id().to_string();
                    if tainted {
                        self.host.mark_tainted(&run);
                    }
                    f(&run, &browser, &windows)
                })
            })
        })
    }
}

fn dispatch(name: &str, args: Value, browser: &Option<Arc<BrowserBackend>>, windows: &Option<Arc<Mutex<aqua_mcp_adapters::StdioMcpClient>>>) -> (String, String) {
    call_named_tool(name, args, &[], browser, windows).unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

// ---------------------------------------------------------------------------------------------

/// With the door on, a browser call of the loop is made by `ToolHost::call`: validated, decided, asked, masked and written in the signed ledger as the agent's call of this run.
#[test]
fn with_the_door_on_a_browser_call_of_the_loop_is_validated_decided_asked_masked_and_written_in_the_signed_ledger() {
    let w = world("browser_on");
    let secret = secret();
    w.in_a_run(DoorMode::On, false, |run, browser, windows| {
        // Validated: a call with no address is refused by the host's schema, in its words, and the refusal is a line of the ledger.
        let (label, text) = dispatch("browser_navigate", json!({}), browser, windows);
        assert_eq!(label, "browser");
        assert!(text.starts_with("ERROR [INVALID_ARGUMENTS] browser.navigate:"), "{text}");
        let line = w.newest(run);
        assert_eq!((line["tool"].as_str(), line["ok"].as_bool(), line["caller"].as_str(), line["agent_run_id"].as_str()), (Some("browser.navigate"), Some(false), Some("agent"), Some(run)), "{line}");
        assert_eq!(line["error_code"].as_str(), Some("INVALID_ARGUMENTS"), "{line}");
        assert!(w.approver.asked().is_empty(), "a refusal before the decision asks nobody");
    });
    // Decided and asked: the run has read outside text, and the call types into a page and submits: the door asks every time. The old route would have put its own question to the
    // browser's broker; here the host asks and that one is not put.
    w.in_a_run(DoorMode::On, true, |run, browser, windows| {
        let answerer = BrokerAnswerer::denying(w.backend.clone());
        let args = json!({"target": "e1", "text": format!("my key is {secret}"), "submit": true});
        let (label, text) = dispatch("browser_type", args, browser, windows);
        assert_eq!(label, "browser");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] browser.type:"), "the person said no: {text}");
        assert!(!text.contains(&secret), "masked in the answer the agent reads: {text}");
        let asked = w.approver.asked();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].tool == "browser.type" && asked[0].always_ask && asked[0].agent_run_id == run, "{:?}", asked[0]);
        assert!(!format!("{:?}", asked[0]).contains(&secret), "masked in the question the person is shown: {:?}", asked[0]);
        // Masked and audited: the ledger line is the agent's call of this run, as the person answered it, with the classes the host worked out, and the key is not in it.
        let line = w.newest(run);
        assert_eq!((line["tool"].as_str(), line["decision"].as_str(), line["caller"].as_str(), line["ok"].as_bool()), (Some("browser.type"), Some("denied_by_user"), Some("agent"), Some(false)), "{line}");
        assert!(!line.to_string().contains(&secret) && line["args"].to_string().contains("REDACTED"), "masked in the ledger: {line}");
        let classes = line["classes"].to_string();
        assert!(classes.contains("NETWORK") && classes.contains("WRITE"), "{classes}");
        assert!(answerer.finish().is_empty(), "the door took the call, so the old route put no question of its own");
    });
    w.assert_the_ledger_is_signed();
}

/// The same for a desktop call: validated, decided, asked, masked and signed.
#[test]
fn with_the_door_on_a_desktop_call_of_the_loop_is_validated_decided_asked_masked_and_written_in_the_signed_ledger() {
    let w = world("desktop_on");
    let secret = secret();
    w.in_a_run(DoorMode::On, false, |run, browser, windows| {
        let (label, text) = dispatch("Type", json!({"loc": [1, 1]}), browser, windows);
        assert_eq!(label, "windows");
        assert!(text.starts_with("ERROR [INVALID_ARGUMENTS] win.type:"), "a typed text is required: {text}");
        let line = w.newest(run);
        assert_eq!((line["tool"].as_str(), line["ok"].as_bool(), line["caller"].as_str()), (Some("win.type"), Some(false), Some("agent")), "{line}");
        assert!(w.approver.asked().is_empty());
    });
    w.in_a_run(DoorMode::On, true, |run, browser, windows| {
        let answerer = BrokerAnswerer::denying(w.backend.clone());
        let (label, text) = dispatch("Type", json!({"loc": [1, 1], "text": format!("pin {secret}"), "press_enter": true}), browser, windows);
        assert_eq!(label, "windows");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] win.type:"), "the person said no: {text}");
        assert!(!text.contains(&secret), "{text}");
        let asked = w.approver.asked();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].tool == "win.type" && asked[0].always_ask, "a send after outside text is asked every time: {:?}", asked[0]);
        assert!(!format!("{:?}", asked[0]).contains(&secret), "{:?}", asked[0]);
        let line = w.newest(run);
        assert_eq!((line["tool"].as_str(), line["decision"].as_str(), line["caller"].as_str()), (Some("win.type"), Some("denied_by_user"), Some("agent")), "{line}");
        assert!(!line.to_string().contains(&secret) && line["args"].to_string().contains("REDACTED"), "{line}");
        assert!(answerer.finish().is_empty(), "the desktop guard's own question is not put as well");
    });
    w.assert_the_ledger_is_signed();
}

/// With the switch at its default nothing changes: the call takes the route it always took (the old route puts its own question to the broker), the host's approver is not asked, and
/// the host's ledger has no line for the call. `off` is the same.
#[test]
fn with_the_switch_at_its_default_a_browser_call_of_the_loop_goes_by_its_own_route_and_the_host_is_not_asked_and_writes_nothing() {
    assert_eq!(DoorMode::parse(None, None), DoorMode::Shadow, "the default of AQUA_DOOR is shadow");
    let w = world("browser_default");
    for mode in [DoorMode::Shadow, DoorMode::Off] {
        w.in_a_run(mode, true, |run, browser, windows| {
            let answerer = BrokerAnswerer::denying(w.backend.clone());
            let (label, text) = dispatch("browser_click", json!({"target": "e1"}), browser, windows);
            let questions = answerer.finish();
            assert_eq!(label, "guard", "{mode:?}: the old route's own gate answered");
            assert!(text.contains("CONFIRMATION_DENIED"), "{mode:?}: {text}");
            assert_eq!(questions.len(), 1, "{mode:?}: the old route put its own question to the person: {questions:?}");
            assert!(w.approver.asked().is_empty(), "{mode:?}: the host was not asked");
            assert!(w.ledger.recent(20, Some(run)).is_empty(), "{mode:?}: the call never went through ToolHost::call, so it has no line in the host's ledger");
            // And the door takes nothing: `door_call` answers None, and the caller goes on by its own route.
            assert_eq!(door_call(mode, Some(&w.host), "browser_click", json!({"target": "e1"}), Caller::Agent, run), None, "{mode:?}");
            assert_eq!(door_call(mode, Some(&w.host), "Click", json!({"loc": [1, 1]}), Caller::Agent, run), None, "{mode:?}");
        });
    }
    assert!(w.ledger.recent(20, None).is_empty(), "nothing at all was written by the host");
}

/// The comparison of the default mode records who made the call: the caller and the verdict against the old route, and the run (card OA8-05, `aqua-cli door-report`).
#[test]
fn the_shadow_comparison_keeps_the_caller_the_verdict_and_the_run_of_a_compared_call_as_words() {
    let w = world("shadow_facts");
    w.in_a_run(DoorMode::Shadow, false, |_run, _browser, _windows| {
        let before = tally_so_far();
        // `Replay` makes no other comparison in this crate's tests, so these counts are this test's own.
        let preview = one_door_preview("browser_click", &json!({"target": "e1"}), Caller::Replay);
        assert!(preview.is_some(), "the host knows the tool, so there is something to compare");
        one_door_compare(preview, "clicked");
        let after = tally_so_far();
        let key = after.by_caller.keys().find(|k| k.starts_with("browser.click|replay|")).cloned().expect("a caller count for the call");
        assert_eq!(after.by_caller[&key], before.by_caller.get(&key).copied().unwrap_or(0) + 1, "{key}");
        assert!(after.runs > before.runs, "the run was counted");
        // The door asks for a click where the old route ran it with no question: stricter, and in no case looser.
        assert!(key.ends_with("|stricter") || key.ends_with("|same"), "{key}");
        assert!(after.to_json()["by_caller"].as_object().is_some_and(|m| m.contains_key(&key)));
    });
}

// ---------------------------------------------------------------------------------------------

fn post(app: &axum::Router, path: &str, token: Option<&str>, body: Value) -> (axum::http::StatusCode, Value) {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut request = axum::http::Request::builder().method("POST").uri(path).header("host", "127.0.0.1:8767").header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app.clone().oneshot(request.body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body()).await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    })
}

/// With the door on, the host's question about a browser call is put where the app shows and answers it, and only the app answers it: the API token is turned away and the question stays
/// open; a spoken request is refused at once and nobody is asked. (The person here says no, so that nothing reaches the browser.)
#[test]
fn with_the_door_on_the_question_about_a_browser_call_is_answered_only_from_the_app_and_a_spoken_request_is_not_asked() {
    let world = TempWorld::new("door_enforce_approval");
    let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")))));
    let state = app_state(&world, host.clone());
    host.register(Arc::new(BrowserProvider::new(state.browser.clone()))).unwrap();
    host.set_approver(Arc::new(crate::tool_host::BrokerApprover::new(state.browser.clone())));
    let auth = ApiAuth::new(API.to_string(), 8767).with_ui_token(UI.to_string());
    let app = crate::api_gateway::server::api_routes().layer(axum::middleware::from_fn_with_state(auth, require_local_auth)).with_state(state.clone());

    // A run that has read outside text makes a call that sends something out, with the door on, on a thread of its own as a run does. The host asks, and the question is the app's.
    let run = "run_door_approval";
    host.mark_tainted(run);
    let (door_host, run_id) = (host.clone(), run.to_string());
    let call = std::thread::spawn(move || door_call(DoorMode::On, Some(&door_host), "browser_press_key", json!({"key": "Enter"}), Caller::Agent, &run_id));
    wait_until(Duration::from_secs(20), "the host's question to be open", || !state.browser.broker().pending().is_empty());
    let pending = state.browser.broker().pending();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].tool, "browser.press_key");
    let id = pending[0].request_id.clone();
    // The API token cannot answer it, in either way, and neither can a request with no token; the question is still there.
    for decision in ["approve", "approve_always", "deny"] {
        let (status, answer) = post(&app, "/api/v1/browser/confirm", Some(API), json!({"request_id": id, "decision": decision}));
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "{decision}: {answer}");
    }
    assert_eq!(post(&app, "/api/v1/browser/confirm", None, json!({"request_id": id, "decision": "approve"})).0, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(state.browser.broker().pending().len(), 1, "no script answered it");
    // The person, in the app, says no: the call ends with the host's words, and the ledger holds the person's answer.
    let (status, answer) = post(&app, "/api/v1/browser/confirm", Some(UI), json!({"request_id": id, "decision": "deny"}));
    assert_eq!(status, axum::http::StatusCode::OK, "{answer}");
    let text = call.join().unwrap().expect("the door took the call");
    assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] browser.press_key:"), "{text}");
    let line = host.audit().recent(10, Some(run)).into_iter().next().expect("a ledger line");
    assert_eq!((line["decision"].as_str(), line["caller"].as_str()), (Some("denied_by_user"), Some("agent")), "{line}");

    // A spoken request through the door is not asked: it is refused at once, and no question is put to the app.
    host.mark_tainted("run_door_voice");
    let text = door_call(DoorMode::On, Some(&host), "browser_press_key", json!({"key": "Enter"}), Caller::Voice, "run_door_voice").expect("the door took the call");
    assert!(text.starts_with("ERROR [") && !text.contains("CONFIRMATION_DENIED"), "refused by a rule, not by an answer: {text}");
    assert!(state.browser.broker().pending().is_empty(), "nobody was asked about a spoken request");
    let line = host.audit().recent(10, Some("run_door_voice")).into_iter().next().expect("a ledger line");
    assert_eq!((line["caller"].as_str(), line["ok"].as_bool()), (Some("voice"), Some(false)), "{line}");
}

// ---------------------------------------------------------------------------------------------
// What the door costs.

/// A browser whose two tools answer at once: `browser.click` looks like a click, `browser.press_key` is one that sends something out. They take the real names so that `door_call` and the
/// comparison find them, and they take part in the door as the real browser does. What it leaves out is the browser itself: a real call is a round trip to Chrome (tens to hundreds of
/// milliseconds), which is the same with the door or without it.
struct FakeBrowser;

fn fake_tool(name: &str) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: "A browser tool that answers at once.".into(),
        classes: vec![PermClass::Network, PermClass::Write],
        default_decision: Decision::Auto,
        input_schema: json!({"type": "object", "properties": {"target": {"type": "string"}, "key": {"type": "string"}}}),
        timeout_default_ms: 1_000,
        timeout_max_ms: 2_000,
        annotations: Annotations::default(),
        untrusted_output: false,
    }
}

impl ToolProvider for FakeBrowser {
    fn id(&self) -> &str {
        "fake-browser"
    }
    fn manifests(&self) -> Vec<ToolManifest> {
        vec![fake_tool("browser.click"), fake_tool("browser.press_key")]
    }
    fn classify(&self, tool: &str, _args: &Value) -> Result<Classification, ToolError> {
        Ok(Classification { classes: vec![], target: "e1".into(), summary: tool.into(), target_allowed: true })
    }
    fn sends_something_out(&self, tool: &str, _args: &Value) -> bool {
        tool == "browser.press_key"
    }
    fn door_applies(&self) -> bool {
        true
    }
    fn call<'a>(&'a self, _tool: &'a str, _args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        Box::pin(async { Ok(ProviderOutput { text: "clicked".into(), ..Default::default() }) })
    }
}

fn median_and_p95(mut micros: Vec<f64>) -> (f64, f64) {
    micros.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = micros.len();
    (micros[n / 2], micros[((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1])
}

fn timed<R>(f: impl FnOnce() -> R) -> (R, f64) {
    let started = Instant::now();
    let r = f();
    (r, started.elapsed().as_secs_f64() * 1e6)
}

/// A host with the fake browser, a ledger signed with a key held in memory (the real one signs a head every 64 lines), and a person who says yes at once. The host's limit of 60 calls a
/// minute to a tool that changes something (it applies to the browser and the desktop with the door on, and the old routes had none) is raised for this host, so that the measurement can
/// make hundreds of calls in a second (`limited: false`); `the_hosts_limit_of_sixty_changes_a_minute_to_one_tool_applies_to_a_browser_call_with_the_door_on` shows the limit itself.
fn fake_world(name: &str, limited: bool) -> (Arc<ToolHost>, TempWorld) {
    let world = TempWorld::new(&format!("door_latency_{name}"));
    let ledger = Arc::new(AuditLedger::open(&world.path("host.audit.jsonl")));
    ledger.set_head_signer(Arc::new(crate::audit_key::load_or_create(&MemorySecrets::new(), &world.path("data")).unwrap()));
    let engine = Arc::new(PolicyEngine::new(Mode::Standard));
    if !limited {
        engine.set_limits(Limits { reads_per_minute: 10_000, changes_per_minute: 10_000, ..Limits::default() });
    }
    let host = Arc::new(ToolHost::new(engine, ledger));
    host.register(Arc::new(FakeBrowser)).unwrap();
    host.set_approver(Arc::new(ScriptedApprover::approving_once()));
    (host, world)
}

/// A fast check that the fake is what the measurement says it is (it runs with the suite): the door takes the fake browser's calls with the switch on and not otherwise, the shadow
/// comparison sees them, and a call that sends something out after outside text is asked.
#[test]
fn the_fake_browser_of_the_measurement_is_taken_by_the_door_and_seen_by_the_comparison() {
    let (host, _world) = fake_world("sanity", false);
    with_host(host.clone(), || {
        let scope = RunScope::begin(Some(host.clone()));
        let run = scope.id().to_string();
        assert_eq!(door_call(DoorMode::On, Some(&host), "browser_click", json!({"target": "e1"}), Caller::Agent, &run).as_deref(), Some("clicked"));
        assert_eq!(door_call(DoorMode::Shadow, Some(&host), "browser_click", json!({"target": "e1"}), Caller::Agent, &run), None);
        let preview = one_door_preview("browser_click", &json!({"target": "e1"}), Caller::Agent);
        assert!(preview.is_some());
        one_door_compare(preview, "clicked");
        // After outside text a call that sends is asked (the scripted person says yes), and it still runs.
        host.mark_tainted(&run);
        assert_eq!(door_call(DoorMode::On, Some(&host), "browser_press_key", json!({"key": "Enter"}), Caller::Agent, &run).as_deref(), Some("clicked"));
        let line = host.audit().recent(10, Some(&run)).into_iter().next().unwrap();
        assert_eq!(line["decision"].as_str(), Some("approved"), "{line}");
    });
}

/// A difference that switching the door on brings (the plan names it among the risks): the host holds one tool to 60 calls a minute when the tool changes something, and the old routes of
/// the browser and the desktop had no such limit. The 61st click of a minute is answered `RATE_LIMITED` in the host's words, with the wait; a tool that only reads has 300.
#[test]
fn the_hosts_limit_of_sixty_changes_a_minute_to_one_tool_applies_to_a_browser_call_with_the_door_on() {
    let (host, _world) = fake_world("limit", true);
    with_host(host.clone(), || {
        let scope = RunScope::begin(Some(host.clone()));
        let run = scope.id().to_string();
        for i in 0..60 {
            let text = door_call(DoorMode::On, Some(&host), "browser_click", json!({"target": "e1"}), Caller::Agent, &run).unwrap();
            assert_eq!(text, "clicked", "call {i}");
        }
        let text = door_call(DoorMode::On, Some(&host), "browser_click", json!({"target": "e1"}), Caller::Agent, &run).unwrap();
        assert!(text.starts_with("ERROR [RATE_LIMITED] browser.click: browser.click was called 60 times in the last minute"), "{text}");
        // Another tool has its own count, and the comparison of the default mode is not limited at all.
        assert_eq!(door_call(DoorMode::On, Some(&host), "browser_press_key", json!({"key": "Enter"}), Caller::Agent, &run).as_deref(), Some("clicked"));
        assert_eq!(door_call(DoorMode::Shadow, Some(&host), "browser_click", json!({"target": "e1"}), Caller::Agent, &run), None, "the default mode takes no call, so it limits none");
    });
}

/// MEASUREMENT, not a check (`cargo test -p aqua-core --lib one_door_enforce_tests::measure -- --ignored --nocapture`). What `ToolHost::call` adds around one browser call against what the
/// shadow comparison adds, with a browser that answers at once, in microseconds, over `ROUNDS` calls each, interleaved so that a busy machine bears on all of them alike.
/// * `bare`: the call's own work (nothing: the stand-in for the old route returns a string).
/// * `shadow`: the old route plus what shadow mode does around it (`one_door_preview` asks the host what it would decide, `one_door_compare` counts it and may write the file).
/// * `door auto`: `door_call` with the switch on: validate, decide, run, mask, write the signed ledger line. No question.
/// * `shadow asked` and `door asked`: the same in a run that has read outside text, for a call that sends something out; the person answers at once, so what is measured is the host's
///   own time to ask, not a person's.
#[test]
#[ignore = "a measurement: run it by name with --ignored --nocapture"]
fn measure_what_the_door_adds_to_a_browser_call_against_the_shadow_comparison() {
    const ROUNDS: usize = 300;
    const WARM_UP: usize = 30;
    let (host, _world) = fake_world("measure", false);
    let click = json!({"target": "e1"});
    let press = json!({"key": "Enter"});
    let (mut bare, mut shadow, mut door_auto) = (Vec::new(), Vec::new(), Vec::new());
    let (mut shadow_asked, mut door_asked) = (Vec::new(), Vec::new());
    with_host(host.clone(), || {
        with_mode(DoorMode::Shadow, || {
            // A clean run: nothing is asked.
            {
                let scope = RunScope::begin(Some(host.clone()));
                let run = scope.id().to_string();
                for i in 0..(WARM_UP + ROUNDS) {
                    let (_, t_bare) = timed(|| std::hint::black_box("clicked".to_string()));
                    let (_, t_shadow) = timed(|| {
                        let preview = one_door_preview("browser_click", &click, Caller::Agent);
                        let observation = std::hint::black_box("clicked".to_string());
                        one_door_compare(preview, &observation);
                    });
                    let (text, t_door) = timed(|| door_call(DoorMode::On, Some(&host), "browser_click", click.clone(), Caller::Agent, &run));
                    assert_eq!(text.as_deref(), Some("clicked"));
                    if i >= WARM_UP {
                        bare.push(t_bare);
                        shadow.push(t_shadow);
                        door_auto.push(t_door);
                    }
                }
            }
            // A run that has read outside text: the call sends something out, so the door asks every time.
            {
                let scope = RunScope::begin(Some(host.clone()));
                let run = scope.id().to_string();
                host.mark_tainted(&run);
                for i in 0..(WARM_UP + ROUNDS) {
                    let (_, t_shadow) = timed(|| {
                        let preview = one_door_preview("browser_press_key", &press, Caller::Agent);
                        let observation = std::hint::black_box("clicked".to_string());
                        one_door_compare(preview, &observation);
                    });
                    let (text, t_door) = timed(|| door_call(DoorMode::On, Some(&host), "browser_press_key", press.clone(), Caller::Agent, &run));
                    assert_eq!(text.as_deref(), Some("clicked"), "the person said yes");
                    if i >= WARM_UP {
                        shadow_asked.push(t_shadow);
                        door_asked.push(t_door);
                    }
                }
            }
        });
    });
    let line = |name: &str, v: Vec<f64>| {
        let (median, p95) = median_and_p95(v);
        println!("door-latency {name:<12} median {median:>9.1} us   p95 {p95:>9.1} us   (n = {ROUNDS})");
        median
    };
    println!("door-latency: a browser call that answers at once, through the shadow comparison and through ToolHost::call (microseconds, per call)");
    let m_bare = line("bare", bare);
    let m_shadow = line("shadow", shadow);
    let m_door = line("door auto", door_auto);
    let m_shadow_asked = line("shadow asked", shadow_asked);
    let m_door_asked = line("door asked", door_asked);
    println!("door-latency: the door adds {:.1} us over the shadow path in the median (auto) and {:.1} us (asked); the shadow path adds {:.1} us over the bare call", m_door - m_shadow, m_door_asked - m_shadow_asked, m_shadow - m_bare);
}
