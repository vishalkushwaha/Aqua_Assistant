use super::browser_exec::*;
use super::model::*;
use super::run::exec::*;
use super::run::manager::FlowRunner;
use super::run::model::*;
use super::run::store::RunStore;
use super::store::FlowStore;
use super::supervisor::{classify, ErrorClass};
use aqua_mcp_adapters::browser::{BrowserErrorCode, BrowserObservation};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- a page in memory ---------------------------------------------------------------------------------------------------------

/// A browser that shows `elements` and records what it is asked to do. `fail` makes a tool fail once, in turn.
struct FakeBrowser {
    url: Mutex<String>,
    title: Mutex<String>,
    elements: Mutex<Vec<(String, String)>>,
    calls: Mutex<Vec<(String, Value)>>,
    fail: Mutex<HashMap<String, Vec<BrowserErrorCode>>>,
    /// Refs are renumbered on every look, as a real page does after it changes.
    epoch: Mutex<u32>,
    /// For each call: was the browser told a person had approved this step?
    approved: Mutex<Vec<(String, bool)>>,
}

impl FakeBrowser {
    fn new(elements: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self { url: Mutex::new("https://example.com/".into()), title: Mutex::new("Example".into()), elements: Mutex::new(elements.iter().map(|(r, n)| (r.to_string(), n.to_string())).collect()), calls: Mutex::new(vec![]), fail: Mutex::new(HashMap::new()), epoch: Mutex::new(0), approved: Mutex::new(vec![]) })
    }
    fn set(&self, elements: &[(&str, &str)]) {
        *self.elements.lock().unwrap() = elements.iter().map(|(r, n)| (r.to_string(), n.to_string())).collect();
    }
    fn failing(&self, tool: &str, codes: Vec<BrowserErrorCode>) {
        self.fail.lock().unwrap().insert(tool.into(), codes);
    }
    fn tools(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|c| c.0.clone()).collect()
    }
    fn args_of(&self, tool: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|c| c.0 == tool).map(|c| c.1.clone()).collect()
    }
    fn snapshot(&self, filter: Option<&str>) -> String {
        let epoch = *self.epoch.lock().unwrap();
        let els = self.elements.lock().unwrap();
        let mut out = format!("- generic [ref=e{}]:\n", epoch * 100);
        for (i, (role, name)) in els.iter().enumerate() {
            if filter.is_none_or(|f| name.to_lowercase().contains(&f.to_lowercase())) {
                out.push_str(&format!("  - {role} \"{name}\" [ref=e{}]\n", epoch * 100 + i as u32 + 1));
            }
        }
        out
    }
}

fn ok(tool: &str, text: &str) -> BrowserObservation {
    BrowserObservation { ok: true, tool: tool.into(), backend: "managed".into(), url: Some("https://example.com/".into()), title: Some("Example".into()), text: text.into(), data: None, truncated: false, artifacts: vec![], confirmation: None, duration_ms: 1, error: None }
}

impl Driver for FakeBrowser {
    fn call(&self, tool: &str, args: Value) -> BrowserObservation {
        self.calls.lock().unwrap().push((tool.to_string(), args.clone()));
        self.approved.lock().unwrap().push((tool.to_string(), aqua_mcp_adapters::browser::is_pre_approved()));
        if let Some(code) = self.fail.lock().unwrap().get_mut(tool).filter(|v| !v.is_empty()).map(|v| v.remove(0)) {
            return BrowserObservation::failure(tool, "managed", code, format!("{code:?} (simulated)"));
        }
        *self.epoch.lock().unwrap() += 1;
        match tool {
            "browser_navigate" => {
                let u = args["url"].as_str().unwrap_or("").to_string();
                *self.url.lock().unwrap() = u.clone();
                let mut o = ok(tool, "");
                o.url = Some(u);
                o.title = Some(self.title.lock().unwrap().clone());
                o
            }
            "browser_snapshot" => {
                let mut o = ok(tool, &self.snapshot(None));
                o.url = Some(self.url.lock().unwrap().clone());
                o
            }
            "browser_find" => ok(tool, &format!("Found matches:\n{}", self.snapshot(args["text"].as_str()))),
            "browser_take_screenshot" => {
                let mut o = ok(tool, "saved");
                o.artifacts = vec![aqua_mcp_adapters::browser::observation::Artifact { kind: "screenshot".into(), path: "data/browser/artifacts/page.png".into() }];
                o
            }
            "aqua_browser_extract" => ok(tool, "The article text."),
            _ => ok(tool, "done"),
        }
    }
}

