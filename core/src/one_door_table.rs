//! Card R6-08, T6-08.5: the golden decision table of the one door.
//!
//! For a browser or desktop call (the model's name for the tool, its arguments, who asks, what the run has read and holds, the settings, the window in front) two decisions
//! are worked out with the code as it is, and set side by side on the ladder `auto < ask < always_ask < deny`:
//!
//! * the **old route**'s, by the very functions `call_named_tool` calls: the voice gate (`voice_may_use_browser_tool`), the rule that a run that read outside text asks
//!   (`taint_asks`), the browser's site policy, and the desktop guard with its tiers, its sign-in and payment steps and its one-session lease;
//! * the **tool host**'s, by the provider's own classification (`BrowserProvider::classify_with`, `WindowsUiProvider::classify_with`) and the policy engine, and what the one
//!   door would apply (`PolicyEngine::decide_door`).
//!
//! The door may be switched on only if the host is never looser than the old route. The table (`core/tests/fixtures/one_door/table.json`) records every row and the sweeps cover
//! every combination. Round 1 found the host looser on five desktop rows (a sign-in or payment step, one session at a time); round 2 moved those rules into the provider
//! (`WindowsUiProvider::classify_with` takes the step and the lease), so no row and no combination is looser now, and a test fails the moment one is.
//!
//! What both routes share is left out: the browser's backend asks the same questions on both (a site nobody listed, an upload, a click on "Send"), because the host's provider
//! calls the same backend. Its decisions about the site and about script are in the table as a floor under both sides.

use crate::agent_runs::scope::RunScope;
use crate::agent_tools::with_host;
use crate::door_shadow::{with_mode, DoorMode};
use crate::modules::browser_agent::{ask_before_acting_when_tainted, call_named_tool, taint_asks, voice_may_use_browser_tool, with_windows_caller};
use crate::test_support::{BrokerAnswerer, ScriptedApprover, TempWorld};
use aqua_mcp_adapters::browser::provider::host_name_for_wire as browser_host_name;
use aqua_mcp_adapters::browser::confirm::questions_put_on_this_thread;
use aqua_mcp_adapters::browser::{BrowserBackend, BrowserPolicyConfig, BrowserProvider, JavascriptPolicy, SiteMode, UrlDecision};
use aqua_mcp_adapters::windows_mcp::guard::{self, Foreground, Verdict, WINDOWS_UI_TOOLS};
use aqua_mcp_adapters::windows_mcp::provider::{host_name_for_wire as windows_host_name, Desktop};
use aqua_mcp_adapters::windows_mcp::{critical, session, tiers};
use aqua_mcp_adapters::{new_browser_client, new_windows_client, WindowsCaller, WindowsUiProvider};
use aqua_tool_host::provider::ClassifyContext;
use aqua_tool_host::{AuditLedger, Caller, Decision, Mode, PermClass, PolicyContext, PolicyEngine, RequestContext, ToolHost, ToolManifest, ToolProvider, ToolRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const TABLE: &str = include_str!("../tests/fixtures/one_door/table.json");

#[derive(Deserialize)]
struct Table {
    stricter_rows: Vec<String>,
    looser_rows: Vec<String>,
    rows: Vec<Row>,
}

#[derive(Deserialize, Clone)]
struct Row {
    name: String,
    /// The model's name for the tool: `browser_click`, `Click`.
    tool: String,
    #[serde(default)]
    args: Value,
    #[serde(default = "agent")]
    caller: String,
    /// The run has read outside text.
    #[serde(default)]
    tainted: bool,
    /// The run holds something private to the person.
    #[serde(default)]
    private: bool,
    #[serde(default)]
    full_access: bool,
    #[serde(default = "ask")]
    site_mode: String,
    /// The person (or, for the voice, a search of the run) named the address.
    #[serde(default)]
    named: bool,
    /// The window in front: [program, title].
    #[serde(default)]
    front: Option<(String, String)>,
    /// The page a script would run on, when the person allowed script on it.
    #[serde(default)]
    js_page: Option<String>,
    #[serde(default)]
    screen_text: String,
    /// Another session has the desktop.
    #[serde(default)]
    other_session: bool,
    #[serde(default)]
    old: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    door: String,
    #[serde(default)]
    relation: String,
    /// Why the row is what it is, for the reader of the table (read by nothing).
    #[serde(default)]
    #[allow(dead_code)]
    why: String,
}

fn agent() -> String {
    "agent".into()
}

fn ask() -> String {
    "ask".into()
}

fn word(d: Decision) -> &'static str {
    match d {
        Decision::Auto => "auto",
        Decision::Ask => "ask",
        Decision::AlwaysAsk => "always_ask",
        Decision::Deny => "deny",
    }
}