fn node(kind: NodeKind, config: Value) -> Node {
    serde_json::from_value(json!({ "id": "n", "kind": kind, "name": "Step", "config": config })).unwrap()
}

fn run(fake: &Arc<FakeBrowser>, kind: NodeKind, config: Value) -> Result<NodeResult, NodeError> {
    let exec = BrowserNodeExecutor::new(fake.clone());
    let n = node(kind, config.clone());
    let cancel = AtomicBool::new(false);
    exec.execute(&NodeCtx { run_id: 1, node: &n, config, cancel: &cancel, timezone: "" })
}

fn form() -> Vec<(&'static str, &'static str)> {
    vec![("heading", "Contact us"), ("textbox", "Name"), ("textbox", "Email"), ("combobox", "Topic"), ("button", "Send")]
}

// ---- finding elements ----------------------------------------------------------------------------------------------------------

#[test]
fn elements_are_read_from_a_snapshot_and_matched_by_role_and_visible_name() {
    let snap = "- generic [active] [ref=f1e1]:\n  - heading \"Contact us\" [level=1] [ref=f1e2]\n  - textbox \"Name\" [ref=f1e4]\n  - button \"Send\" [ref=f1e11]\n  - link \"Send feedback\" [ref=f1e12] [cursor=pointer]:\n    - /url: /fb\n  - status\n";
    let page = elements(snap);
    assert_eq!(page.iter().map(|e| (e.role.as_str(), e.name.as_str(), e.target.as_str())).collect::<Vec<_>>(), vec![("heading", "Contact us", "f1e2"), ("textbox", "Name", "f1e4"), ("button", "Send", "f1e11"), ("link", "Send feedback", "f1e12")]);
    assert!(matches!(pick("Send", &page), Found::One(e) if e.target == "f1e11"), "an exact name beats one that only contains it");
    assert!(matches!(pick("button:send", &page), Found::One(e) if e.role == "button"), "case does not matter");
    assert!(matches!(pick("link:Send", &page), Found::One(e) if e.name == "Send feedback"), "the role narrows a partial name");
    assert!(matches!(pick("name", &page), Found::One(e) if e.name == "Name"));
    match pick("Submit", &page) {
        Found::Missing(near) => assert!(!near.is_empty(), "it says what is on the page instead"),
        _ => panic!("nothing is called Submit"),
    }
    let twice = elements("- button \"Send\" [ref=a]\n- button \"Send\" [ref=b]\n");
    assert!(matches!(pick("Send", &twice), Found::Ambiguous(l) if l.len() == 2), "two equal matches are not guessed between");
}

#[test]
fn an_element_is_found_by_name_even_after_the_page_renumbers_and_reorders() {
    let fake = FakeBrowser::new(&form());
    run(&fake, NodeKind::Browser, json!({ "action": "click", "target": "button:Send" })).unwrap();
    let first = fake.args_of("browser_click")[0].clone();
    // The page changes: another button appears first, and every reference is new.
    fake.set(&[("button", "Cancel"), ("textbox", "Name"), ("button", "Send")]);
    run(&fake, NodeKind::Browser, json!({ "action": "click", "target": "button:Send" })).unwrap();
    let second = fake.args_of("browser_click")[1].clone();
    assert_eq!(first["element"], "Send");
    assert_eq!(second["element"], "Send");
    assert_ne!(first["target"], second["target"], "no reference is remembered between looks");
}