/// What each side decides for a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Verdicts {
    old: Decision,
    host: Decision,
    door: Decision,
}

impl Verdicts {
    fn relation(&self) -> &'static str {
        match self.host.cmp(&self.old) {
            std::cmp::Ordering::Equal => "same",
            std::cmp::Ordering::Greater => "stricter",
            std::cmp::Ordering::Less => "looser",
        }
    }
}

/// The providers' manifests, and a folder the browser backend keeps its files in.
struct Env {
    browser: BTreeMap<String, ToolManifest>,
    windows: BTreeMap<String, ToolManifest>,
    dir: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("aqua_door_table_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let backend = Arc::new(BrowserBackend::new(&dir.join("browser"), new_browser_client()));
        let browser = BrowserProvider::new(backend).manifests().into_iter().map(|m| (m.name.clone(), m)).collect();
        let windows = WindowsUiProvider::new(Arc::new(Mutex::new(new_windows_client()))).manifests().into_iter().map(|m| (m.name.clone(), m)).collect();
        Self { browser, windows, dir }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn host_caller(row: &Row) -> Caller {
    match row.caller.as_str() {
        "voice" => Caller::Voice,
        "api" => Caller::Api,
        _ => Caller::Agent,
    }
}

fn guard_caller(row: &Row) -> WindowsCaller {
    match row.caller.as_str() {
        "voice" => WindowsCaller::Voice,
        "api" => WindowsCaller::Api,
        _ => WindowsCaller::Agent,
    }
}

fn is_desktop(row: &Row) -> bool {
    WINDOWS_UI_TOOLS.contains(&row.tool.as_str())
}

/// What the person's settings say about the browser for a row.
fn browser_policy(row: &Row) -> BrowserPolicyConfig {
    let mut p = BrowserPolicyConfig::default();
    p.mode = match row.site_mode.as_str() {
        "denylist" => SiteMode::Denylist,
        "allowlist" => SiteMode::Allowlist,
        _ => SiteMode::Ask,
    };
    p.allow = vec!["docs.example.org".into()];
    p.deny.push("bad.example.net".into());
    if row.js_page.is_some() {
        p.javascript = JavascriptPolicy::AllowedSites;
        p.javascript_sites = vec!["docs.example.org".into()];
    }
    p
}

/// What the browser's backend decides on both routes: the site policy for an address that is opened, script that is switched off, and the question it always asks before an upload.
fn backend_floor(row: &Row, policy: &BrowserPolicyConfig) -> Decision {
    let action = row.args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let url = match row.tool.as_str() {
        "browser_navigate" => row.args.get("url").and_then(|v| v.as_str()),
        "browser_tabs" if action == "new" => row.args.get("url").and_then(|v| v.as_str()),
        _ => None,
    };
    let mut floor = Decision::Auto;
    if let Some(u) = url {
        floor = match policy.check_url_with(u, row.full_access) {
            UrlDecision::Allow => Decision::Auto,
            UrlDecision::Ask { .. } => Decision::Ask,
            UrlDecision::Deny(_) => Decision::Deny,
        };
    }
    if row.tool == "browser_evaluate" && !policy.javascript_allowed(row.js_page.as_deref()) {
        floor = Decision::Deny;
    }
    if row.tool == "browser_file_upload" {
        floor = floor.max(Decision::Ask);
    }
    floor
}

/// The host's decision and the door's for a call whose provider said `classified`: the classes of the manifest and of the call, the engine of the standard mode with the
/// row's full-access switch, and the contexts the host would build.
fn host_and_door(row: &Row, manifest: &ToolManifest, classified: Result<aqua_tool_host::Classification, aqua_tool_host::ToolError>, sends_out: bool) -> (Decision, Decision) {
    let Ok(class) = classified else { return (Decision::Deny, Decision::Deny) };
    let mut classes: Vec<PermClass> = manifest.classes.clone();
    for c in &class.classes {
        if !classes.contains(c) {
            classes.push(*c);
        }
    }
    classes.sort();
    let engine = PolicyEngine::new(Mode::Standard);
    engine.set_full_access(row.full_access);
    let applied = PolicyContext { caller: host_caller(row), tainted: row.tainted, private_data: false, target_allowed: class.target_allowed };
    let caps = PolicyContext { private_data: row.private, ..applied };
    (engine.decide(manifest, &classes, &applied).decision, engine.decide_door(manifest, &classes, &applied, &caps, sends_out).decision)
}

fn evaluate(env: &Env, row: &Row) -> Verdicts {
    if is_desktop(row) {
        evaluate_desktop(env, row)
    } else {
        evaluate_browser(env, row)
    }
}

fn evaluate_browser(env: &Env, row: &Row) -> Verdicts {
    let policy = browser_policy(row);
    // The old route, in the order `call_named_tool` takes it.
    let mut old = Decision::Auto;
    if row.caller == "voice" && !voice_may_use_browser_tool(&row.tool, &row.args) {
        old = Decision::Deny;
    }
    if taint_asks(row.tainted, "browser", &row.tool, &row.args, || Some(policy.clone()), row.full_access, || row.named) {
        old = old.max(Decision::AlwaysAsk);
    }
    let floor = backend_floor(row, &policy);
    old = old.max(floor);
    // The host.
    let name = browser_host_name(&row.tool).unwrap_or_else(|| panic!("{}: no host name for {}", row.name, row.tool));
    let manifest = env.browser.get(name).unwrap_or_else(|| panic!("{}: no manifest for {name}", row.name));
    let ctx = ClassifyContext { caller: host_caller(row), run: "run".into(), tainted: row.tainted, private: row.private };
    let classified = BrowserProvider::classify_with(name, &row.args, &policy, row.full_access, Some(&ctx));
    let sends = BrowserProvider::call_sends_something_out(name, &row.args, &policy, row.full_access);
    let (host, door) = host_and_door(row, manifest, classified, sends);
    Verdicts { old, host: host.max(floor), door: door.max(floor) }
}

fn evaluate_desktop(env: &Env, row: &Row) -> Verdicts {
    let fg = row.front.as_ref().map(|(exe, title)| Foreground { exe: exe.clone(), title: title.clone(), window: 1 });
    let caller = guard_caller(row);
    // What the guard knows besides the call, and the provider is told: a sign-in or payment step in front (by the window's title or by what the last snapshot of it showed), and
    // whether another session has the desktop (what the lease says to this one).
    let step = fg.as_ref().and_then(|f| critical::critical_for(f, Some(&critical::View::of(f, &row.screen_text, 0)), 0));
    let busy = row.other_session.then(|| {
        let desk = session::Desk::new();
        desk.claim_at("another-run", Instant::now()).unwrap();
        desk.check_at("this-run", Instant::now()).unwrap_err()
    });
    // The old route: the question for a run that read outside text, then the guard (its rules, the tier of the window, a sign-in or payment step, the one-session lease).
    let mut old = Decision::Auto;
    if taint_asks(row.tainted, "windows", &row.tool, &row.args, || None, row.full_access, || false) {
        old = old.max(Decision::AlwaysAsk);
    }
    let mut verdict = guard::evaluate_tiered(&row.tool, &row.args, caller, fg.as_ref(), row.full_access, tiers::Policy::Strict);
    if fg.is_some() {
        verdict = critical::critical_verdict(verdict, &row.tool, &row.args, caller, fg.as_ref(), step.as_ref());
    }
    let refused = matches!(verdict, Verdict::Deny(..));
    if !refused && !guard::is_observe_call(&row.tool, &row.args) && busy.is_some() {
        old = old.max(Decision::Deny);
    }
    old = old.max(match verdict {
        Verdict::Allow => Decision::Auto,
        Verdict::Ask(_) => Decision::Ask,
        Verdict::Deny(..) => Decision::Deny,
    });
    // The host.
    let name = windows_host_name(&row.tool).unwrap_or_else(|| panic!("{}: no host name for {}", row.name, row.tool));
    let manifest = env.windows.get(name).unwrap_or_else(|| panic!("{}: no manifest for {name}", row.name));
    let desktop = Desktop { front: fg.as_ref(), full_access: row.full_access, tiers: tiers::Policy::Strict, step: step.as_ref(), busy: busy.as_deref() };
    let classified = WindowsUiProvider::classify_with(name, &row.args, caller, &desktop);
    let (host, door) = host_and_door(row, manifest, classified, WindowsUiProvider::call_sends_something_out(name, &row.args));
    Verdicts { old, host, door }
}

fn table() -> Table {
    serde_json::from_str(TABLE).expect("core/tests/fixtures/one_door/table.json is not valid")
}

#[test]
fn every_row_of_the_table_is_what_the_code_decides_now() {
    let env = Env::new("rows");
    let t = table();
    let mut names = std::collections::BTreeSet::new();
    let mut wrong = Vec::new();
    for row in &t.rows {
        assert!(names.insert(row.name.clone()), "two rows are called \"{}\"", row.name);
        let got = evaluate(&env, row);
        let recorded = (row.old.as_str(), row.host.as_str(), row.door.as_str(), row.relation.as_str());
        let now = (word(got.old), word(got.host), word(got.door), got.relation());
        if recorded != now {
            wrong.push(format!("{}: the table says old/host/door/relation {recorded:?}, the code says {now:?}", row.name));
        }
    }
    assert!(wrong.is_empty(), "{} of {} rows differ:\n{}", wrong.len(), t.rows.len(), wrong.join("\n"));
    assert!(t.rows.len() >= 75, "the table has {} rows", t.rows.len());
}

#[test]
fn the_rows_where_the_host_is_stricter_are_listed_by_name_and_no_row_is_one_where_it_is_looser() {
    let env = Env::new("lists");
    let t = table();
    let mut stricter: Vec<String> = Vec::new();
    for row in &t.rows {
        match evaluate(&env, row).relation() {
            "stricter" => stricter.push(row.name.clone()),
            // A row where the host is looser than the old route is a hole the door would open: the switch is not safe while there is one.
            "looser" => panic!("{}: the host is looser than the old route here ({:?})", row.name, evaluate(&env, row)),
            _ => {}
        }
    }
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(sorted(stricter), sorted(t.stricter_rows.clone()), "the stricter rows listed in the table are not the rows that are");
    assert!(t.looser_rows.is_empty(), "the table lists rows where the host is looser than the old route: {:?}", t.looser_rows);
}

#[test]
fn the_door_is_never_looser_than_the_host_or_than_the_old_route() {
    let env = Env::new("door");
    for row in &table().rows {
        let got = evaluate(&env, row);
        assert!(got.door >= got.host, "{}: the door ({:?}) is looser than the host ({:?})", row.name, got.door, got.host);
        assert!(got.door >= got.old, "{}: the door ({:?}) is looser than the old route ({:?})", row.name, got.door, got.old);
    }
}

fn browser_row(tool: &str, args: Value) -> Row {
    Row { name: tool.into(), tool: tool.into(), args, caller: agent(), tainted: false, private: false, full_access: false, site_mode: ask(), named: false, front: None, js_page: None, screen_text: String::new(), other_session: false, old: String::new(), host: String::new(), door: String::new(), relation: String::new(), why: String::new() }
}

/// Every browser tool the host has a name for, with the arguments that make it different.
fn browser_calls() -> Vec<(&'static str, Value)> {
    let u = "https://collector.example.net/?d=my+notes";
    vec![
        ("browser_navigate", json!({"url": u})),
        ("browser_navigate", json!({"url": "https://docs.example.org/page?q=x"})),
        ("browser_navigate", json!({"url": "http://127.0.0.1:8080/"})),
        ("browser_navigate", json!({"url": "https://bad.example.net/"})),
        ("browser_navigate", json!({"url": "about:blank"})),
        ("browser_navigate_back", json!({})),
        ("browser_snapshot", json!({})),
        ("browser_find", json!({"text": "x"})),
        ("aqua_browser_extract", json!({"kind": "links"})),
        ("browser_take_screenshot", json!({})),
        ("browser_console_messages", json!({"level": "error"})),
        ("browser_wait_for", json!({"time": 1})),
        ("aqua_browser_session", json!({})),
        ("browser_tabs", json!({"action": "list"})),
        ("browser_tabs", json!({"action": "new", "url": u})),
        ("browser_tabs", json!({"action": "close", "index": 0})),
        ("browser_tabs", json!({"action": "select", "index": 0})),
        ("browser_tabs", json!({})),
        ("browser_click", json!({"target": "e1"})),
        ("browser_hover", json!({"target": "e1"})),
        ("browser_type", json!({"target": "e1", "text": "x"})),
        ("browser_type", json!({"target": "e1", "text": "x", "submit": true})),
        ("browser_fill_form", json!({"fields": [{"name": "a", "type": "textbox", "ref": "e1", "value": "x"}]})),
        ("browser_select_option", json!({"target": "e1", "values": ["a"]})),
        ("browser_press_key", json!({"key": "Enter"})),
        ("browser_press_key", json!({"key": "Tab"})),
        ("browser_handle_dialog", json!({"accept": true})),
        ("browser_close", json!({})),
        ("browser_evaluate", json!({"function": "() => document.title"})),
        ("browser_file_upload", json!({"paths": ["a.txt"]})),
    ]
}

#[test]
fn across_every_browser_tool_caller_and_setting_the_host_is_never_looser_than_the_old_route_and_the_door_is_never_looser_than_the_host() {
    let env = Env::new("browser_sweep");
    let mut compared = 0;
    let mut looser = Vec::new();
    for (tool, args) in browser_calls() {
        for caller in ["agent", "voice", "api"] {
            for tainted in [false, true] {
                for private in [false, true] {
                    for full_access in [false, true] {
                        for site_mode in ["ask", "denylist", "allowlist"] {
                            for named in [false, true] {
                                for js_page in [None, Some("https://docs.example.org/x".to_string())] {
                                    let row = Row { caller: caller.into(), tainted, private, full_access, site_mode: site_mode.into(), named, js_page, ..browser_row(tool, args.clone()) };
                                    let got = evaluate(&env, &row);
                                    compared += 1;
                                    if got.host < got.old || got.door < got.host {
                                        looser.push(format!("{tool} {args} caller={caller} tainted={tainted} private={private} full={full_access} mode={site_mode} named={named} js={}: old {:?} host {:?} door {:?}", row.js_page.is_some(), got.old, got.host, got.door));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(compared > 8_000, "{compared} browser combinations were compared");
    assert!(looser.is_empty(), "{} of {compared} browser combinations are looser through the host than through the old route:\n{}", looser.len(), looser.join("\n"));
}

fn desktop_row(tool: &str, args: Value) -> Row {
    Row { tool: tool.into(), ..browser_row(tool, args) }
}

fn desktop_calls() -> Vec<(&'static str, Value)> {
    vec![
        ("App", json!({"mode": "launch", "name": "Notepad"})),
        ("App", json!({"mode": "switch", "name": "Notepad"})),
        ("App", json!({"mode": "launch", "name": "PowerShell"})),
        ("Snapshot", json!({})),
        ("Screenshot", json!({})),
        ("DisplayInventory", json!({})),
        ("Click", json!({"loc": [10, 10]})),
        ("Click", json!({"loc": [10, 10], "button": "right"})),
        ("Type", json!({"text": "hi"})),
        ("Type", json!({"text": "hi", "press_enter": true})),
        ("Type", json!({"text": "hi\n"})),
        ("Scroll", json!({"direction": "down"})),
        ("Move", json!({"loc": [1, 1]})),
        ("Move", json!({"loc": [1, 1], "drag": true, "from_loc": [2, 2]})),
        ("Shortcut", json!({"shortcut": "ctrl+s"})),
        ("Shortcut", json!({"shortcut": "ctrl+enter"})),
        ("Shortcut", json!({"shortcut": "win+r"})),
        ("Wait", json!({"duration": 1})),
        ("WaitFor", json!({"text": "x", "timeout": 1})),
        ("MultiSelect", json!({"locs": [[1, 1]]})),
        ("MultiEdit", json!({"locs": [[1, 1]], "text": "x"})),
        ("Notification", json!({"title": "t", "message": "m"})),
    ]
}

#[test]
fn across_every_desktop_tool_caller_window_and_setting_the_host_is_never_looser_than_the_old_route_and_the_door_never_looser_than_the_host() {
    let env = Env::new("desktop_sweep");
    let windows: [Option<(&str, &str)>; 8] = [
        Some(("notepad.exe", "Untitled - Notepad")),
        Some(("slack.exe", "Slack - team")),
        Some(("chrome.exe", "Docs - Google Chrome")),
        Some(("powershell.exe", "Windows PowerShell")),
        Some(("aqua-desktop.exe", "Aqua Assistant")),
        Some(("bank.exe", "Sign in to your account")),
        Some(("shop.exe", "Checkout - Shop")),
        None,
    ];
    // What the last snapshot of the window showed: nothing, or a payment form under a title that gives nothing away.
    let screens = ["", "Card number CVV Pay now"];
    let (mut compared, mut unexplained) = (0, Vec::new());
    for (tool, args) in desktop_calls() {
        for caller in ["agent", "voice", "api"] {
            for tainted in [false, true] {
                for private in [false, true] {
                    for full_access in [false, true] {
                        for front in windows {
                            for other_session in [false, true] {
                                for screen_text in screens {
                                    let row = Row {
                                        caller: caller.into(),
                                        tainted,
                                        private,
                                        full_access,
                                        front: front.map(|(e, t)| (e.to_string(), t.to_string())),
                                        other_session,
                                        screen_text: screen_text.into(),
                                        ..desktop_row(tool, args.clone())
                                    };
                                    let got = evaluate(&env, &row);
                                    compared += 1;
                                    if got.door < got.host {
                                        unexplained.push(format!("{tool} {args}: the door ({:?}) is looser than the host ({:?}) for {}", got.door, got.host, describe(&row)));
                                    }
                                    if got.host < got.old {
                                        unexplained.push(format!("{tool} {args}: old {:?}, host {:?} for {}", got.old, got.host, describe(&row)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(compared > 16_000, "{compared} desktop combinations were compared");
    assert!(unexplained.is_empty(), "{} of {compared} desktop combinations are looser through the host than through the old route:
{}", unexplained.len(), unexplained.join("
"));
}

/// The table's `door` column is what the one door would apply, worked out by the policy engine from the providers' classifications. With the door on (`RequestContext::door`) the
/// real host applies exactly that: every row is asked of a real host with the browser's and the desktop's providers, a run that has read and holds what the row says, and the
/// window in front that the row names, and the decision it gives is the table's `door` column (with the backend's own floor, which both routes share).
#[test]
fn every_row_of_the_table_is_what_the_real_host_applies_with_the_door_on() {
    let env = Env::new("host_rows");
    let t = table();
    let backend = Arc::new(BrowserBackend::new(&env.dir.join("host_browser"), new_browser_client()));
    let front: Arc<Mutex<Option<Foreground>>> = Arc::new(Mutex::new(None));
    let build = |full_access: bool| {
        let engine = Arc::new(PolicyEngine::new(Mode::Standard));
        engine.set_full_access(full_access);
        let host = ToolHost::new(engine, Arc::new(AuditLedger::open(env.dir.join(format!("audit_{full_access}.jsonl")))));
        host.register(Arc::new(BrowserProvider::new(backend.clone()))).unwrap();
        let reader = front.clone();
        host.register(Arc::new(WindowsUiProvider::isolated(Arc::new(Mutex::new(new_windows_client())), Arc::new(move || reader.lock().unwrap().clone()), full_access, tiers::Policy::Strict)))
            .unwrap();
        host
    };
    let (plain, full) = (build(false), build(true));
    let (mut compared, mut wrong) = (0, Vec::new());
    for (i, row) in t.rows.iter().enumerate() {
        // The browser's provider reads the process's full-access switch, which a test must not set: its rows with full access are covered by the sweep above. The desktop's lease
        // and the last snapshot of a window belong to the desktop of the provider, and are covered by its own tests and by the sweep.
        if (!is_desktop(row) && row.full_access) || row.other_session || !row.screen_text.is_empty() {
            continue;
        }
        let host = if row.full_access { &full } else { &plain };
        let run = format!("row_{i}");
        if row.tainted {
            host.mark_tainted(&run);
        }
        if row.private {
            host.mark_private(&run);
        }
        let (name, floor) = if is_desktop(row) {
            *front.lock().unwrap() = row.front.as_ref().map(|(exe, title)| Foreground { exe: exe.clone(), title: title.clone(), window: 1 });
            (windows_host_name(&row.tool), Decision::Auto)
        } else {
            let policy = browser_policy(row);
            backend.set_policy(policy.clone()).unwrap();
            (browser_host_name(&row.tool), backend_floor(row, &policy))
        };
        let name = name.unwrap_or_else(|| panic!("{}: no host name for {}", row.name, row.tool));
        let mut context = RequestContext::new(host_caller(row));
        context.agent_run_id = run;
        context.door = true;
        let applied = match host.preview(&ToolRequest { tool: name.to_string(), arguments: row.args.clone(), context }) {
            Ok(p) => p.decision,
            Err(_) => Decision::Deny,
        };
        let got = evaluate(&env, row);
        compared += 1;
        if applied.max(floor) != got.door {
            wrong.push(format!("{}: the table's door column says {:?}, the host with the door on applies {:?} (with the backend's floor {:?})", row.name, got.door, applied, floor));
        }
    }
    assert!(compared >= 60, "{compared} rows were asked of the real host");
    assert!(wrong.is_empty(), "{} of {compared} rows differ:
{}", wrong.len(), wrong.join("
"));
}

fn describe(row: &Row) -> String {
    format!("caller={} tainted={} private={} full={} front={:?} screen={:?} other_session={}", row.caller, row.tainted, row.private, row.full_access, row.front, row.screen_text, row.other_session)
}

// ---------------------------------------------------------------------------------------------
// The order in which the old route decides (card OA3-01, AQ-04). The table above assumes it, and a change of the order in `call_named_tool` is a change of the old route: the table
// must then be read again. This used to be a test that searched the source of `call_named_tool` for the calls and compared where they stood; the function is run now, with a host,
// a browser that is never started, a run that has read outside text, and a thread that answers the old route's questions "no" and keeps them.

/// The old route's world: a host with the browser's and the desktop's providers, a scripted approver that says no, and the browser the old route asks its questions of (the very
/// one the host's provider drives).
struct OldRoute {
    _world: TempWorld,
    host: Arc<ToolHost>,
    backend: Arc<BrowserBackend>,
    approver: Arc<ScriptedApprover>,
}

fn old_route(name: &str) -> OldRoute {
    let world = TempWorld::new(&format!("door_table_{name}"));
    let backend = Arc::new(BrowserBackend::new(&world.root.join("browser"), new_browser_client()));
    let mut policy = BrowserPolicyConfig::default();
    policy.mode = SiteMode::Ask;
    policy.allow = vec!["docs.example.org".into()];
    backend.set_policy(policy).unwrap();
    let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(&world.root.join("host.audit.jsonl")))));
    host.register(Arc::new(BrowserProvider::new(backend.clone()))).unwrap();
    let front = Arc::new(Mutex::new(Some(Foreground { exe: "notepad.exe".into(), title: "Untitled - Notepad".into(), window: 1 })));
    host.register(Arc::new(WindowsUiProvider::isolated(Arc::new(Mutex::new(new_windows_client())), Arc::new(move || front.lock().unwrap().clone()), false, tiers::Policy::Strict))).unwrap();
    let approver = Arc::new(ScriptedApprover::denying());
    host.set_approver(approver.clone());
    OldRoute { _world: world, host, backend, approver }
}

/// A run on this thread that has read outside text, and the limits and the host of its own: `f` is the agent's loop for as long as it is.
fn a_run_that_has_read_outside_text<R>(o: &OldRoute, f: impl FnOnce() -> R) -> R {
    let counters = Arc::new(crate::autonomy::Autonomy::new(crate::autonomy::Settings::default()));
    crate::autonomy::with_autonomy(counters, || {
        with_host(o.host.clone(), || {
            let scope = RunScope::begin(Some(o.host.clone()));
            o.host.mark_tainted(scope.id());
            f()
        })
    })
}

#[test]
fn the_old_routes_browser_branch_takes_the_voice_gate_then_the_door_then_the_question_and_only_then_the_backend() {
    let o = old_route("browser_order");
    let browser = Some(o.backend.clone());
    let click = json!({"target": "e1"});
    let answerer = BrokerAnswerer::denying(o.backend.clone());
    let questions = a_run_that_has_read_outside_text(&o, || {
        let before = questions_put_on_this_thread();
        // The voice gate comes first: a spoken request may not click, whatever the run has read, and nobody is asked about it.
        let (label, text) = with_windows_caller(WindowsCaller::Voice, || call_named_tool("browser_click", click.clone(), &[], &browser, &None).unwrap());
        assert_eq!(label, "guard");
        assert!(text.starts_with("ERROR [POLICY_TOOL_BLOCKED] browser_click: a voice request can read pages but not click"), "{text}");
        assert_eq!(questions_put_on_this_thread(), before, "refused by the voice gate before any question was put");
        assert_eq!(o.approver.asked.lock().unwrap().len(), 0);
        // Then the door (`AQUA_DOOR=on`): the host decides, and asks (the scripted approver says no); the old route's own question is not put as well.
        let (label, text) = with_mode(DoorMode::On, || call_named_tool("browser_click", click.clone(), &[], &browser, &None).unwrap());
        assert_eq!(label, "browser");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] browser.click:"), "{text}");
        assert_eq!(o.approver.asked.lock().unwrap().len(), 1, "the host put its question");
        assert_eq!(questions_put_on_this_thread(), before, "and the old route put none");
        // With the door off, the old route's question comes before the backend is reached: the answer is no, and the call never goes on (the backend here is never started, so a call
        // that reached it would have said so in other words).
        let (label, text) = with_mode(DoorMode::Shadow, || call_named_tool("browser_click", click.clone(), &[], &browser, &None).unwrap());
        assert_eq!(label, "guard");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] browser_click: you did not approve:"), "{text}");
        assert_eq!(questions_put_on_this_thread(), before + 1, "the old route put its question");
        assert_eq!(o.approver.asked.lock().unwrap().len(), 1, "and the host was not asked again");
        questions_put_on_this_thread() - before
    });
    let kept = answerer.finish();
    assert_eq!((questions, kept.len()), (1, 1), "{kept:?}");
    assert_eq!((kept[0].tool.as_str(), kept[0].always_ask, kept[0].kind), ("browser_click", true, aqua_mcp_adapters::browser::ConfirmKind::Action));
}

#[test]
fn the_old_routes_desktop_branch_takes_the_door_then_the_question_and_only_then_the_guard() {
    let o = old_route("desktop_order");
    let windows = Some(Arc::new(Mutex::new(new_windows_client())));
    let click = json!({"loc": [10, 10]});
    let answerer = BrokerAnswerer::denying(o.backend.clone());
    a_run_that_has_read_outside_text(&o, || {
        let before = questions_put_on_this_thread();
        // The door: the host decides and asks; the old route's question is not put as well.
        let (label, text) = with_mode(DoorMode::On, || call_named_tool("Click", click.clone(), &[], &Some(o.backend.clone()), &windows).unwrap());
        assert_eq!(label, "windows");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] win.click:"), "{text}");
        assert_eq!((o.approver.asked.lock().unwrap().len(), questions_put_on_this_thread()), (1, before));
        // With the door off the old route asks first, and a no never reaches the desktop guard (which has its own rules, its own audit file and a desktop of its own).
        let (label, text) = with_mode(DoorMode::Shadow, || call_named_tool("Click", click.clone(), &[], &Some(o.backend.clone()), &windows).unwrap());
        assert_eq!(label, "guard");
        assert!(text.starts_with("ERROR [CONFIRMATION_DENIED] Click: you did not approve:"), "{text}");
        assert_eq!((o.approver.asked.lock().unwrap().len(), questions_put_on_this_thread()), (1, before + 1));
    });
    assert_eq!(answerer.finish().len(), 1);
}

/// The question is the table's rule and nothing else: for every browser and desktop call of the sweeps and every state of the run, the old route puts its question exactly when
/// `taint_asks` (the function the table calls) says it must, and a call it does not ask about is not refused by it.
#[test]
fn the_old_routes_question_is_put_exactly_when_the_taint_rule_the_table_calls_says_so() {
    let o = old_route("taint_rule");
    let answerer = BrokerAnswerer::denying(o.backend.clone());
    let browser = Some(o.backend.clone());
    let named = |args: &Value| crate::sources::may_read(&crate::agent_runs::scope::current_run(), args.get("url").and_then(|v| v.as_str()).unwrap_or(""));
    let (mut asked, mut not_asked) = (0, 0);
    a_run_that_has_read_outside_text(&o, || {
        for mode in [SiteMode::Ask, SiteMode::Denylist, SiteMode::Allowlist] {
            let mut policy = o.backend.policy();
            policy.mode = mode;
            o.backend.set_policy(policy.clone()).unwrap();
            let calls = browser_calls().into_iter().map(|(t, a)| ("browser", t, a)).chain(desktop_calls().into_iter().map(|(t, a)| ("windows", t, a)));
            for (source, tool, args) in calls {
                for tainted in [false, true] {
                    let must = taint_asks(tainted, source, tool, &args, || (source == "browser").then(|| policy.clone()), aqua_mcp_adapters::full_access::is_on(), || named(&args));
                    let before = questions_put_on_this_thread();
                    let got = ask_before_acting_when_tainted(&browser, tainted, source, tool, &args);
                    let put = questions_put_on_this_thread() - before;
                    assert_eq!((put, got.is_some()), (u64::from(must), must), "{source} {tool} {args} tainted={tainted} mode={mode:?}");
                    if must {
                        asked += 1;
                    } else {
                        not_asked += 1;
                    }
                }
            }
        }
    });
    assert!(asked > 30 && not_asked > 100, "the sweep asks about {asked} calls and about {not_asked} it lets by");
    assert_eq!(answerer.finish().len(), asked);
}