// ---- opening a website ------------------------------------------------------------------------------------------------------------

#[test]
fn opening_a_website_reports_the_page_and_whether_a_sign_in_form_is_showing() {
    let fake = FakeBrowser::new(&form());
    let r = run(&fake, NodeKind::Website, json!({ "url": "https://example.com/contact" })).unwrap();
    assert_eq!(r.outputs["logged_in"], json!(true));
    assert!(r.outputs["page"].as_str().unwrap().contains("Example") && r.outputs["page"].as_str().unwrap().contains("/contact"));
    assert_eq!(fake.args_of("browser_navigate")[0]["url"], "https://example.com/contact");

    fake.set(&[("heading", "Sign in"), ("textbox", "Email"), ("textbox", "Password"), ("button", "Sign in")]);
    let r = run(&fake, NodeKind::Website, json!({ "url": "https://example.com/login" })).unwrap();
    assert_eq!(r.outputs["logged_in"], json!(false));
    let e = run(&fake, NodeKind::Website, json!({ "url": "https://example.com/account", "require_login": true })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("not logged in") && m.contains("never asks for or stores the password")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Authentication, "so it is treated as a login problem, not retried");
    assert!(matches!(run(&fake, NodeKind::Website, json!({ "url": "" })), Err(NodeError::Fatal(m)) if m.contains("empty")));
}

// ---- the actions -----------------------------------------------------------------------------------------------------------------------

#[test]
fn each_action_does_one_thing_through_the_guarded_browser() {
    let fake = FakeBrowser::new(&form());
    run(&fake, NodeKind::Browser, json!({ "action": "type", "target": "textbox:Name", "value": "Ada Lovelace" })).unwrap();
    assert_eq!(fake.args_of("browser_type")[0]["text"], "Ada Lovelace");
    let typed = run(&fake, NodeKind::Browser, json!({ "action": "type", "target": "Email", "value": "ada@example.com" })).unwrap();
    assert!(typed.logs.iter().all(|l| !l.contains("ada@example.com")), "what was typed is not written to the log: {:?}", typed.logs);
    run(&fake, NodeKind::Browser, json!({ "action": "select", "target": "Topic", "value": "Support" })).unwrap();
    assert_eq!(fake.args_of("browser_select_option")[0]["values"], json!(["Support"]));
    let read = run(&fake, NodeKind::Browser, json!({ "action": "read" })).unwrap();
    assert_eq!(read.outputs["result"], "The article text.");
    let one = run(&fake, NodeKind::Browser, json!({ "action": "read", "target": "heading:Contact us" })).unwrap();
    assert_eq!(one.outputs["result"], "Contact us");
    run(&fake, NodeKind::Browser, json!({ "action": "wait_for", "value": "Message sent" })).unwrap();
    run(&fake, NodeKind::Browser, json!({ "action": "wait_for", "value": "600" })).unwrap();
    let waits = fake.args_of("browser_wait_for");
    assert_eq!((waits[0]["text"].clone(), waits[1]["time"].clone()), (json!("Message sent"), json!(30)), "a wait is capped at 30 seconds");
    let shot = run(&fake, NodeKind::Browser, json!({ "action": "screenshot" })).unwrap();
    assert_eq!(shot.outputs["result"], "data/browser/artifacts/page.png");

    // Upload: open the chooser, then hand over the file.
    let up = FakeBrowser::new(&[("button", "Add photo")]);
    run(&up, NodeKind::Browser, json!({ "action": "upload", "target": "button:Add photo", "value": "uploads/pic.png" })).unwrap();
    let order: Vec<String> = up.tools().into_iter().filter(|t| t == "browser_click" || t == "browser_file_upload").collect();
    assert_eq!(order, vec!["browser_click", "browser_file_upload"]);
    assert_eq!(up.args_of("browser_file_upload")[0]["paths"], json!(["uploads/pic.png"]));
}

#[test]
fn a_step_that_is_set_up_wrongly_says_so_before_touching_the_page() {
    let fake = FakeBrowser::new(&form());
    for (cfg, needle) in [
        (json!({ "action": "click" }), "needs an element"),
        (json!({ "action": "type", "value": "x" }), "needs an element"),
        (json!({ "action": "upload", "value": "" }), "needs the file path"),
        (json!({ "action": "wait_for" }), "needs text"),
        (json!({ "action": "hover", "target": "x" }), "not supported"),
    ] {
        let e = run(&fake, NodeKind::Browser, cfg.clone()).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{cfg}: {e:?}");
        assert_eq!(classify(e.message()), ErrorClass::InvalidInput, "{cfg}");
    }
    assert!(fake.tools().is_empty(), "nothing was sent to the browser: {:?}", fake.tools());
    let two = FakeBrowser::new(&[("button", "Send"), ("button", "Send")]);
    let e = run(&two, NodeKind::Browser, json!({ "action": "click", "target": "Send" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("more than one element")), "{e:?}");
    assert!(two.args_of("browser_click").is_empty(), "it did not guess which one");
}

#[test]
fn when_the_page_changed_the_error_says_what_is_there_and_is_classed_as_a_page_change() {
    let fake = FakeBrowser::new(&[("textbox", "Name"), ("button", "Submit message")]);
    let e = run(&fake, NodeKind::Browser, json!({ "action": "click", "target": "button:Send" })).unwrap_err();
    let NodeError::Fatal(m) = &e else { panic!("{e:?}") };
    assert!(m.contains("element not found") && m.contains("Submit message"), "it lists what the page shows now: {m}");
    assert_eq!(classify(m), ErrorClass::WebsiteUiChange);
    assert!(fake.args_of("browser_click").is_empty(), "nothing was clicked");
}

#[test]
fn what_may_be_retried_depends_on_whether_the_step_changes_the_page() {
    // Looking is safe to repeat.
    let fake = FakeBrowser::new(&form());
    fake.failing("browser_navigate", vec![BrowserErrorCode::NavigationFailed]);
    let e = run(&fake, NodeKind::Website, json!({ "url": "https://example.com" })).unwrap_err();
    assert!(matches!(e, NodeError::Retryable(_)));
    fake.failing("browser_snapshot", vec![BrowserErrorCode::Timeout]);
    fake.failing("aqua_browser_extract", vec![BrowserErrorCode::Timeout]);
    assert!(matches!(run(&fake, NodeKind::Browser, json!({ "action": "read" })), Err(NodeError::Retryable(_))));
    fake.failing("browser_wait_for", vec![BrowserErrorCode::BrowserCrashed]);
    assert!(matches!(run(&fake, NodeKind::Browser, json!({ "action": "wait_for", "value": "x" })), Err(NodeError::Retryable(_))));

    // Changing it is not: after a timeout nobody knows whether the click happened.
    fake.failing("browser_click", vec![BrowserErrorCode::Timeout]);
    let e = run(&fake, NodeKind::Browser, json!({ "action": "click", "target": "Send" })).unwrap_err();
    assert!(matches!(&e, NodeError::Unknown(m) if m.contains("not known whether it happened")), "{e:?}");
    fake.failing("browser_type", vec![BrowserErrorCode::BrowserCrashed]);
    assert!(matches!(run(&fake, NodeKind::Browser, json!({ "action": "type", "target": "Name", "value": "x" })), Err(NodeError::Unknown(_))));

    // Refusals are final, and read as what they are.
    for (code, class) in [(BrowserErrorCode::ConfirmationDenied, ErrorClass::PermissionError), (BrowserErrorCode::PolicyBlockedOrigin, ErrorClass::PermissionError), (BrowserErrorCode::InvalidArguments, ErrorClass::InvalidInput), (BrowserErrorCode::ElementNotFound, ErrorClass::WebsiteUiChange)] {
        fake.failing("browser_click", vec![code]);
        let e = run(&fake, NodeKind::Browser, json!({ "action": "click", "target": "Send" })).unwrap_err();
        assert!(matches!(e, NodeError::Fatal(_)), "{code:?}: {e:?}");
        assert_eq!(classify(e.message()), class, "{code:?}: {}", e.message());
    }
}

#[test]
fn a_stopped_run_does_not_touch_the_page() {
    let fake = FakeBrowser::new(&form());
    let exec = BrowserNodeExecutor::new(fake.clone());
    let n = node(NodeKind::Browser, json!({ "action": "click", "target": "Send" }));
    let cancel = AtomicBool::new(true);
    assert!(exec.execute(&NodeCtx { run_id: 1, node: &n, config: n.config.clone(), cancel: &cancel, timezone: "" }).is_err());
    assert!(fake.tools().is_empty());
}

// ---- inside a real run ---------------------------------------------------------------------------------------------------------------

#[test]
fn a_flow_opens_a_page_fills_a_form_clicks_send_and_reads_the_confirmation_in_a_real_run() {
    let dir = std::env::temp_dir().join(format!("aqua_browser_flow_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let fake = FakeBrowser::new(&form());
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(BrowserNodeExecutor::new(fake.clone()))), Arc::new(|_, c| !c.load(Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    let def: FlowDef = serde_json::from_value(json!({
        "name": "Contact form",
        "settings": { "approval": "never" },
        "variables": [{ "name": "who", "label": "Name", "kind": "text", "required": true, "default": "Ada" }],
        "nodes": [
            { "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } },
            { "id": "open", "kind": "website", "name": "Open the form", "config": { "url": "https://example.com/contact", "require_login": true } },
            { "id": "name", "kind": "browser", "name": "Type the name", "config": { "action": "type", "target": "textbox:Name", "value": "{{vars.who}}" } },
            { "id": "send", "kind": "browser", "name": "Send", "config": { "action": "click", "target": "button:Send" } },
            { "id": "seen", "kind": "browser", "name": "Read the result", "config": { "action": "read" } },
            { "id": "say", "kind": "notification", "config": { "channel": "in_app", "message": "signed in: {{nodes.open.logged_in}}; page says: {{nodes.seen.result}}" } }
        ],
        "edges": [{ "from": "start", "to": "open" }, { "from": "open", "to": "name" }, { "from": "name", "to": "send" }, { "from": "send", "to": "seen" }, { "from": "seen", "to": "say" }]
    }))
    .unwrap();
    let f = flows.create("local", def, "user", "", 1_800_000_000).unwrap();
    let mut vars = Map::new();
    vars.insert("who".into(), json!("Grace"));
    let run = runner.start("local", f.id, vars, Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let done = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Succeeded, "{}", done.error);
    let tools = fake.tools();
    let pos = |t: &str| tools.iter().position(|x| x == t).unwrap();
    assert!(pos("browser_navigate") < pos("browser_type") && pos("browser_type") < pos("browser_click"), "in order: {tools:?}");
    assert_eq!(fake.args_of("browser_type")[0]["text"], "Grace", "the flow's input reached the page");
    let events: Vec<String> = runs.events("local", run.id, 0, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|m| m.contains("signed in: true; page says: The article text.")), "{events:?}");
    assert!(events.iter().all(|m| !m.contains("Grace")), "what was typed is not in the log: {events:?}");
}

// ---- approval before a click ---------------------------------------------------------------------------------------------------------

fn click_flow(approval: &str, explicit_approval: bool) -> FlowDef {
    let mut nodes = vec![
        json!({ "id": "start", "kind": "trigger", "config": { "trigger_type": "manual" } }),
        json!({ "id": "name", "kind": "browser", "name": "Type the name", "config": { "action": "type", "target": "textbox:Name", "value": "Ada" } }),
    ];
    let mut edges = vec![json!({ "from": "start", "to": "name" })];
    let mut last = "name".to_string();
    if explicit_approval {
        nodes.push(json!({ "id": "ok", "kind": "approval", "name": "Send it?", "config": { "title": "Send the message?", "preview": "Ada" } }));
        edges.push(json!({ "from": "name", "to": "ok" }));
        last = "ok".into();
    }
    nodes.push(json!({ "id": "send", "kind": "browser", "name": "Send", "config": { "action": "click", "target": "button:Send" } }));
    edges.push(json!({ "from": last, "to": "send", "port": if explicit_approval { "approved" } else { "out" } }));
    serde_json::from_value(json!({ "name": "Click flow", "settings": { "approval": approval }, "nodes": nodes, "edges": edges })).unwrap()
}

fn click_rig(name: &str, def: FlowDef) -> (Arc<FakeBrowser>, Arc<FlowRunner>, Arc<RunStore>, i64) {
    let dir = std::env::temp_dir().join(format!("aqua_browser_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let fake = FakeBrowser::new(&form());
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(BrowserNodeExecutor::new(fake.clone()))), Arc::new(|_, c| !c.load(Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    let id = flows.create("local", def, "user", "", 1_800_000_000).unwrap().id;
    (fake, runner, runs, id)
}

fn approved_at(fake: &FakeBrowser, tool: &str) -> Vec<bool> {
    fake.approved.lock().unwrap().iter().filter(|c| c.0 == tool).map(|c| c.1).collect()
}

#[test]
fn a_click_asks_first_by_itself_and_only_an_approved_click_is_let_through_the_browsers_own_check() {
    let (fake, runner, runs, id) = click_rig("gate", click_flow("always", false));
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let waiting = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(waiting.status, RunStatus::Waiting, "it stops before the click, not after");
    assert!(fake.args_of("browser_click").is_empty(), "nothing was clicked");
    assert_eq!(approved_at(&fake, "browser_type"), vec![false], "typing is not a click, so it neither asks nor gets a pass");
    let ask = runs.node_run(run.id, "send").unwrap().unwrap();
    assert!(matches!(ask.wait, Some(WaitRequest::Approval { implicit: true, .. })));

    runner.respond("local", run.id, "send", Response { decision: Some("approved".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(approved_at(&fake, "browser_click"), vec![true], "the click was told a person approved it");
}

#[test]
fn an_approval_step_before_the_click_covers_it_and_rejecting_it_means_no_click() {
    let (fake, runner, runs, id) = click_rig("explicit", click_flow("always", true));
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    runner.respond("local", run.id, "ok", Response { decision: Some("rejected".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert!(fake.args_of("browser_click").is_empty(), "rejected: no click");

    let (fake, runner, runs2, id) = click_rig("explicit2", click_flow("always", true));
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    runner.respond("local", run.id, "ok", Response { decision: Some("approved".into()), ..Default::default() }).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs2.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(approved_at(&fake, "browser_click"), vec![true]);
    let _ = runs;
}

#[test]
fn with_approvals_switched_off_the_click_runs_and_a_dry_run_never_touches_the_browser() {
    let (fake, runner, runs, id) = click_rig("never", click_flow("never", false));
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(approved_at(&fake, "browser_click"), vec![true], "the person switched asking off for this flow");

    let (fake, runner, runs, id) = click_rig("dry", click_flow("always", false));
    let run = runner.start("local", id, Map::new(), Value::Null, true, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", run.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert!(fake.tools().is_empty(), "a dry run does not open or click anything: {:?}", fake.tools());
}

#[test]
fn a_click_that_was_approved_is_never_done_twice_after_a_crash() {
    let (fake, runner, runs, id) = click_rig("ledger", click_flow("never", false));
    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(fake.args_of("browser_click").len(), 1);
    // The run is re-driven from the start (as after a restart): the recorded click is reused, not repeated.
    runs.set_status(run.id, RunStatus::Interrupted, "restart", 1_800_000_100).unwrap();
    let mut n = runs.node_run(run.id, "send").unwrap().unwrap();
    n.status = NodeStatus::Pending;
    runs.save_node(&n).unwrap();
    runner.resume("local", run.id).unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(fake.args_of("browser_click").len(), 1, "the click is in the ledger as done, so it was not sent again");
}

// ---- recovering from a page that changed -----------------------------------------------------------------------------------------------

#[test]
fn when_a_button_was_renamed_the_only_matching_element_is_offered_and_needs_approval_then_the_flow_works_again() {
    use super::supervisor::recovery::{self, USE_SIMILAR_ELEMENT};
    let dir = std::env::temp_dir().join(format!("aqua_browser_heal_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("flows.db");
    let (flows, runs) = (Arc::new(FlowStore::new(&path)), Arc::new(RunStore::new(&path)));
    let fake = FakeBrowser::new(&[("textbox", "Name"), ("button", "Submit message")]);
    let runner = FlowRunner::with_clock(flows.clone(), runs.clone(), Arc::new(Executors::new().with(BrowserNodeExecutor::new(fake.clone()))), Arc::new(|_, c| !c.load(Ordering::SeqCst)), Arc::new(|| 1_800_000_000));
    let id = flows.create("local", click_flow("never", false), "user", "", 1_800_000_000).unwrap().id;

    let run = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    let failed = runs.get_run("local", run.id).unwrap().unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    let err = runs.node_run(run.id, "send").unwrap().unwrap().error;
    let node = failed.def.node("send").unwrap();
    let offers = recovery::suggest_with_error(&runs, ErrorClass::WebsiteUiChange, node, &err);
    let fix = offers.iter().find(|s| s.key == USE_SIMILAR_ELEMENT).expect("the renamed button is offered");
    assert!(fix.title.contains("button:Submit message") && fix.needs_approval && fix.changes_logic, "{fix:?}");
    assert!(fix.why.contains("only button left"), "{}", fix.why);
    // Without the error text there is nothing to go on, so nothing is offered.
    assert!(recovery::suggest(&runs, ErrorClass::WebsiteUiChange, node).iter().all(|s| s.key != USE_SIMILAR_ELEMENT));
    assert_eq!(flows.get("local", id).unwrap().unwrap().version, 1, "nothing changed yet");

    let (flow, _) = recovery::apply(&flows, &runs, "local", &failed, "send", USE_SIMILAR_ELEMENT, 1_800_000_050).unwrap();
    assert_eq!(flow.def.node("send").unwrap().config["target"], "button:Submit message");
    let again = runner.start("local", id, Map::new(), Value::Null, false, "manual").unwrap();
    assert!(runner.wait_idle(Duration::from_secs(15)));
    assert_eq!(runs.get_run("local", again.id).unwrap().unwrap().status, RunStatus::Succeeded);
    assert_eq!(fake.args_of("browser_click")[0]["element"], "Submit message");

    // Two candidates: no guess.
    let two = FakeBrowser::new(&[("button", "Submit message"), ("button", "Send later")]);
    let e = run_err(&two, "button:Send now");
    assert!(recovery::suggest_with_error(&runs, ErrorClass::WebsiteUiChange, &node_with_target("button:Send now"), &e).iter().all(|s| s.key != USE_SIMILAR_ELEMENT), "{e}");
}

fn node_with_target(target: &str) -> Node {
    node(NodeKind::Browser, json!({ "action": "click", "target": target }))
}

fn run_err(fake: &Arc<FakeBrowser>, target: &str) -> String {
    run(fake, NodeKind::Browser, json!({ "action": "click", "target": target })).unwrap_err().message().to_string()
}
